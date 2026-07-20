//! The one-pane local smoke path.
//!
//! This is the M0 end of the roadmap: a single PTY, a single grid, and the
//! renderer, wired together **in-process**. There is no socket, no daemon, and
//! no detach — the child dies with the client, and that is the whole point of
//! the milestone. The socket lifecycle lands in M1-01 and turns this loop into
//! the daemon's session task with a client on the other end of a wire.
//!
//! The three-way split is already the real one, though, and nothing here
//! crosses it: `cloo-server` owns the PTY and the authoritative grid,
//! `cloo-client` owns raw mode and every escape sequence, and this module only
//! moves snapshots one way and bytes the other.
//!
//! Two ordering rules keep the terminal safe. Raw mode is entered *before* the
//! child is spawned, so a failure that is going to happen happens while the
//! terminal is still untouched; and the render is driven by a frame timer
//! rather than by PTY readiness, so a fast producer coalesces into at most one
//! frame per tick instead of one per read.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::process::ExitStatus;
use std::time::Duration;

use cloo_client::outer::{detect_caps, window_size};
use cloo_client::raw_mode::{RawMode, RawModeError};
use cloo_client::renderer::{Cursor, Grid, RenderError, Renderer};
use cloo_server::pty::{PaneSnapshot, PtyConfig, PtyError, PtyReactor, Pump};

/// The render tick, capping the frame rate at roughly 60fps.
///
/// A large `cat` is the classic multiplexer killer: without a cap, every PTY
/// read becomes a full-screen repaint and the renderer, not the child, becomes
/// the bottleneck.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Size of a single stdin read on the input thread.
const INPUT_BUF_LEN: usize = 1024;

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
        }
    }
}

impl std::error::Error for LocalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RawMode(e) => Some(e),
            Self::Output(e) | Self::Runtime(e) => Some(e),
            Self::Pty(e) => Some(e),
            Self::Render(e) => Some(e),
        }
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
    let size = outer_size();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(LocalError::Runtime)?;

    let result = runtime.block_on(session(program, args, size));

    // Restore before anything is printed: an error message rendered into a raw
    // terminal comes out as a staircase.
    let restored = raw.restore().map_err(LocalError::RawMode);
    let status = result?;
    restored?;
    Ok(status)
}

/// The outer terminal's geometry.
///
/// Stdout is asked first because that is where frames are written, and stdin is
/// the fallback for the case where output was redirected but the session is
/// still interactive. A terminal that reports nothing gets
/// [`outer::FALLBACK_SIZE`](cloo_client::outer::FALLBACK_SIZE) rather than an
/// error: refusing to start over an unanswered `ioctl` would be a worse
/// failure than drawing at a conventional 80x24.
fn outer_size() -> cloo_proto::Size {
    window_size(io::stdout().as_fd())
        .or_else(|_| window_size(io::stdin().as_fd()))
        .unwrap_or(cloo_client::outer::FALLBACK_SIZE)
}

/// The async body of [`run`], with the terminal already raw.
async fn session(
    program: &str,
    args: &[String],
    size: cloo_proto::Size,
) -> Result<ExitStatus, LocalError> {
    let mut config = PtyConfig::new(program).wire_size(size)?;
    for arg in args {
        config = config.arg(arg);
    }
    let mut reactor = PtyReactor::spawn(&config)?;

    let mut grid = Grid::new(size);
    let mut renderer = Renderer::new(detect_caps());
    let mut out = io::stdout();

    let mut input = spawn_input_reader();
    let mut input_open = true;
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    // Missed ticks are frames nobody saw; there is no value in catching up.
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut dirty = true;
    loop {
        // `pump` is cancel-safe: it awaits readiness and only then reads, so
        // losing this race drops a readiness notification, never bytes.
        let step = tokio::select! {
            pumped = reactor.pump() => Step::Output(pumped?),
            received = input.recv(), if input_open => match received {
                Some(bytes) => Step::Input(bytes),
                None => Step::InputClosed,
            },
            _ = frames.tick() => Step::Frame,
        };

        match step {
            Step::Output(Pump::Bytes(_)) => dirty = true,
            Step::Output(Pump::Eof) => break,
            Step::Input(bytes) => reactor.write_all(&bytes)?,
            // Stdin reached end of file. The child keeps running; it simply
            // gets no more input.
            Step::InputClosed => input_open = false,
            Step::Frame => {
                if dirty {
                    draw(&mut out, &mut renderer, &mut grid, &reactor.snapshot())?;
                    dirty = false;
                }
            }
        }
    }

    // The child's last output arrived after the final tick more often than not.
    if dirty {
        draw(&mut out, &mut renderer, &mut grid, &reactor.snapshot())?;
    }
    Ok(reactor.wait()?)
}

/// What one turn of the loop did.
enum Step {
    /// The PTY produced output, or reached end of file.
    Output(Pump),
    /// The user typed something.
    Input(Vec<u8>),
    /// Stdin reached end of file.
    InputClosed,
    /// The frame timer fired.
    Frame,
}

/// Applies a snapshot to the client's cache and paints it.
fn draw(
    out: &mut io::Stdout,
    renderer: &mut Renderer,
    grid: &mut Grid,
    snapshot: &PaneSnapshot,
) -> Result<(), LocalError> {
    if grid.size() != snapshot.size {
        grid.resize(snapshot.size);
    }
    for row in &snapshot.rows {
        grid.apply(row)?;
    }
    let cursor = snapshot.cursor.map(|(pos, shape)| Cursor::new(pos, shape));
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
