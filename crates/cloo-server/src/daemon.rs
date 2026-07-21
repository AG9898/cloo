//! The single-pane daemon: own the PTY, serve clients over the socket, and
//! outlive every one of them.
//!
//! This is the M1 shape of the loop `crates/cloo/src/local.rs` runs in-process.
//! The difference that matters is lifetime: the child belongs to the daemon,
//! not to whoever is watching it. A client that detaches, disconnects, or dies
//! takes nothing with it — the PTY keeps being pumped between connections, so a
//! reattaching client finds the session where it left it rather than where it
//! last drew it.
//!
//! One client at a time, and a full grid capture per frame tick. Both are
//! deliberate placeholders with real successors: damage coalescing and fan-out
//! to several clients are M1-04, and the `mpsc<Command>` session task that
//! serializes input and resize is M1-03. What is already true is the property
//! those tasks must not break — the render rate is capped by a timer rather
//! than driven by PTY readiness, so a fast child cannot turn into one frame per
//! read.

use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use cloo_core::grid::wire_size;
use cloo_proto::{
    Action, ClientMessage, PaneId, ServerMessage, SessionId, Size, StreamError, TabId,
};
use cloo_term::TermSize;
use tokio::net::{UnixListener, UnixStream};

use crate::conn::{self, Connection};
use crate::pty::{PtyConfig, PtyError, PtyReactor, Pump};
use crate::socket::{Listener, SocketError};

/// The render tick, capping the fan-out rate at roughly 60fps.
///
/// A large `cat` is the classic multiplexer killer: without a cap, every PTY
/// read becomes a full-screen update on the wire and the socket, not the child,
/// becomes the bottleneck.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The single session, tab, and pane this milestone's daemon owns.
const THE_SESSION: SessionId = SessionId::new(1);
/// See [`THE_SESSION`].
const THE_TAB: TabId = TabId::new(1);
/// See [`THE_SESSION`].
const THE_PANE: PaneId = PaneId::new(1);

/// Everything the daemon can refuse to do.
#[derive(Debug)]
pub enum DaemonError {
    /// The socket could not be bound or owned.
    Socket(SocketError),
    /// The PTY or its child failed.
    Pty(PtyError),
    /// The listener could not be handed to the runtime, or accepting failed.
    Accept(io::Error),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(e) => write!(f, "{e}"),
            Self::Pty(e) => write!(f, "{e}"),
            Self::Accept(e) => write!(f, "could not accept a client: {e}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Socket(e) => Some(e),
            Self::Pty(e) => Some(e),
            Self::Accept(e) => Some(e),
        }
    }
}

impl From<SocketError> for DaemonError {
    fn from(value: SocketError) -> Self {
        Self::Socket(value)
    }
}

impl From<PtyError> for DaemonError {
    fn from(value: PtyError) -> Self {
        Self::Pty(value)
    }
}

/// Why one client's turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Served {
    /// The client left. The session keeps running.
    Gone,
    /// The child exited while this client was attached.
    ChildExited,
}

/// A bound socket and the pane behind it.
///
/// The [`Listener`] is held for the daemon's whole life, which is what unlinks
/// the socket on a clean exit; the async listener is a duplicate of the same
/// descriptor, so accepting and cleanup stay one lifetime.
pub struct Daemon {
    /// Owns the socket file and its lock. Dropped last, unlinking the path.
    _listener: Listener,
    accepting: UnixListener,
    reactor: PtyReactor,
    /// The session's authoritative size, which is also the pane's.
    size: Size,
}

impl Daemon {
    /// Binds `listener` and spawns the session's child on a fresh PTY.
    ///
    /// Must be called from inside a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Accept`] if the listener could not be registered
    /// with the runtime, or [`DaemonError::Pty`] if the child could not be
    /// started.
    pub fn new(listener: Listener, config: &PtyConfig) -> Result<Self, DaemonError> {
        let std_listener = listener.try_clone_std().map_err(DaemonError::Accept)?;
        std_listener
            .set_nonblocking(true)
            .map_err(DaemonError::Accept)?;
        let accepting = UnixListener::from_std(std_listener).map_err(DaemonError::Accept)?;

        let reactor = PtyReactor::spawn(config)?;
        let size = wire_size(config.term_size());

        Ok(Self {
            _listener: listener,
            accepting,
            reactor,
            size,
        })
    }

    /// The session's current size.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// The child's process id.
    #[must_use]
    pub fn child_id(&self) -> u32 {
        // The reactor is the only owner; this is for tests and diagnostics.
        self.reactor.child_id()
    }

    /// Serves clients until the session's child exits, then reaps it.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Pty`] if the PTY failed, or
    /// [`DaemonError::Accept`] if the listener did. A client that misbehaves is
    /// never one of these: it is disconnected and the session carries on.
    pub async fn run(&mut self) -> Result<ExitStatus, DaemonError> {
        loop {
            let stream = match self.wait_for_client().await? {
                Some(stream) => stream,
                // The child exited with nobody watching.
                None => break,
            };
            if self.serve(stream).await? == Served::ChildExited {
                break;
            }
        }
        Ok(self.reactor.wait()?)
    }

    /// Pumps the PTY until a client connects.
    ///
    /// Returns `None` if the child exited first. This is the half of "detach
    /// leaves the child running" that is easy to get wrong: a daemon that only
    /// pumps while a client is attached loses everything a child wrote between
    /// connections.
    async fn wait_for_client(&mut self) -> Result<Option<UnixStream>, DaemonError> {
        loop {
            tokio::select! {
                pumped = self.reactor.pump() => {
                    if pumped? == Pump::Eof {
                        return Ok(None);
                    }
                }
                accepted = self.accepting.accept() => {
                    let (stream, _addr) = accepted.map_err(DaemonError::Accept)?;
                    return Ok(Some(stream));
                }
            }
        }
    }

    /// Runs one client's connection from handshake to disconnect.
    async fn serve(&mut self, stream: UnixStream) -> Result<Served, DaemonError> {
        let mut conn = Connection::new(stream);

        let request = match conn::accept_attach(&mut conn).await {
            Ok(request) => request,
            // A client that is refused, broken, or gone is not a daemon
            // failure. It has already been told why where that was possible,
            // and the session is untouched either way.
            Err(_) => return Ok(Served::Gone),
        };

        // The session renders at what the attached client can draw. With one
        // client this is simply its size; the minimum across several clients
        // arrives with fan-out in M1-04.
        self.resize(request.size)?;

        let hello = ServerMessage::Hello {
            protocol_version: cloo_proto::PROTOCOL_VERSION,
            session: THE_SESSION,
            tabs: conn::single_tab(THE_TAB, "shell"),
            size: self.size,
        };
        if conn.send(&hello).await.is_err() {
            return Ok(Served::Gone);
        }
        if send_all(
            &mut conn,
            &conn::session_snapshot(THE_TAB, THE_PANE, &self.reactor.snapshot()),
        )
        .await
        .is_err()
        {
            return Ok(Served::Gone);
        }

        let mut frames = tokio::time::interval(FRAME_INTERVAL);
        // Missed ticks are frames nobody saw; there is no value in catching up.
        frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = false;

        loop {
            // `pump` and `recv` are both cancel-safe: each awaits readiness and
            // buffers before it decides anything, so losing this race drops a
            // wakeup and never a byte.
            let step = tokio::select! {
                pumped = self.reactor.pump() => Step::Output(pumped?),
                received = conn.recv::<ClientMessage>() => Step::From(received),
                _ = frames.tick() => Step::Frame,
            };

            match step {
                Step::Output(Pump::Bytes(_)) => dirty = true,
                Step::Output(Pump::Eof) => {
                    // Draw the child's last words before reporting its death.
                    let _ = send_all(
                        &mut conn,
                        &conn::session_snapshot(THE_TAB, THE_PANE, &self.reactor.snapshot()),
                    )
                    .await;
                    let _ = conn.send(&ServerMessage::Exit(0)).await;
                    let _ = conn.shutdown().await;
                    return Ok(Served::ChildExited);
                }
                Step::Frame => {
                    if dirty {
                        if send_all(
                            &mut conn,
                            &conn::session_snapshot(THE_TAB, THE_PANE, &self.reactor.snapshot()),
                        )
                        .await
                        .is_err()
                        {
                            return Ok(Served::Gone);
                        }
                        dirty = false;
                    }
                }
                Step::From(Ok(Some(ClientMessage::Input(bytes)))) => {
                    self.reactor.write_all(&bytes)?;
                }
                Step::From(Ok(Some(ClientMessage::Resize(size)))) => {
                    self.resize(size)?;
                    dirty = true;
                }
                Step::From(Ok(Some(
                    ClientMessage::Detach | ClientMessage::Command(Action::DetachClient),
                ))) => {
                    // Acknowledge, close, and leave everything else alone. The
                    // child never learns this happened.
                    let _ = conn.send(&ServerMessage::Detached).await;
                    let _ = conn.shutdown().await;
                    return Ok(Served::Gone);
                }
                // Splits, tabs, and mouse routing are M1-07 and M2. Ignoring
                // them keeps an old client from taking the session down.
                Step::From(Ok(Some(ClientMessage::Mouse(_) | ClientMessage::Command(_)))) => {}
                // A second attach on an attached connection is a desync.
                Step::From(Ok(Some(ClientMessage::Attach { .. }))) => {
                    let _ = conn::refuse(&mut conn, "this connection is already attached").await;
                    return Ok(Served::Gone);
                }
                // The client closed, or its connection broke. Either way the
                // session outlives it.
                Step::From(Ok(None) | Err(_)) => return Ok(Served::Gone),
            }
        }
    }

    /// Resizes the grid and the child's `winsize` together.
    ///
    /// A zero-sized client is ignored rather than refused: it has no bearing on
    /// a child that is running fine.
    fn resize(&mut self, size: Size) -> Result<(), DaemonError> {
        if size == self.size {
            return Ok(());
        }
        let Ok(term_size) = TermSize::new(size.cols, size.rows) else {
            return Ok(());
        };
        self.reactor.resize(term_size)?;
        self.size = size;
        Ok(())
    }
}

/// What one turn of a served connection did.
enum Step {
    /// The PTY produced output, or reached end of file.
    Output(Pump),
    /// The client sent something, closed, or broke.
    From(Result<Option<ClientMessage>, StreamError>),
    /// The frame timer fired.
    Frame,
}

/// Sends a batch of messages, stopping at the first failure.
async fn send_all<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    messages: &[ServerMessage],
) -> Result<(), StreamError> {
    for message in messages {
        conn.send(message).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anything that needs a real PTY and a real socket lives in
    // `tests/attach.rs` — see docs/TESTING.md.

    #[test]
    fn the_frame_interval_is_about_sixty_per_second() {
        let per_second = 1000 / FRAME_INTERVAL.as_millis();
        assert!((55..=65).contains(&per_second), "got {per_second}fps");
    }

    #[test]
    fn the_session_ids_are_distinct_newtypes() {
        assert_eq!(THE_SESSION.get(), 1);
        assert_eq!(THE_TAB.get(), 1);
        assert_eq!(THE_PANE.get(), 1);
    }
}
