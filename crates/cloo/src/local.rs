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
use cloo_client::input::{
    InputDecoder, InputEvent, MouseOwner, MouseReport, OuterModes, mouse_owner,
};
use cloo_client::outer::current_size;
use cloo_client::raw_mode::{RawMode, RawModeError};
use cloo_client::renderer::{Cursor, Grid, RenderError, Renderer};
use cloo_client::resize::ResizeWatch;
use cloo_proto::{MouseEvent, PaneId, PaneModes, Point};
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

/// Runs `program` in a single pane until it exits, and reports its status.
///
/// The outer terminal is restored on every exit path — normal, error, panic,
/// and signal — by the guard taken here; see `cloo-client`'s `raw_mode`.
///
/// # Errors
///
/// Returns a [`LocalError`] if the terminal could not be prepared, the child
/// could not be spawned, or a frame could not be drawn.
pub fn run(program: &str, args: &[String]) -> Result<ExitStatus, LocalError> {
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

    let result = runtime.block_on(session(program, args, size, caps, modes));

    // Restore before anything is printed: an error message rendered into a raw
    // terminal comes out as a staircase.
    let restored = raw.restore().map_err(LocalError::RawMode);
    let status = result?;
    restored?;
    Ok(status)
}

/// The async body of [`run`], with the terminal already raw.
async fn session(
    program: &str,
    args: &[String],
    size: cloo_proto::Size,
    caps: cloo_proto::TermCaps,
    modes: OuterModes,
) -> Result<ExitStatus, LocalError> {
    let mut config = PtyConfig::new(program).wire_size(size)?;
    for arg in args {
        config = config.arg(arg);
    }

    // Installed before the session exists so a `SIGWINCH` between spawning the
    // child and entering the loop is not the one resize that gets lost.
    let mut resizes = ResizeWatch::new(size).map_err(LocalError::Signal)?;

    let spawned = Session::spawn(&config, THE_PANE)?;
    let mut events = spawned.events;
    // Held for the loop's whole life. Dropping it is what lets the session task
    // finish, so it happens exactly once, below.
    let session = Some(spawned.handle);

    let mut grid = Grid::new(size);
    let mut renderer = Renderer::new(caps);
    let mut out = io::stdout();

    // What the child has negotiated, as of the last frame drawn. At most one
    // frame stale, which is what routing a mouse event costs instead of a
    // round trip to the session task per click.
    let mut pane_modes = PaneModes::default();
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
            Step::Session(Some(SessionEvent::Exited) | None) => break,
            Step::Input(bytes) => {
                for event in decoder.feed(&bytes) {
                    route(handle(&session)?, pane_modes, event).await?;
                }
            }
            // The outer terminal changed shape. The grid reflow and the child's
            // `TIOCSWINSZ` are the session task's business, in that order.
            Step::Resized(size) => handle(&session)?.resize(size).await?,
            // Stdin reached end of file. The child keeps running; it simply
            // gets no more input.
            Step::InputClosed => input_open = false,
            Step::Frame => {
                // A held escape prefix is released within a frame, which is what
                // makes a lone Escape key reach the pane at all.
                if let Some(event) = decoder.flush() {
                    route(handle(&session)?, pane_modes, event).await?;
                }
                if dirty {
                    let snapshot = handle(&session)?.snapshot().await?;
                    pane_modes = snapshot.modes;
                    draw(&mut out, &mut renderer, &mut grid, &snapshot)?;
                    dirty = false;
                }
            }
        }
    }

    // The child's last output arrived after the final tick more often than not,
    // and the session task is still answering until its handle is dropped.
    if dirty {
        let snapshot = handle(&session)?.snapshot().await?;
        draw(&mut out, &mut renderer, &mut grid, &snapshot)?;
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
/// The one branch worth reading twice is the mouse. An event the pane's
/// application does not own belongs to cloo's chrome, and it is **dropped
/// rather than forwarded**: a chrome click delivered to a child appears in the
/// user's shell as garbage. M1 has no chrome to act on it yet, so dropping is
/// the whole of the chrome half; M6-01 gives those events somewhere to go.
async fn route(
    session: &SessionHandle,
    modes: PaneModes,
    event: InputEvent,
) -> Result<(), LocalError> {
    match event {
        InputEvent::Keys(bytes) => session.input(bytes).await?,
        InputEvent::Paste(text) => session.paste(text).await?,
        InputEvent::Focus(focused) => session.focus(focused).await?,
        InputEvent::Mouse(report) => {
            // One pane filling the whole area, so every report is over it. Real
            // hit testing arrives with splits in M2.
            if mouse_owner(modes, &report, true) == MouseOwner::Application {
                session.mouse(pane_event(&report)).await?;
            }
        }
    }
    Ok(())
}

/// Places a mouse report in the session's only pane.
fn pane_event(report: &MouseReport) -> MouseEvent {
    MouseEvent {
        pane: THE_PANE,
        at: Point::new(report.col, report.row),
        kind: report.kind,
        mods: report.mods,
    }
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
) -> Result<(), LocalError> {
    let pane = &snapshot.pane;
    if grid.size() != pane.size {
        grid.resize(pane.size);
    }
    for row in &pane.rows {
        grid.apply(row)?;
    }
    let cursor = pane.cursor.map(|(pos, shape)| Cursor::new(pos, shape));
    out.write_all(renderer.render_full(grid, cursor))
        .map_err(LocalError::Output)?;
    out.flush().map_err(LocalError::Output)
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

/// The program a bare `cloo` should run.
///
/// `$SHELL` is what the user actually chose; `/bin/sh` is the fallback that
/// POSIX guarantees exists.
#[must_use]
pub fn default_shell() -> String {
    shell_from(std::env::var("SHELL").ok().as_deref())
}

/// The pure form of [`default_shell`].
fn shell_from(shell: Option<&str>) -> String {
    match shell {
        Some(shell) if !shell.is_empty() => shell.to_owned(),
        _ => "/bin/sh".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anything that needs a real terminal is driven through the binary from
    // `tests/cli.rs` — see docs/TESTING.md.

    #[test]
    fn a_set_shell_is_preferred() {
        assert_eq!(shell_from(Some("/usr/bin/fish")), "/usr/bin/fish");
    }

    #[test]
    fn an_absent_or_empty_shell_falls_back_to_sh() {
        assert_eq!(shell_from(None), "/bin/sh");
        assert_eq!(shell_from(Some("")), "/bin/sh");
    }

    #[test]
    fn the_frame_interval_is_about_sixty_per_second() {
        let per_second = 1000 / FRAME_INTERVAL.as_millis();
        assert!((55..=65).contains(&per_second), "got {per_second}fps");
    }
}
