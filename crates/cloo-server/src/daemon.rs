//! The daemon's bounded damage fan-out loop.
//!
//! The daemon is a transport coordinator, not another owner of session state.
//! It receives coalesced [`SessionEvent::Output`] notifications, captures one
//! authoritative snapshot on a frame tick, and publishes the row delta through
//! a bounded `broadcast` channel. Every socket lives in its own task, so a
//! slow terminal can delay only its own writes. A lagged task discards its
//! partial history and asks the coordinator for a full snapshot.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use cloo_proto::{
    Action, ClientId, ClientMessage, PaneId, ServerMessage, SessionId, Size, StreamError, TabId,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::conn::{self, AttachRequest, Connection};
use crate::damage::{DamageFrame, DamageTracker};
use crate::pty::{PtyConfig, PtyError};
use crate::session::{Session, SessionEvent, SessionGone, SessionHandle, SessionSnapshot, usable};
use crate::socket::{Listener, SocketError};

/// The render tick, capping fan-out at roughly 60fps.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Frames held for each client before it must resync from a snapshot.
const DAMAGE_QUEUE: usize = 8;
/// Client-to-daemon requests waiting for the coordinator.
const CLIENT_COMMAND_QUEUE: usize = 64;

/// The single session, tab, and initial pane this milestone's daemon owns.
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
    /// The session task ended before the daemon was done with it.
    Session(SessionGone),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(e) => write!(f, "{e}"),
            Self::Pty(e) => write!(f, "{e}"),
            Self::Accept(e) => write!(f, "could not accept a client: {e}"),
            Self::Session(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Socket(e) => Some(e),
            Self::Pty(e) => Some(e),
            Self::Accept(e) => Some(e),
            Self::Session(e) => Some(e),
        }
    }
}

impl From<SessionGone> for DaemonError {
    fn from(value: SessionGone) -> Self {
        Self::Session(value)
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

/// A command sent from one socket task to the coordinator.
///
/// Socket tasks deliberately hold no [`SessionHandle`]. That keeps client
/// backpressure outside the session actor and lets the daemon release the only
/// handle promptly when the child exits.
enum ClientCommand {
    /// A version-checked client needs its hello, snapshot, and update receiver.
    Attached {
        client: ClientId,
        request: AttachRequest,
        reply: oneshot::Sender<ClientStart>,
    },
    /// A normal post-handshake client message.
    Message {
        client: ClientId,
        message: ClientMessage,
    },
    /// A broadcast receiver fell behind and needs a fresh baseline.
    Resync {
        reply: oneshot::Sender<ClientResync>,
    },
    /// The connection ended, so it no longer contributes to the minimum size.
    Gone { client: ClientId },
}

/// State a newly attached client needs before it can enter its socket loop.
struct ClientStart {
    size: Size,
    snapshot: SessionSnapshot,
    updates: broadcast::Receiver<DamageFrame>,
}

/// The private reply to a lagged client's resync request.
struct ClientResync {
    snapshot: SessionSnapshot,
    updates: broadcast::Receiver<DamageFrame>,
}

/// A bound socket and the session behind it.
///
/// The [`Listener`] is held for the daemon's whole life, which is what unlinks
/// the socket on a clean exit; the async listener is a duplicate of the same
/// descriptor, so accepting and cleanup stay one lifetime.
pub struct Daemon {
    /// Owns the socket file and its lock. Dropped last, unlinking the path.
    _listener: Listener,
    accepting: UnixListener,
    /// The only way into the session. `None` once the daemon releases it so
    /// the session task can finish and report the child's status.
    session: Option<SessionHandle>,
    events: mpsc::Receiver<SessionEvent>,
    task: Option<JoinHandle<Result<ExitStatus, PtyError>>>,
    child_id: u32,
    /// What the daemon last told the session its area was. The session remains
    /// authoritative; this is the answer to put in the next hello.
    size: Size,
    /// Current usable geometry of every attached client.
    client_sizes: BTreeMap<ClientId, Size>,
    next_client: u64,
    client_commands: mpsc::Receiver<ClientCommand>,
    client_tx: mpsc::Sender<ClientCommand>,
    /// Bounded frames fan out without a socket write in the coordinator.
    updates: broadcast::Sender<DamageFrame>,
    clients: JoinSet<()>,
    damage: DamageTracker,
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

        let spawned = Session::spawn(config, THE_PANE)?;
        let size = cloo_core::grid::wire_size(config.term_size());
        let (client_tx, client_commands) = mpsc::channel(CLIENT_COMMAND_QUEUE);
        let (updates, _) = broadcast::channel(DAMAGE_QUEUE);

        Ok(Self {
            _listener: listener,
            accepting,
            session: Some(spawned.handle),
            events: spawned.events,
            task: Some(spawned.task),
            child_id: spawned.child_id,
            size,
            client_sizes: BTreeMap::new(),
            next_client: 1,
            client_commands,
            client_tx,
            updates,
            clients: JoinSet::new(),
            damage: DamageTracker::default(),
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
        self.child_id
    }

    /// The handle into the session task.
    fn session(&self) -> Result<&SessionHandle, DaemonError> {
        self.session
            .as_ref()
            .ok_or(DaemonError::Session(SessionGone))
    }

    /// Serves clients until the session's child exits, then reaps it.
    ///
    /// Each attached client owns only a socket task and a bounded broadcast
    /// receiver. The select below is therefore the sole place that captures
    /// session state and sends a damage frame, so a lagging socket can never
    /// delay a PTY read or a session command.
    pub async fn run(&mut self) -> Result<ExitStatus, DaemonError> {
        let mut frames = tokio::time::interval(FRAME_INTERVAL);
        frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = false;

        loop {
            // All receives are cancel-safe: they wait for readiness and do not
            // mutate state before returning an item. The state changes happen
            // below in this single coordinator after the select chooses one.
            tokio::select! {
                event = self.events.recv() => match event {
                    Some(SessionEvent::Output) => dirty = true,
                    Some(SessionEvent::Exited) | None => {
                        self.publish_current().await?;
                        let _ = self.updates.send(DamageFrame::exit(0));
                        break;
                    }
                },
                command = self.client_commands.recv() => {
                    if let Some(command) = command {
                        self.apply_client_command(command, &mut dirty).await?;
                    }
                }
                accepted = self.accepting.accept() => {
                    let (stream, _addr) = accepted.map_err(DaemonError::Accept)?;
                    self.spawn_client(stream);
                }
                _ = frames.tick(), if dirty => {
                    self.publish_current().await?;
                    dirty = false;
                }
                joined = self.clients.join_next(), if !self.clients.is_empty() => {
                    // A connection task's own final `Gone` command owns the
                    // size bookkeeping. Reaping here only prevents completed
                    // tasks accumulating during a long-lived daemon.
                    let _ = joined;
                }
            }
        }

        // Releasing the handle is what tells the session task nobody can reach
        // it any more; it then reaps the child and returns its status.
        self.session = None;
        let Some(task) = self.task.take() else {
            return Err(DaemonError::Session(SessionGone));
        };
        let status = task
            .await
            .map_err(|_| DaemonError::Session(SessionGone))?
            .map_err(DaemonError::Pty)?;

        // Socket tasks never hold a session handle, so aborting them cannot
        // keep the session actor alive. Waiting for the session gave queued
        // final damage a scheduling turn; this then stops any terminal write
        // that still cannot make progress.
        self.clients.abort_all();
        while self.clients.join_next().await.is_some() {}
        Ok(status)
    }

    /// Starts a connection task without letting an unfinished handshake stall
    /// accepts, frame ticks, or every already-attached client.
    fn spawn_client(&mut self, stream: UnixStream) {
        let client = ClientId::new(self.next_client);
        self.next_client = self.next_client.saturating_add(1);
        let commands = self.client_tx.clone();
        self.clients.spawn(async move {
            serve_client(stream, client, commands).await;
        });
    }

    /// Applies one request sent by a socket task.
    async fn apply_client_command(
        &mut self,
        command: ClientCommand,
        dirty: &mut bool,
    ) -> Result<(), DaemonError> {
        match command {
            ClientCommand::Attached {
                client,
                request,
                reply,
            } => {
                // The capabilities were negotiated in the handshake. They do
                // not affect server state, so dropping them here preserves the
                // client-side policy boundary.
                let _ = request.term_caps;
                if usable(request.size) {
                    self.client_sizes.insert(client, request.size);
                    if self.resize_to_clients().await? {
                        *dirty = true;
                    }
                }

                // The snapshot and subscription have no await between them.
                // No damage frame can be published in that gap, so this new
                // receiver starts strictly after its full baseline.
                let snapshot = self.snapshot().await?;
                self.publish_snapshot(&snapshot);
                let updates = self.updates.subscribe();
                let _ = reply.send(ClientStart {
                    size: self.size,
                    snapshot,
                    updates,
                });
            }
            ClientCommand::Message { client, message } => match message {
                ClientMessage::Input(bytes) => self.session()?.input(bytes).await?,
                ClientMessage::Paste(text) => self.session()?.paste(text).await?,
                ClientMessage::Focus { focused } => self.session()?.focus(focused).await?,
                ClientMessage::Mouse(event) => self.session()?.mouse(event).await?,
                ClientMessage::Resize(size) => {
                    if usable(size) {
                        self.client_sizes.insert(client, size);
                        if self.resize_to_clients().await? {
                            *dirty = true;
                        }
                    }
                }
                // Detach is handled by the connection task so it can send the
                // acknowledgement before it reports `Gone`. All other actions
                // still await their own client and layout milestones.
                ClientMessage::Detach
                | ClientMessage::Attach { .. }
                | ClientMessage::Command(Action::DetachClient)
                | ClientMessage::Command(_) => {}
            },
            ClientCommand::Resync { reply } => {
                // As on attach, subscribe only after the snapshot command
                // returned. The coordinator is the only broadcaster, so this
                // receiver cannot miss a newer frame between the two.
                let snapshot = self.snapshot().await?;
                self.publish_snapshot(&snapshot);
                let updates = self.updates.subscribe();
                let _ = reply.send(ClientResync { snapshot, updates });
            }
            ClientCommand::Gone { client } => {
                if self.client_sizes.remove(&client).is_some() && self.resize_to_clients().await? {
                    *dirty = true;
                }
            }
        }
        Ok(())
    }

    /// Captures one authoritative snapshot and broadcasts only its delta.
    async fn publish_current(&mut self) -> Result<(), DaemonError> {
        let snapshot = self.snapshot().await?;
        self.publish_snapshot(&snapshot);
        Ok(())
    }

    /// Pushes `snapshot` through the non-blocking bounded fan-out.
    fn publish_snapshot(&mut self, snapshot: &SessionSnapshot) {
        if let Some(frame) = self.damage.update(THE_TAB, snapshot) {
            // With no clients this returns an error carrying the frame. That
            // is expected — a future attach gets a direct full snapshot.
            let _ = self.updates.send(frame);
        }
    }

    /// Asks the session task for the current picture.
    async fn snapshot(&self) -> Result<SessionSnapshot, DaemonError> {
        Ok(self.session()?.snapshot().await?)
    }

    /// Returns the minimum size of all usable attached clients.
    fn minimum_client_size(&self) -> Option<Size> {
        self.client_sizes
            .values()
            .copied()
            .reduce(|smallest, size| {
                Size::new(smallest.cols.min(size.cols), smallest.rows.min(size.rows))
            })
    }

    /// Applies the currently negotiated minimum size, if any.
    async fn resize_to_clients(&mut self) -> Result<bool, DaemonError> {
        match self.minimum_client_size() {
            Some(size) => self.resize(size).await,
            // No client means no outer-terminal authority. Keep the last
            // usable geometry so a detached child does not receive a surprise
            // resize to an arbitrary default.
            None => Ok(false),
        }
    }

    /// Tells the session its area changed and records the successful result.
    async fn resize(&mut self, size: Size) -> Result<bool, DaemonError> {
        if size == self.size || !usable(size) {
            return Ok(false);
        }
        self.session()?.resize(size).await?;
        self.size = size;
        Ok(true)
    }
}

/// Runs one client from handshake through disconnect.
///
/// A socket write is intentionally confined here. A full terminal, a paused
/// debugger, or a slow remote filesystem underneath a terminal can stall this
/// task, but never the coordinator's damage publication or the session actor.
async fn serve_client(stream: UnixStream, client: ClientId, commands: mpsc::Sender<ClientCommand>) {
    let mut conn = Connection::new(stream);
    let request = match conn::accept_attach(&mut conn).await {
        Ok(request) => request,
        Err(_) => return,
    };

    let (reply, started) = oneshot::channel();
    if commands
        .send(ClientCommand::Attached {
            client,
            request,
            reply,
        })
        .await
        .is_err()
    {
        return;
    }
    let Ok(ClientStart {
        size,
        snapshot,
        mut updates,
    }) = started.await
    else {
        return;
    };

    let hello = ServerMessage::Hello {
        protocol_version: cloo_proto::PROTOCOL_VERSION,
        session: THE_SESSION,
        tabs: conn::single_tab(THE_TAB, "shell"),
        size,
    };
    if conn.send(&hello).await.is_err()
        || send_all(&mut conn, &conn::session_snapshot(THE_TAB, &snapshot))
            .await
            .is_err()
    {
        let _ = commands.send(ClientCommand::Gone { client }).await;
        return;
    }

    loop {
        // `recv` on both the framed socket and a broadcast receiver is
        // cancel-safe: a losing select branch has not consumed a frame. The
        // subsequent socket writes are deliberately outside the select, where
        // they can delay only this one client task.
        tokio::select! {
            update = updates.recv() => match update {
                Ok(frame) => {
                    let ends_session = frame.ends_session();
                    if send_all(&mut conn, frame.messages()).await.is_err() || ends_session {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (reply, resynced) = oneshot::channel();
                    if commands.send(ClientCommand::Resync { reply }).await.is_err() {
                        break;
                    }
                    let Ok(ClientResync { snapshot, updates: replacement }) = resynced.await else {
                        break;
                    };
                    if send_all(&mut conn, &conn::session_snapshot(THE_TAB, &snapshot))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    updates = replacement;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            received = conn.recv::<ClientMessage>() => match received {
                Ok(Some(ClientMessage::Detach | ClientMessage::Command(Action::DetachClient))) => {
                    let _ = conn.send(&ServerMessage::Detached).await;
                    let _ = conn.shutdown().await;
                    break;
                }
                Ok(Some(ClientMessage::Attach { .. })) => {
                    let _ = conn::refuse(&mut conn, "this connection is already attached").await;
                    break;
                }
                Ok(Some(message)) => {
                    if commands.send(ClientCommand::Message { client, message }).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            },
        }
    }

    let _ = commands.send(ClientCommand::Gone { client }).await;
}

/// Sends a batch of messages, stopping at the first socket failure.
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

    #[test]
    fn the_smallest_attached_client_sets_both_dimensions() {
        let mut clients = BTreeMap::new();
        clients.insert(ClientId::new(1), Size::new(100, 30));
        clients.insert(ClientId::new(2), Size::new(80, 40));
        let smallest = clients.values().copied().reduce(|current, size| {
            Size::new(current.cols.min(size.cols), current.rows.min(size.rows))
        });
        assert_eq!(smallest, Some(Size::new(80, 30)));
    }

    #[tokio::test]
    async fn a_lagged_receiver_can_be_replaced_without_waiting_for_it() {
        let (updates, mut lagging) = broadcast::channel(1);
        let sent = updates.send(DamageFrame::exit(0));
        assert!(matches!(sent, Ok(1)));
        let sent = updates.send(DamageFrame::exit(0));
        assert!(matches!(sent, Ok(1)));
        assert!(matches!(
            lagging.recv().await,
            Err(broadcast::error::RecvError::Lagged(1))
        ));

        let mut replacement = updates.subscribe();
        let sent = updates.send(DamageFrame::exit(0));
        assert!(matches!(sent, Ok(2)));
        assert!(
            replacement
                .recv()
                .await
                .expect("replacement receives")
                .ends_session()
        );
    }
}
