//! The one-pane local smoke path.
//!
//! This is the M0 end of the roadmap with M1's session task underneath it: a
//! single PTY, a single grid, and the renderer, wired together **in-process**.
//! There is no socket, no daemon, and no detach — the child dies with the
//! client, and that is the whole point of the milestone.
//!
//! The three-way split is already the real one, though, and nothing here
//! crosses it: `cloo-server` owns the PTY and the authoritative grid,
//! `cloo-client` owns raw mode and every escape sequence, and this module only
//! moves snapshots one way and commands the other. In particular it holds a
//! [`SessionHandle`] rather than a reactor, so the local path mutates session
//! state through exactly the same `mpsc<Command>` the daemon does — one
//! serialized owner, no second path, no `Mutex`.
//!
//! Three ordering rules keep the terminal safe and honest. Raw mode is entered
//! *before* the child is spawned, so a failure that is going to happen happens
//! while the terminal is still untouched; the render is driven by a frame timer
//! rather than by PTY readiness, so a fast producer coalesces into at most one
//! frame per tick instead of one per read; and a `SIGWINCH` becomes a resize
//! *command*, so the grid reflow and the child's `TIOCSWINSZ` happen in one
//! place in one order.

use std::fmt;
use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::time::Duration;

use cloo_client::capabilities::detect_caps;
use cloo_client::effects::{EffectPolicy, apply_effect};
use cloo_client::input::{
    ChromeAction, ChromeMouse, InputDecoder, InputEvent, MouseRoute, OuterModes, ScreenLayout,
    route_mouse,
};
use cloo_client::outer::current_size;
use cloo_client::raw_mode::{RawMode, RawModeError};
use cloo_client::renderer::{Cursor, Grid, RenderError, Renderer};
use cloo_client::resize::ResizeWatch;
use cloo_proto::{PaneId, PaneModes};
use cloo_server::launch::Launch;
use cloo_server::pty::{PtyConfig, PtyError};
use cloo_server::session::{Session, SessionEvent, SessionGone, SessionHandle, SessionSnapshot};

/// The render tick, capping the frame rate at roughly 60fps.
///
/// A large `cat` is the classic multiplexer killer: without a cap, every PTY
/// read becomes a full-screen repaint and the renderer, not the child, becomes
/// the bottleneck.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Size of a single stdin read on the input thread.
const INPUT_BUF_LEN: usize = 1024;

/// The one pane a local session has.
const THE_PANE: PaneId = PaneId::new(1);

/// Everything the local session can refuse to do.
#[derive(Debug)]
pub enum LocalError {
    /// The outer terminal could not be put into raw mode.
    RawMode(RawModeError),
    /// The PTY or its child failed.
    Pty(PtyError),
    /// The server and the client disagreed about geometry.
    Render(RenderError),
    /// A frame could not be written to the terminal.
    Output(io::Error),
    /// The Tokio runtime could not be built.
    Runtime(io::Error),
    /// A `SIGWINCH` handler could not be installed.
    Signal(io::Error),
    /// The session task ended before the loop was done with it.
    Session(SessionGone),
}

impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawMode(RawModeError::NotATerminal) => {
                write!(f, "cloo must be run from a terminal")
            }
            Self::RawMode(e) => write!(f, "{e}"),
            Self::Pty(e) => write!(f, "{e}"),
            Self::Render(e) => write!(f, "render failed: {e}"),
            Self::Output(e) => write!(f, "could not write to the terminal: {e}"),
            Self::Runtime(e) => write!(f, "could not start the runtime: {e}"),
            Self::Signal(e) => write!(f, "could not watch for terminal resizes: {e}"),
            Self::Session(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LocalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RawMode(e) => Some(e),
            Self::Output(e) | Self::Runtime(e) | Self::Signal(e) => Some(e),
            Self::Pty(e) => Some(e),
            Self::Render(e) => Some(e),
            Self::Session(e) => Some(e),
        }
    }
}

impl From<SessionGone> for LocalError {
    fn from(value: SessionGone) -> Self {
        Self::Session(value)
    }
}

impl From<PtyError> for LocalError {
    fn from(value: PtyError) -> Self {
        Self::Pty(value)
    }
}

impl From<RenderError> for LocalError {
    fn from(value: RenderError) -> Self {
        Self::Render(value)
    }
}

/// Runs `launch` in a single pane until its child exits, and reports the status.
///
/// The launch was validated before it got here — profile, name, task label, and
/// directory — so the only failure left is the spawn itself, which is where a
/// program that is not on `PATH` surfaces.
///
/// The outer terminal is restored on every exit path — normal, error, panic,
/// and signal — by the guard taken here; see `cloo-client`'s `raw_mode`.
///
/// # Errors
///
/// Returns a [`LocalError`] if the terminal could not be prepared, the child
/// could not be spawned, or a frame could not be drawn.
pub fn run(launch: Launch) -> Result<ExitStatus, LocalError> {
    // First, and before the child exists: "cloo must be run from a terminal" is
    // the error a misuse should produce, and a failure here leaves nothing to
    // clean up.
    let raw = RawMode::stdin().map_err(LocalError::RawMode)?;
    let size = current_size();

    // `detect_caps`, not `detect_attach_caps`: a local pane negotiates with
    // nobody, so an unresolvable `TERM` claims nothing here rather than refusing
    // the way an attach does (DECISIONS.md RESOLVED-12).
    let caps = detect_caps();
    let modes = OuterModes::negotiated(caps);
    // Registered before the modes are turned on, so a panic between the two
    // still resets a mode that did make it out.
    raw.on_restore(&modes.disable())
        .map_err(LocalError::RawMode)?;
    enable_modes(modes)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(LocalError::Runtime)?;

    let result = runtime.block_on(session(launch, size, caps, modes));

    // Restore before anything is printed: an error message rendered into a raw
    // terminal comes out as a staircase.
    let restored = raw.restore().map_err(LocalError::RawMode);
    let status = result?;
    restored?;
    Ok(status)
}

/// The async body of [`run`], with the terminal already raw.
async fn session(
    launch: Launch,
    size: cloo_proto::Size,
    caps: cloo_proto::TermCaps,
    modes: OuterModes,
) -> Result<ExitStatus, LocalError> {
    // The base carries what belongs to the session rather than to the profile:
    // the geometry, and the environment a pane inherits. The launch supplies the
    // argv and the working directory.
    let base = PtyConfig::session(size)?;

    // Installed before the session exists so a `SIGWINCH` between spawning the
    // child and entering the loop is not the one resize that gets lost.
    let mut resizes = ResizeWatch::new(size).map_err(LocalError::Signal)?;

    let spawned = Session::spawn(&base, THE_PANE, launch)?;
    let mut events = spawned.events;
    // Held for the loop's whole life. Dropping it is what lets the session task
    // finish, so it happens exactly once, below.
    let session = Some(spawned.handle);

    let mut grid = Grid::new(size);
    let mut renderer = Renderer::new(caps);
    let mut out = io::stdout();
    // No preference surface exists yet, so the local pane begins deny-all.
    // The policy is still applied here, matching the attached-client path.
    let effect_policy = EffectPolicy::default();
    // The first picture and every geometry change must clear stale outer
    // terminal cells. Ordinary snapshots below then repaint only rows whose
    // server contents actually changed.
    let mut needs_full_render = true;

    // What the child has negotiated, as of the last frame drawn. At most one
    // frame stale, which is what routing a mouse event costs instead of a
    // round trip to the session task per click.
    let mut pane_modes = PaneModes::default();
    // What the client drew, which is what a mouse report is hit-tested against.
    // One pane, no chrome: the local path composes no tab row, no status bar,
    // and no header. It is rebuilt on a resize rather than adjusted, because a
    // stale screen would place a click in a pane that has moved.
    let mut screen = ScreenLayout::single(size, THE_PANE);
    // The gesture machine holds only a divider drag in flight, and this path has
    // no divider to drag. It is here so the wheel reaches the same commands the
    // copy-mode keys do rather than through a second path of its own.
    let mut chrome = ChromeMouse::new();
    // Which pane the server says is in copy mode, from the last frame drawn.
    // Copy mode is session state, so a client asks rather than assumes.
    let mut copy_mode = None;
    let mut decoder = InputDecoder::new(modes);
    let mut input = spawn_input_reader();
    let mut input_open = true;
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    // Missed ticks are frames nobody saw; there is no value in catching up.
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut dirty = true;
    loop {
        // Every branch is cancel-safe: each awaits readiness and only then
        // decides anything, so losing this race drops a wakeup, never a byte, a
        // command, or a resize.
        let step = tokio::select! {
            event = events.recv() => Step::Session(event),
            received = input.recv(), if input_open => match received {
                Some(bytes) => Step::Input(bytes),
                None => Step::InputClosed,
            },
            resized = resizes.changed() => Step::Resized(resized),
            _ = frames.tick() => Step::Frame,
        };

        match step {
            Step::Session(Some(SessionEvent::Output)) => dirty = true,
            Step::Session(Some(SessionEvent::Effect { effect, .. })) => {
                let _ = apply_effect(&mut out, caps, effect_policy, &effect)
                    .map_err(LocalError::Output)?;
            }
            Step::Session(Some(SessionEvent::Exited) | None) => break,
            Step::Input(bytes) => {
                for event in decoder.feed(&bytes) {
                    route(
                        handle(&session)?,
                        &screen,
                        &mut chrome,
                        pane_modes,
                        copy_mode,
                        event,
                    )
                    .await?;
                }
            }
            // The outer terminal changed shape. The grid reflow and the child's
            // `TIOCSWINSZ` are the session task's business, in that order.
            Step::Resized(size) => {
                screen = ScreenLayout::single(size, THE_PANE);
                handle(&session)?.resize(size).await?;
            }
            // Stdin reached end of file. The child keeps running; it simply
            // gets no more input.
            Step::InputClosed => input_open = false,
            Step::Frame => {
                // A held escape prefix is released within a frame, which is what
                // makes a lone Escape key reach the pane at all.
                if let Some(event) = decoder.flush() {
                    route(
                        handle(&session)?,
                        &screen,
                        &mut chrome,
                        pane_modes,
                        copy_mode,
                        event,
                    )
                    .await?;
                }
                if dirty {
                    let snapshot = handle(&session)?.snapshot().await?;
                    pane_modes = snapshot.modes;
                    copy_mode = snapshot.copy_mode.as_ref().map(|state| state.pane);
                    draw(
                        &mut out,
                        &mut renderer,
                        &mut grid,
                        &snapshot,
                        &mut needs_full_render,
                    )?;
                    dirty = false;
                }
            }
        }
    }

    // The child's last output arrived after the final tick more often than not,
    // and the session task is still answering until its handle is dropped.
    if dirty {
        let snapshot = handle(&session)?.snapshot().await?;
        draw(
            &mut out,
            &mut renderer,
            &mut grid,
            &snapshot,
            &mut needs_full_render,
        )?;
    }

    drop(session);
    spawned
        .task
        .await
        .map_err(|_| LocalError::Session(SessionGone))?
        .map_err(LocalError::Pty)
}

/// Asks the outer terminal to turn on the modes cloo negotiated.
///
/// Paired with the reset registered on the raw-mode guard, which is what turns
/// them off again on every exit path — including a panic or a signal, where
/// nothing here gets a chance to run.
fn enable_modes(modes: OuterModes) -> Result<(), LocalError> {
    let mut out = io::stdout();
    out.write_all(&modes.enable()).map_err(LocalError::Output)?;
    out.flush().map_err(LocalError::Output)
}

/// Sends one decoded input event to the session.
///
/// The one branch worth reading twice is the mouse. `route_mouse` hit-tests the
/// report against the screen the client actually drew and answers with either
/// the wire event or a chrome target; a chrome target has no wire form at all,
/// which is what keeps a chrome click out of a child's input — delivered there
/// it appears in the user's shell as garbage. What cloo *does* with a chrome
/// target is `ChromeMouse`'s answer, and as of M6-02 the local path acts on it:
/// there is one pane and no gutter here, so click-to-focus and a divider drag
/// have nothing to move, but the wheel does — it walks the same server-owned
/// scrollback the copy-mode keys do, through the same commands.
async fn route(
    session: &SessionHandle,
    screen: &ScreenLayout,
    chrome: &mut ChromeMouse,
    modes: PaneModes,
    copy_mode: Option<cloo_proto::PaneId>,
    event: InputEvent,
) -> Result<(), LocalError> {
    match event {
        InputEvent::Keys(bytes) => session.input(bytes).await?,
        InputEvent::Paste(text) => session.paste(text).await?,
        InputEvent::Focus(focused) => session.focus(focused).await?,
        InputEvent::Mouse(report) => match route_mouse(screen, modes, &report) {
            MouseRoute::Application(event) => session.mouse(event).await?,
            MouseRoute::Chrome(target) => {
                if let Some(action) = chrome.feed(screen, target, &report) {
                    apply_chrome(session, action, copy_mode).await?;
                }
            }
        },
    }
    Ok(())
}

/// Applies one chrome gesture through the commands it maps onto.
///
/// The gesture is turned into `Action`s first and then dispatched, rather than
/// calling the session directly, so the local path and an attached client run the
/// same list — a gesture that gained a command would otherwise gain it in one
/// place only. The two actions the local path cannot answer are exactly the two a
/// single full-screen pane has no room for.
async fn apply_chrome(
    session: &SessionHandle,
    action: ChromeAction,
    copy_mode: Option<cloo_proto::PaneId>,
) -> Result<(), LocalError> {
    for command in action.commands(copy_mode) {
        match command {
            cloo_proto::Action::EnterCopyMode => session.enter_copy_mode().await?,
            cloo_proto::Action::CopyMotion(motion) => session.copy_motion(motion.into()).await?,
            cloo_proto::Action::FocusPane(pane) => session.focus_pane(pane).await?,
            cloo_proto::Action::ResizePane { pane, dir, delta } => {
                session.resize_pane(pane, dir, delta).await?;
            }
            // Nothing else is reachable from a gesture today, and a gesture that
            // grew one would be a change to `ChromeAction::commands`.
            _ => {}
        }
    }
    Ok(())
}

/// Borrows the session handle, or reports that the task is gone.
fn handle(session: &Option<SessionHandle>) -> Result<&SessionHandle, LocalError> {
    session.as_ref().ok_or(LocalError::Session(SessionGone))
}

/// What one turn of the loop did.
enum Step {
    /// The session reported something, or its task ended.
    Session(Option<SessionEvent>),
    /// The user typed something.
    Input(Vec<u8>),
    /// The outer terminal changed size.
    Resized(cloo_proto::Size),
    /// Stdin reached end of file.
    InputClosed,
    /// The frame timer fired.
    Frame,
}

/// Applies a snapshot to the client's cache and paints it.
///
/// The cache is resized to whatever the server reports before any row is
/// applied. A row that disagrees with the cache means a resize crossed a frame
/// in flight, and the client resyncs rather than drawing a guess.
fn draw(
    out: &mut io::Stdout,
    renderer: &mut Renderer,
    grid: &mut Grid,
    snapshot: &SessionSnapshot,
    needs_full_render: &mut bool,
) -> Result<(), LocalError> {
    let pane = &snapshot.pane;
    if grid.size() != pane.size {
        grid.resize(pane.size);
        *needs_full_render = true;
    }
    let mut changed = Vec::new();
    for row in &pane.rows {
        if grid.row(row.row) != Some(row.cells.as_slice()) {
            grid.apply(row)?;
            changed.push(row.row);
        }
    }
    let cursor = pane.cursor.map(|(pos, shape)| Cursor::new(pos, shape));
    let frame = if *needs_full_render {
        renderer.render_full(grid, cursor)
    } else {
        renderer.render_rows(grid, &changed, cursor)
    };
    out.write_all(frame).map_err(LocalError::Output)?;
    out.flush().map_err(LocalError::Output)?;
    *needs_full_render = false;
    Ok(())
}

/// Reads stdin on a dedicated thread and forwards bytes to the loop.
///
/// A thread rather than an async descriptor on purpose: making descriptor 0
/// non-blocking would change a file description the user's shell shares, and a
/// shell left non-blocking after cloo exits is a worse bug than a parked
/// thread. The thread is detached and dies with the process.
fn spawn_input_reader() -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0_u8; INPUT_BUF_LEN];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => {
                    if tx.send(buf[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anything that needs a real terminal is driven through the binary from
    // `tests/cli.rs` — see docs/TESTING.md. Login-shell resolution moved to
    // `cloo-server::launch` at M2-06, since it is a profile's answer now.

    #[test]
    fn the_frame_interval_is_about_sixty_per_second() {
        let per_second = 1000 / FRAME_INTERVAL.as_millis();
        assert!((55..=65).contains(&per_second), "got {per_second}fps");
    }
}
