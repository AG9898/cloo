//! Attaching to a daemon, and leaving without taking it down.
//!
//! The client's side of the handshake. It connects, sends
//! [`ClientMessage::Attach`] carrying its protocol version, geometry, and
//! capabilities, and refuses to interpret anything until the reply is a
//! [`ServerMessage::Hello`] whose version matches. Both directions check:
//! `Attach` lets the server catch a stale client and `Hello` lets a stale
//! client catch a rebuilt server, which is the case that actually happens the
//! first time anyone rebuilds mid-session.
//!
//! Every refusal here is a message a user can act on. A
//! [`ServerMessage::Refused`] is surfaced with the server's own reason string
//! rather than being flattened into "connection failed", and a missing socket
//! says so rather than reporting a bare `ENOENT`.
//!
//! Detach is the other half of the milestone and is deliberately unremarkable:
//! [`Attached::detach`] sends [`ClientMessage::Detach`], waits for the
//! acknowledgement, and drops the connection. Nothing about it reaches the
//! child — that is the point.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cloo_core::keymap::Keymap;
use cloo_core::layout::Side;
use cloo_core::{Config, Profile, VisualConfig};
use cloo_proto::{
    Action, AttentionState, ClientMessage, CopyModeState, CursorShape, FrameStream, LayoutSnapshot,
    MouseEvent, PROTOCOL_VERSION, PaneAttention, PaneId, PaneInfo, PaneModes, Point, ProtoError,
    ServerMessage, SessionId, SessionSummary, Size, StreamError, TabSummary, TermCaps,
    WorkspaceStatus, check_version,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::capabilities::{CapsError, detect_attach_caps};
use crate::chrome::{
    Attention, AttentionQueue, ChromeOptions, PaneChrome, PrefixHint, StatusBar, TOAST_CAPACITY,
    TabBar, ToastDeck, resize_affordance_spans, toast_rows, toast_stack_span,
};
use crate::copy_mode::{highlight_spans, status_span as copy_status_span};
use crate::effects::{EffectPolicy, apply_effect};
use crate::input::{
    ChromeAction, ChromeMouse, ChromeTarget, Divider, InputDecoder, InputEvent, KeyRoute,
    KeyRouter, MouseRoute, OuterModes, PaneArea, ScreenLayout, overlay_action, palette_actions,
    queue_action, route_mouse,
};
use crate::motion::{Motion, MotionKind, MotionSettings, Phase, phase_span};
use crate::outer::current_size;
use crate::overlay::{
    AttentionEntry, ClientSurface, HELP_KEY, LaunchNotice, LaunchRequest, Overlay, OverlayKind,
    OverlayOutcome, PaneDetails, SessionEntry, backdrop_span, launch_notice_span, overlay_spans,
};
use crate::raw_mode::{RawMode, RawModeError};
use crate::renderer::{Cursor, FramePane, Grid, RenderError, Renderer, compose_frame};
use crate::resize::ResizeWatch;
use crate::session_catalog::{SessionCatalogEntry, SessionCatalogError, discover_sessions};
use crate::status::{
    ClientStatus, REPOSITORY_REFRESH_INTERVAL, RepositoryStatus, SystemClock, repository_status,
};
use crate::theme::{Theme, ThemeToken};

/// The render tick shared with the daemon: roughly 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Size of a single blocking stdin read on the helper thread.
const INPUT_BUF_LEN: usize = 1024;
/// The fixed outer rows reserved before session geometry: tab and status.
///
/// Pane headers and bottom edges live inside the session's framed allocations,
/// so they are not subtracted a second time here.
const FIXED_CHROME_ROWS: u16 = 2;
/// How often an open session switcher verifies its catalog again.
const SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// A healthy detach acknowledgement should be immediate; dropping the socket
/// after this bound still removes the old attachment without touching its daemon.
const SWITCH_DETACH_DEADLINE: Duration = Duration::from_secs(2);
/// How long one keyboard resize remains visible after its layout answer.
const KEYBOARD_RESIZE_LINGER: Duration = Duration::from_millis(750);

type CatalogResult = Result<Vec<SessionCatalogEntry>, SessionCatalogError>;
type RepositoryResult = (PathBuf, Option<RepositoryStatus>);

/// Everything attaching can refuse to do.
#[derive(Debug)]
pub enum AttachError {
    /// The outer terminal's capabilities could not be negotiated, so there was
    /// nothing to attach with. Refused before the socket is touched.
    Capabilities(CapsError),
    /// Nothing is listening on the socket.
    NoDaemon(PathBuf),
    /// The socket could not be connected to.
    Connect {
        /// The socket path.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The server turned the attach away, and said why.
    Refused(String),
    /// The server speaks a different protocol version.
    Version(ProtoError),
    /// The server replied with something other than a hello or a refusal.
    UnexpectedReply,
    /// The server closed before replying.
    Closed,
    /// The connection failed.
    Stream(StreamError),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capabilities(e) => write!(f, "{e}"),
            Self::NoDaemon(path) => write!(
                f,
                "no cloo daemon is listening on {}; start one first",
                path.display()
            ),
            Self::Connect { path, source } => {
                write!(f, "could not connect to {}: {source}", path.display())
            }
            Self::Refused(reason) => write!(f, "the cloo server refused the attach: {reason}"),
            Self::Version(e) => write!(f, "{e}"),
            Self::UnexpectedReply => {
                f.write_str("the cloo server replied to an attach with something else")
            }
            Self::Closed => f.write_str("the cloo server closed the connection during the attach"),
            Self::Stream(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capabilities(e) => Some(e),
            Self::Connect { source, .. } => Some(source),
            Self::Version(e) => Some(e),
            Self::Stream(e) => Some(e),
            Self::NoDaemon(_) | Self::Refused(_) | Self::UnexpectedReply | Self::Closed => None,
        }
    }
}

impl From<StreamError> for AttachError {
    fn from(value: StreamError) -> Self {
        Self::Stream(value)
    }
}

impl From<CapsError> for AttachError {
    fn from(value: CapsError) -> Self {
        Self::Capabilities(value)
    }
}

/// Everything a read-only session inspection can refuse to do.
///
/// Inspection is deliberately separate from [`AttachError`]: it sends no
/// terminal size or capabilities and never becomes an attached client.
#[derive(Debug)]
pub enum InspectError {
    /// Nothing is listening on the candidate socket.
    NoDaemon(PathBuf),
    /// The candidate socket could not be connected to.
    Connect {
        /// The candidate path.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The peer rejected the versioned inspection.
    Refused(String),
    /// The peer answered with something other than a session summary.
    UnexpectedReply,
    /// The peer closed before answering.
    Closed,
    /// The framed connection failed.
    Stream(StreamError),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDaemon(path) => {
                write!(f, "no cloo daemon is listening on {}", path.display())
            }
            Self::Connect { path, source } => {
                write!(f, "could not inspect {}: {source}", path.display())
            }
            Self::Refused(reason) => write!(f, "the cloo server refused inspection: {reason}"),
            Self::UnexpectedReply => {
                f.write_str("the candidate replied to inspection with something else")
            }
            Self::Closed => f.write_str("the candidate closed during inspection"),
            Self::Stream(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for InspectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect { source, .. } => Some(source),
            Self::Stream(err) => Some(err),
            Self::NoDaemon(_) | Self::Refused(_) | Self::UnexpectedReply | Self::Closed => None,
        }
    }
}

impl From<StreamError> for InspectError {
    fn from(value: StreamError) -> Self {
        Self::Stream(value)
    }
}

/// Everything the live attached-client loop can refuse to do.
///
/// The handshake errors remain [`AttachError`] because they are the useful
/// explanation when no session can be reached. The other variants name the
/// local boundary that failed after the socket was already a valid choice.
#[derive(Debug)]
pub enum AttachRunError {
    /// The outer terminal could not enter or leave raw mode.
    RawMode(RawModeError),
    /// Capabilities could not be negotiated before attaching.
    Capabilities(CapsError),
    /// The daemon refused or lost the attachment.
    Attach(AttachError),
    /// A server row disagreed with the client cache.
    Render(RenderError),
    /// The client's terminal could not be written.
    Output(io::Error),
    /// The single-thread Tokio runtime could not be built.
    Runtime(io::Error),
    /// A `SIGWINCH` watcher could not be installed.
    Signal(io::Error),
}

impl fmt::Display for AttachRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawMode(RawModeError::NotATerminal) => {
                f.write_str("cloo attach must be run from a terminal")
            }
            Self::RawMode(err) => write!(f, "{err}"),
            Self::Capabilities(err) => write!(f, "{err}"),
            Self::Attach(err) => write!(f, "{err}"),
            Self::Render(err) => write!(f, "render failed: {err}"),
            Self::Output(err) => write!(f, "could not write to the terminal: {err}"),
            Self::Runtime(err) => write!(f, "could not start the runtime: {err}"),
            Self::Signal(err) => write!(f, "could not watch for terminal resizes: {err}"),
        }
    }
}

impl std::error::Error for AttachRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RawMode(err) => Some(err),
            Self::Capabilities(err) => Some(err),
            Self::Attach(err) => Some(err),
            Self::Render(err) => Some(err),
            Self::Output(err) | Self::Runtime(err) | Self::Signal(err) => Some(err),
        }
    }
}

impl From<AttachError> for AttachRunError {
    fn from(value: AttachError) -> Self {
        Self::Attach(value)
    }
}

impl From<RenderError> for AttachRunError {
    fn from(value: RenderError) -> Self {
        Self::Render(value)
    }
}

/// A live attachment to a session.
///
/// Holds the connection and what the server said about the session at attach
/// time. Nothing here is authoritative: the size and tab list are the server's
/// answers, cached only so the client can draw chrome without asking again.
#[derive(Debug)]
pub struct Attached<T> {
    conn: FrameStream<T>,
    session: SessionId,
    tabs: Vec<TabSummary>,
    size: Size,
    status: Option<WorkspaceStatus>,
}

impl<T> Attached<T> {
    /// The session this client is attached to.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The session's tabs, as of the attach.
    #[must_use]
    pub fn tabs(&self) -> &[TabSummary] {
        &self.tabs
    }

    /// The effective session size, already reduced to the minimum across every
    /// attached client.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// The newest daemon-owned attached workspace projection.
    #[must_use]
    pub fn status(&self) -> Option<&WorkspaceStatus> {
        self.status.as_ref()
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Attached<T> {
    /// Reads the next message from the server.
    ///
    /// `Ok(None)` means the server closed cleanly, which after a detach is the
    /// expected end of the conversation.
    ///
    /// # Errors
    ///
    /// Returns the transport or framing failure.
    pub async fn recv(&mut self) -> Result<Option<ServerMessage>, StreamError> {
        let message = self.conn.recv().await?;
        match &message {
            Some(ServerMessage::Tabs(tabs)) => self.tabs = tabs.clone(),
            Some(ServerMessage::WorkspaceStatus(status)) => {
                self.size = status.effective_size;
                self.status = Some(status.clone());
            }
            _ => {}
        }
        Ok(message)
    }

    /// Sends keyboard bytes to the focused pane.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_input(&mut self, bytes: Vec<u8>) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Input(bytes)).await
    }

    /// Sends pasted text as text.
    ///
    /// Deliberately not [`send_input`](Self::send_input): whether the child
    /// wants paste brackets is a mode only the server can see, so the client
    /// sends what the user pasted and lets the server encode it.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_paste(&mut self, text: Vec<u8>) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Paste(text)).await
    }

    /// Tells the server the client's terminal gained or lost focus.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_focus(&mut self, focused: bool) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Focus { focused }).await
    }

    /// Sends a mouse event the client routed to the pane's application.
    ///
    /// Only application-owned events belong here. An event
    /// [`mouse_owner`](crate::input::mouse_owner) gave to the chrome must never
    /// be sent: it would land in the child's input as garbage.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_mouse(&mut self, event: MouseEvent) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Mouse(event)).await
    }

    /// Tells the server the client's terminal changed size.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_resize(&mut self, size: Size) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Resize(size)).await
    }

    /// Sends a keymap-resolved command to the session actor.
    ///
    /// Commands carry intent rather than raw keys, so keymap changes do not
    /// require a wire-format change.
    ///
    /// # Errors
    ///
    /// Returns the transport or framing failure.
    pub async fn send_command(&mut self, action: Action) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Command(action)).await
    }

    /// Detaches, leaving the session and its children running.
    ///
    /// Waits for [`ServerMessage::Detached`] so the caller knows the server
    /// heard it, discarding any damage still in flight. A server that closes
    /// without acknowledging is not an error: the session is just as detached
    /// either way.
    ///
    /// # Errors
    ///
    /// Returns the transport failure if the request could not be sent.
    pub async fn detach(mut self) -> Result<(), AttachError> {
        self.conn.send(&ClientMessage::Detach).await?;
        loop {
            match self.conn.recv::<ServerMessage>().await {
                Ok(Some(ServerMessage::Detached)) | Ok(None) => return Ok(()),
                // Frames the server had already queued. They describe a session
                // this client is done with.
                Ok(Some(_)) => {}
                Err(_) => return Ok(()),
            }
        }
    }
}

/// Connects to a daemon's socket and attaches to its session.
///
/// `term_caps` is a parameter rather than something read here so the handshake
/// stays a pure function of what it is given. A caller negotiating from the
/// real environment gets them from
/// [`detect_attach_caps`](crate::capabilities::detect_attach_caps), whose
/// [`CapsError`] converts into [`AttachError::Capabilities`] with a `?` — that
/// is where an unset or `dumb` `TERM` is turned away, before the socket is
/// touched.
///
/// # Errors
///
/// Returns [`AttachError::NoDaemon`] when nothing is listening — the common
/// case, and worth its own message — or any [`handshake`] failure.
pub async fn attach(
    path: &Path,
    size: Size,
    term_caps: TermCaps,
    session: Option<SessionId>,
) -> Result<Attached<UnixStream>, AttachError> {
    let stream = UnixStream::connect(path).await.map_err(|source| {
        match source.kind() {
            // Both mean the same thing to a user: there is no daemon there. A
            // stale socket file left by a killed daemon produces the first.
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => {
                AttachError::NoDaemon(path.to_owned())
            }
            _ => AttachError::Connect {
                path: path.to_owned(),
                source,
            },
        }
    })?;
    handshake(FrameStream::new(stream), size, term_caps, session).await
}

/// Inspects one untrusted socket candidate without attaching to it.
///
/// The request carries this build's protocol version and no terminal
/// capabilities or geometry. Only a [`ServerMessage::SessionSummary`] is
/// accepted; attach frames, damage, and every other reply are rejected without
/// being interpreted as session data.
///
/// # Errors
///
/// Returns [`InspectError::NoDaemon`] for a stale or vanished socket, or the
/// specific connection, framing, refusal, or reply-shape failure.
pub async fn inspect(path: &Path) -> Result<SessionSummary, InspectError> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|source| match source.kind() {
            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound => {
                InspectError::NoDaemon(path.to_owned())
            }
            _ => InspectError::Connect {
                path: path.to_owned(),
                source,
            },
        })?;
    inspect_handshake(FrameStream::new(stream)).await
}

/// Performs a read-only inspection over an already-connected transport.
///
/// Split out from [`inspect`] so the exact one-request/one-summary exchange is
/// unit-testable without a filesystem socket.
///
/// # Errors
///
/// Returns the peer's refusal, a framing failure, a close before the reply, or
/// [`InspectError::UnexpectedReply`] for anything but a session summary.
pub async fn inspect_handshake<T: AsyncRead + AsyncWrite + Unpin>(
    mut conn: FrameStream<T>,
) -> Result<SessionSummary, InspectError> {
    conn.send(&ClientMessage::InspectSession {
        protocol_version: PROTOCOL_VERSION,
    })
    .await?;

    match conn.recv::<ServerMessage>().await? {
        Some(ServerMessage::SessionSummary(summary)) => Ok(summary),
        Some(ServerMessage::Refused { reason }) => Err(InspectError::Refused(reason)),
        Some(_) => Err(InspectError::UnexpectedReply),
        None => Err(InspectError::Closed),
    }
}

/// Performs the attach handshake over an already-connected transport.
///
/// Split out from [`attach`] so the handshake is testable over a duplex pipe,
/// and so a future `cloo attach` over something other than a Unix socket does
/// not have to reimplement it.
///
/// # Errors
///
/// Returns [`AttachError::Refused`] with the server's reason,
/// [`AttachError::Version`] if the server's hello announces a version this
/// build does not speak, [`AttachError::Closed`] if the server said nothing,
/// or [`AttachError::UnexpectedReply`] if it said something else entirely.
pub async fn handshake<T: AsyncRead + AsyncWrite + Unpin>(
    mut conn: FrameStream<T>,
    size: Size,
    term_caps: TermCaps,
    session: Option<SessionId>,
) -> Result<Attached<T>, AttachError> {
    conn.send(&ClientMessage::Attach {
        protocol_version: PROTOCOL_VERSION,
        size,
        term_caps,
        session,
    })
    .await?;

    match conn.recv::<ServerMessage>().await? {
        Some(ServerMessage::Hello {
            protocol_version,
            session,
            tabs,
            size,
        }) => {
            check_version(protocol_version).map_err(AttachError::Version)?;
            Ok(Attached {
                conn,
                session,
                tabs,
                size,
                status: None,
            })
        }
        Some(ServerMessage::Refused { reason }) => Err(AttachError::Refused(reason)),
        Some(_) => Err(AttachError::UnexpectedReply),
        None => Err(AttachError::Closed),
    }
}

/// Runs one attached client until it detaches, the daemon exits, or the socket
/// closes.
///
/// The caller supplies the complete resolved local configuration so the client
/// applies the same keymap it advertises, offers the profiles it resolved, and
/// uses its visual preferences from the first frame. `reload_config` repeats
/// that client-local resolution after a daemon reload revision and returns
/// `None` when the replacement was rejected. Profiles remain the *launcher's*
/// list and never an authority: the daemon owns the table a launch identifier
/// is resolved against, so a profile this client can see but that daemon cannot
/// is refused there and reported here.
///
/// Entering raw mode, enabling outer-terminal reporting, restoring both on every
/// exit path, and owning the render loop all live here because they are client
/// concerns — the daemon never writes a byte to the user's terminal.
///
/// # Errors
///
/// Returns an actionable attach, terminal, rendering, or signal error. The raw
/// mode guard restores the terminal before an error reaches the caller.
pub fn run<F>(path: &Path, config: Config, reload_config: F) -> Result<i32, AttachRunError>
where
    F: FnMut() -> Option<Config>,
{
    // This has to be first. A pipe should explain that this is not an attached
    // terminal, and no attach attempt or reporting mode should precede it.
    let raw = RawMode::stdin().map_err(AttachRunError::RawMode)?;
    let outer_size = current_size();
    let caps = detect_attach_caps().map_err(AttachRunError::Capabilities)?;
    let modes = OuterModes::negotiated(caps);
    raw.on_restore(&modes.disable())
        .map_err(AttachRunError::RawMode)?;
    enable_modes(modes)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(AttachRunError::Runtime)?;
    let result = runtime.block_on(run_attachments(
        path.to_owned(),
        outer_size,
        caps,
        modes,
        config,
        reload_config,
    ));

    // Restore before `main` prints an error: diagnostics written while raw are
    // unreadable, and `RawMode` also turns reporting modes back off here.
    let restored = raw.restore().map_err(AttachRunError::RawMode);
    let status = result?;
    restored?;
    Ok(status)
}

/// Keeps terminal ownership stable while the client moves between daemon
/// sockets. Input and resize sources are created once, so a switch cannot leave
/// an old reader consuming keys intended for the new attachment.
async fn run_attachments<F>(
    mut path: PathBuf,
    outer_size: Size,
    caps: TermCaps,
    modes: OuterModes,
    config: Config,
    mut reload_config: F,
) -> Result<i32, AttachRunError>
where
    F: FnMut() -> Option<Config>,
{
    let mut resizes = ResizeWatch::new(outer_size).map_err(AttachRunError::Signal)?;
    let mut input = spawn_input_reader();
    let mut attached = attach(&path, session_size(outer_size), caps, None).await?;
    loop {
        match live_loop(
            &path,
            attached,
            AttachmentSettings {
                caps,
                modes,
                config: config.clone(),
            },
            &mut reload_config,
            &mut input,
            &mut resizes,
        )
        .await?
        {
            LiveOutcome::Exit(status) => return Ok(status),
            LiveOutcome::Switch {
                path: next_path,
                attached: next,
            } => {
                path = next_path;
                attached = next;
            }
        }
    }
}

struct AttachmentSettings {
    caps: TermCaps,
    modes: OuterModes,
    config: Config,
}

/// A terminal's usable session area after the frame's fixed chrome rows.
///
/// A client reports the pane area rather than its full outer-terminal height:
/// tab and status rows are client-owned and must not become child grid rows.
/// Pane frames are accounted for by the server's single framed geometry pass.
const fn session_size(outer: Size) -> Size {
    Size::new(outer.cols, outer.rows.saturating_sub(FIXED_CHROME_ROWS))
}

/// The async attached-client body, entered only after raw mode is armed.
async fn live_loop<F>(
    path: &Path,
    mut attached: Attached<UnixStream>,
    settings: AttachmentSettings,
    reload_config: &mut F,
    input: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    resizes: &mut ResizeWatch,
) -> Result<LiveOutcome, AttachRunError>
where
    F: FnMut() -> Option<Config>,
{
    let AttachmentSettings {
        caps,
        modes,
        config,
    } = settings;
    let outer_size = resizes.last();
    // The chrome shows the chord this client actually resolved, so a rebound
    // prefix is discoverable from the first frame rather than only from the
    // configuration file.
    let keymap = config.keys().clone();
    let prefix = keymap.prefix().to_string();
    let profiles = config.profiles().to_vec();
    let visual = *config.visual();
    let mut state = LiveState::new(
        outer_size,
        attached.session(),
        attached.tabs().to_vec(),
        prefix,
    )
    .preferences(caps, visual)
    .profiles(profiles);
    let mut renderer = Renderer::new(caps);
    let mut out = io::stdout();
    let policy = EffectPolicy::default();
    let mut input_open = true;
    let mut decoder = InputDecoder::new(modes);
    let mut keys = KeyRouter::new(keymap);
    let mut chrome = ChromeMouse::new();
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut motion = Motion::new(state.motion_settings());
    let mut phase = None;
    let mut dirty = true;
    let mut detaching = false;
    let (catalog_tx, mut catalog_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut catalog_refreshing = false;
    let mut catalog_refreshed = None;
    let clock = SystemClock;
    let _ = state.local_status.refresh_clock(&clock);
    let (repository_tx, mut repository_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut repository_refreshing = BTreeSet::new();
    let mut repository_refreshed = None;

    loop {
        let step = tokio::select! {
            message = attached.recv() => Step::Server(message.map_err(AttachError::from)?),
            received = input.recv(), if input_open && !detaching => match received {
                Some(bytes) => Step::Input(bytes),
                None => Step::InputClosed,
            },
            resized = resizes.changed(), if !detaching => Step::Resized(resized),
            catalog = catalog_rx.recv(), if catalog_refreshing => {
                Step::Catalog(catalog.expect("a refresh sender exists while one is running"))
            },
            repository = repository_rx.recv() => {
                Step::Repository(repository.expect("the repository sender lives with the loop"))
            },
            _ = frames.tick() => Step::Frame,
        };

        match step {
            Step::Server(Some(ServerMessage::Effect { effect, .. })) => {
                let _ = apply_effect(&mut out, caps, policy, &effect)
                    .map_err(AttachRunError::Output)?;
            }
            Step::Server(Some(ServerMessage::Exit(status))) => {
                if dirty {
                    draw(&mut out, &mut renderer, &state, phase)?;
                }
                return Ok(LiveOutcome::Exit(status));
            }
            Step::Server(Some(ServerMessage::Detached)) | Step::Server(None) => {
                return Ok(LiveOutcome::Exit(0));
            }
            Step::Server(Some(ServerMessage::ConfigReloaded { revision })) => {
                if state.needs_reload(revision)
                    && state.reload_visual(revision, reload_config().map(|config| *config.visual()))
                {
                    // A changed motion preference takes effect on this frame;
                    // no transition authored under the preceding preference
                    // survives the atomic visual replacement.
                    motion = Motion::new(state.motion_settings());
                    phase = None;
                    dirty = true;
                }
            }
            Step::Server(Some(message)) => {
                let transition = state.transition_for(&message);
                dirty |= state.apply_at(message, Instant::now())?;
                state.tabs = attached.tabs().to_vec();
                if let Some(cwd) = state.refresh_repository_target() {
                    repository_refreshed = None;
                    if repository_refreshing.insert(cwd.clone()) {
                        start_repository_refresh(repository_tx.clone(), cwd);
                    }
                }
                if let Some(kind) = transition {
                    phase = Some(motion.start(kind, Instant::now()));
                    dirty = true;
                }
            }
            Step::Input(bytes) => {
                if let Some(settled) = motion.interrupt() {
                    phase = Some(settled);
                }
                let overlay_was_open = state.overlay.is_some();
                match route(
                    &mut attached,
                    &mut state,
                    &mut chrome,
                    &mut keys,
                    &mut decoder,
                    bytes,
                )
                .await?
                {
                    RouteOutcome::Continue => {}
                    RouteOutcome::Detach => detaching = true,
                    RouteOutcome::Switch(target) if target == path => {
                        state.overlay = None;
                    }
                    RouteOutcome::Switch(target) => {
                        match attach(&target, session_size(state.outer_size), caps, None).await {
                            Ok(next) => {
                                // The selected daemon has accepted the new
                                // attachment before the current one is released.
                                // A disappearing row therefore cannot strand the
                                // user between sessions.
                                let _ =
                                    tokio::time::timeout(SWITCH_DETACH_DEADLINE, attached.detach())
                                        .await;
                                return Ok(LiveOutcome::Switch {
                                    path: target,
                                    attached: next,
                                });
                            }
                            Err(_) => {
                                state.remove_session(&target);
                                catalog_refreshed = None;
                            }
                        }
                    }
                }
                state.prefix_pending = keys.is_pending();
                if overlay_was_open != state.overlay.is_some() {
                    phase = Some(motion.start(MotionKind::Overlay, Instant::now()));
                }
                dirty = true;
                if state.session_switcher_open() && !catalog_refreshing {
                    start_catalog_refresh(catalog_tx.clone());
                    catalog_refreshing = true;
                }
            }
            Step::InputClosed => input_open = false,
            Step::Resized(size) => {
                if let Some(settled) = motion.interrupt() {
                    phase = Some(settled);
                }
                state.set_outer_size(size);
                attached
                    .send_resize(session_size(size))
                    .await
                    .map_err(AttachError::from)?;
                dirty = true;
            }
            Step::Frame => {
                if let Some(InputEvent::Keys(bytes)) = decoder.flush() {
                    if let Some(settled) = motion.interrupt() {
                        phase = Some(settled);
                    }
                    let overlay_was_open = state.overlay.is_some();
                    match route_keys(&mut attached, &mut keys, &mut state, bytes).await? {
                        RouteOutcome::Continue => {}
                        RouteOutcome::Detach => detaching = true,
                        // A lone Escape flush cannot confirm a switcher row.
                        RouteOutcome::Switch(_) => {}
                    }
                    state.prefix_pending = keys.is_pending();
                    if overlay_was_open != state.overlay.is_some() {
                        phase = Some(motion.start(MotionKind::Overlay, Instant::now()));
                    }
                    dirty = true;
                }
                if let Some(next) = motion.tick(Instant::now()) {
                    phase = Some(next);
                    dirty = true;
                }
                // A launch the workspace silently refused has no message to
                // wait for, so the render clock is what turns its deadline into
                // something the user can see. The toast stack's entrance and
                // expiry ride the same clock, so a burst of pane output can
                // never become an animation source.
                let now = Instant::now();
                dirty |= state.local_status.refresh_clock(&clock);
                if let Some(cwd) = state.repository_target.clone()
                    && !repository_refreshing.contains(&cwd)
                    && repository_refreshed.as_ref().is_none_or(
                        |(refreshed_cwd, last): &(PathBuf, Instant)| {
                            refreshed_cwd != &cwd
                                || now.duration_since(*last) >= REPOSITORY_REFRESH_INTERVAL
                        },
                    )
                {
                    repository_refreshing.insert(cwd.clone());
                    start_repository_refresh(repository_tx.clone(), cwd);
                }
                if state.session_switcher_open()
                    && !catalog_refreshing
                    && catalog_refreshed
                        .is_none_or(|last| now.duration_since(last) >= SESSION_REFRESH_INTERVAL)
                {
                    start_catalog_refresh(catalog_tx.clone());
                    catalog_refreshing = true;
                }
                dirty |= state.tick_launch(now);
                dirty |= state.tick_toasts(now);
                dirty |= state.tick_resize(now);
                if dirty {
                    draw(&mut out, &mut renderer, &state, phase)?;
                    dirty = false;
                }
            }
            Step::Catalog(result) => {
                catalog_refreshing = false;
                catalog_refreshed = Some(Instant::now());
                if let Ok(entries) = result {
                    dirty |= state.refresh_sessions(entries, path);
                }
            }
            Step::Repository((cwd, repository)) => {
                repository_refreshing.remove(&cwd);
                if state.repository_target.as_ref() == Some(&cwd) {
                    repository_refreshed = Some((cwd.clone(), Instant::now()));
                }
                dirty |= state.apply_repository(cwd, repository);
            }
        }
    }
}

/// The live loop either leaves the terminal or hands an already-verified
/// attachment back to the outer switching loop.
enum LiveOutcome {
    Exit(i32),
    Switch {
        path: PathBuf,
        attached: Attached<UnixStream>,
    },
}

/// One selected branch of the live loop.
enum Step {
    /// A framed server message, or the connection's clean close.
    Server(Option<ServerMessage>),
    /// Bytes read from stdin.
    Input(Vec<u8>),
    /// Stdin reached EOF.
    InputClosed,
    /// The outer terminal resized.
    Resized(Size),
    /// The render clock advanced.
    Frame,
    /// One bounded, version-verified local catalog refresh completed.
    Catalog(CatalogResult),
    /// One client-local repository lookup completed away from the render loop.
    Repository(RepositoryResult),
}

fn start_catalog_refresh(tx: tokio::sync::mpsc::UnboundedSender<CatalogResult>) {
    tokio::spawn(async move {
        let _ = tx.send(discover_sessions().await);
    });
}

fn start_repository_refresh(
    tx: tokio::sync::mpsc::UnboundedSender<RepositoryResult>,
    cwd: PathBuf,
) {
    tokio::task::spawn_blocking(move || {
        let repository = repository_status(&cwd);
        let _ = tx.send((cwd, repository));
    });
}

/// The client-owned projection of the server's latest frame.
///
/// This is deliberately a cache, not another session model. The server decides
/// every pane rectangle and state transition; this structure only joins those
/// independent wire clocks into the grids, headers, cursor, and hit-test map
/// that one terminal frame needs.
struct LiveState {
    outer_size: Size,
    tabs: Vec<TabSummary>,
    status: Option<WorkspaceStatus>,
    layout: Option<LayoutSnapshot>,
    areas: BTreeMap<PaneId, PaneArea>,
    panes: BTreeMap<PaneId, PaneInfo>,
    attention: BTreeMap<PaneId, PaneAttention>,
    grids: BTreeMap<PaneId, Grid>,
    cursors: BTreeMap<PaneId, (Point, CursorShape, bool)>,
    modes: BTreeMap<PaneId, PaneModes>,
    copy_mode: Option<CopyModeState>,
    overlay: Option<Overlay>,
    queue: AttentionQueue,
    /// The bounded stack of transient notices floating in the upper-right safe
    /// area. Raised only by a new actionable attention projection, and advanced
    /// only by the render clock.
    toasts: ToastDeck,
    screen: ScreenLayout,
    /// The configured prefix's spelling, as the status row must draw it.
    prefix: String,
    /// Whether the router is holding a prefix and owns the next chord.
    prefix_pending: bool,
    /// The profiles this client's launcher can offer. Client-visible only: the
    /// daemon still resolves every identifier against its own table.
    profiles: Vec<Profile>,
    /// Independently inspected local sessions. Socket paths are retained only
    /// as typed switch targets; filenames never become display identity.
    sessions: Vec<SessionEntry>,
    /// The launch this client is still waiting on, or the refusal it is showing.
    launch: Option<LaunchNotice>,
    /// The fully validated visual preferences this client resolved locally.
    visual: VisualConfig,
    /// The palette resolved for this outer terminal's negotiated capabilities.
    theme: Theme,
    /// Capabilities are retained only to resolve a replacement theme on reload.
    caps: TermCaps,
    /// The newest daemon reload revision this client has attempted locally.
    config_revision: u64,
    /// The client-local card-08 treatment. The session still owns the ratio;
    /// this remembers only which drawn divider is active and its visible share.
    resize: Option<ResizeActivity>,
    /// Clock and repository facts owned by this terminal, never by the session.
    local_status: ClientStatus,
    /// The focused working directory whose repository answer is current or in flight.
    repository_target: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeSource {
    Keyboard,
    Mouse,
}

#[derive(Debug, Clone, Copy)]
struct ResizeActivity {
    divider: Divider,
    ratio: f32,
    source: ResizeSource,
    until: Option<Instant>,
}

impl LiveState {
    fn new(outer_size: Size, _session: SessionId, tabs: Vec<TabSummary>, prefix: String) -> Self {
        Self {
            outer_size,
            tabs,
            status: None,
            layout: None,
            areas: BTreeMap::new(),
            panes: BTreeMap::new(),
            attention: BTreeMap::new(),
            grids: BTreeMap::new(),
            cursors: BTreeMap::new(),
            modes: BTreeMap::new(),
            copy_mode: None,
            overlay: None,
            queue: AttentionQueue::new(),
            toasts: ToastDeck::new(TOAST_CAPACITY),
            screen: ScreenLayout::new(outer_size)
                .tab_row(0)
                .status_row(outer_size.rows.saturating_sub(1)),
            prefix,
            prefix_pending: false,
            profiles: Vec::new(),
            sessions: Vec::new(),
            launch: None,
            visual: VisualConfig::defaults(),
            theme: Theme::new(VisualConfig::defaults().theme, TermCaps::default()),
            caps: TermCaps::default(),
            config_revision: 0,
            resize: None,
            local_status: ClientStatus::default(),
            repository_target: None,
        }
    }

    /// Supplies this attached client's resolved visual preferences.
    ///
    /// This builder runs before the first frame, so the default fields in
    /// [`Self::new`] remain useful to small unit fixtures without becoming the
    /// production answer.
    fn preferences(mut self, caps: TermCaps, visual: VisualConfig) -> Self {
        self.caps = caps;
        self.theme = Theme::new(visual.theme, caps);
        self.visual = visual;
        self.toasts = ToastDeck::new(TOAST_CAPACITY).motion(MotionSettings::from_visual(visual));
        self
    }

    /// Whether a reload notification is newer than the one already observed.
    fn needs_reload(&self, revision: u64) -> bool {
        revision > self.config_revision
    }

    /// Applies one daemon reload revision to this client's local appearance.
    ///
    /// The caller supplies `None` when its own file did not produce a complete
    /// validated [`Config`]. The revision is still observed so a duplicate
    /// notification cannot make a later file edit apply without another
    /// successful daemon reload, while every prior visual field remains
    /// unchanged.
    fn reload_visual(&mut self, revision: u64, visual: Option<VisualConfig>) -> bool {
        if !self.needs_reload(revision) {
            return false;
        }
        self.config_revision = revision;
        let Some(visual) = visual else {
            return false;
        };
        if visual == self.visual {
            return false;
        }
        self.theme = Theme::new(visual.theme, self.caps);
        self.visual = visual;
        // A preference change is not a dismissal: the notices already up keep
        // their own deadlines and only their entrance follows the new setting.
        self.toasts.set_motion(MotionSettings::from_visual(visual));
        true
    }

    /// The single motion answer derived from the live visual preferences.
    fn motion_settings(&self) -> MotionSettings {
        MotionSettings::from_visual(self.visual)
    }

    /// Theme and focus treatment for the frame about to be composed.
    fn chrome_options(&self) -> ChromeOptions {
        ChromeOptions {
            dim_unfocused: self.visual.dim_unfocused,
            theme: self.theme,
        }
    }

    /// Supplies the profiles the launcher offers.
    ///
    /// Separate from [`Self::new`] because a client with none is still a valid
    /// client: an empty launcher lists nothing and confirms to nothing, which is
    /// exactly what a workspace with no configured profile should show.
    fn profiles(mut self, profiles: Vec<Profile>) -> Self {
        self.profiles = profiles;
        self
    }

    /// Replaces the verified catalog and refreshes an open switcher by socket,
    /// preserving its selected daemon when that daemon is still live.
    fn refresh_sessions(&mut self, entries: Vec<SessionCatalogEntry>, current: &Path) -> bool {
        let next = entries
            .into_iter()
            .map(|entry| {
                let attached = entry.socket == current;
                SessionEntry::new(entry.socket, entry.summary).attached(attached)
            })
            .collect::<Vec<_>>();
        if self.sessions == next {
            return false;
        }
        self.sessions = next;
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.refresh_sessions(self.sessions.clone());
        }
        true
    }

    /// Removes a switch target whose attach raced its disappearance.
    fn remove_session(&mut self, socket: &Path) {
        self.sessions.retain(|entry| entry.socket() != socket);
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.refresh_sessions(self.sessions.clone());
        }
    }

    fn session_switcher_open(&self) -> bool {
        self.overlay
            .as_ref()
            .is_some_and(|overlay| matches!(overlay.kind(), OverlayKind::Sessions(_)))
    }

    /// The status row's prefix field for the frame about to be drawn.
    ///
    /// The clues are offered while the workspace still has one pane, which is
    /// the shape a freshly created default workspace has, and whenever a prefix
    /// is pending — the moment the next chord matters is the moment to say what
    /// it can be.
    fn prefix_hint(&self) -> PrefixHint {
        PrefixHint::for_panes(self.prefix.as_str(), self.areas.len()).pending(self.prefix_pending)
    }

    /// Changes repository provenance only when the focused pane's reported
    /// directory changes. Clearing first prevents a slow old answer from being
    /// displayed for the new focus.
    fn refresh_repository_target(&mut self) -> Option<PathBuf> {
        let next = self.focused_cwd();
        if next == self.repository_target {
            return None;
        }
        self.repository_target = next.clone();
        self.local_status.set_repository(None);
        next
    }

    fn focused_cwd(&self) -> Option<PathBuf> {
        let focused = self.layout.as_ref()?.focused?;
        self.panes
            .get(&focused)
            .map(|pane| PathBuf::from(&pane.cwd))
    }

    /// Accepts only the answer still naming the focused pane's directory.
    fn apply_repository(&mut self, cwd: PathBuf, repository: Option<RepositoryStatus>) -> bool {
        if self.repository_target.as_ref() != Some(&cwd)
            || self.focused_cwd().as_ref() != Some(&cwd)
        {
            return false;
        }
        self.local_status.set_repository(repository)
    }

    /// Applies one server clock tick against the current instant.
    ///
    /// The client's own transient surfaces are the only reason a wire message
    /// needs a clock at all, so the instant is passed in by
    /// [`Self::apply_at`] and this remains the convenient call.
    #[cfg(test)]
    fn apply(&mut self, message: ServerMessage) -> Result<bool, RenderError> {
        self.apply_at(message, Instant::now())
    }

    /// Applies one server clock tick and reports whether it changes the frame.
    fn apply_at(&mut self, message: ServerMessage, now: Instant) -> Result<bool, RenderError> {
        match message {
            ServerMessage::Damage { pane, rows } => {
                let Some(grid) = self.grids.get_mut(&pane) else {
                    // A peer is allowed to resend an already-obsolete damage
                    // frame while a newer layout resync is in flight. There is
                    // no pane left to draw it into, so dropping it is safer than
                    // associating it with a successor.
                    return Ok(false);
                };
                for row in &rows {
                    grid.apply(row)?;
                }
                Ok(true)
            }
            ServerMessage::CursorMoved {
                pane,
                pos,
                shape,
                visible,
            } => {
                self.cursors.insert(pane, (pos, shape, visible));
                Ok(true)
            }
            ServerMessage::Layout(layout) => {
                self.set_layout(layout);
                self.refresh_resize_ratio(now);
                Ok(true)
            }
            ServerMessage::Panes(panes) => {
                // The pane the client asked for arriving is the launch
                // answering for itself, so the notice has nothing left to say.
                if self
                    .launch
                    .as_ref()
                    .is_some_and(|notice| !notice.refused() && notice.arrived(&panes))
                {
                    self.launch = None;
                }
                self.panes = panes.into_iter().map(|pane| (pane.pane, pane)).collect();
                self.rebuild_queue();
                Ok(true)
            }
            ServerMessage::Attention(attention) => {
                let next: BTreeMap<PaneId, PaneAttention> = attention
                    .into_iter()
                    .map(|state| (state.pane, state))
                    .collect();
                // Diffed against the projection still cached, because a toast is
                // raised by an *event* and the wire carries state.
                self.raise_toasts(&next, now);
                self.attention = next;
                self.rebuild_queue();
                Ok(true)
            }
            ServerMessage::CopyMode(copy_mode) => {
                self.copy_mode = copy_mode;
                Ok(true)
            }
            ServerMessage::Modes { pane, modes } => {
                self.modes.insert(pane, modes);
                Ok(false)
            }
            ServerMessage::Tabs(tabs) => {
                self.tabs = tabs;
                Ok(true)
            }
            // Reload revisions are handled by the live loop because only it
            // owns the local reload callback. A summary is not part of an
            // attached stream at all — it answers an inspection on a connection
            // that never became a client.
            ServerMessage::WorkspaceStatus(status) => {
                if self.status.as_ref() == Some(&status) {
                    return Ok(false);
                }
                self.status = Some(status);
                Ok(true)
            }
            ServerMessage::SessionSummary(_)
            | ServerMessage::ConfigReloaded { .. }
            | ServerMessage::Hello { .. }
            | ServerMessage::Refused { .. }
            | ServerMessage::Effect { .. }
            | ServerMessage::Bell(_)
            | ServerMessage::Detached
            | ServerMessage::Exit(_) => Ok(false),
        }
    }

    /// Changes the outer frame without pretending the session geometry changed.
    fn set_outer_size(&mut self, outer_size: Size) {
        self.outer_size = outer_size;
        if let Some(layout) = self.layout.clone() {
            self.set_layout(layout);
        } else {
            self.screen = ScreenLayout::new(outer_size)
                .tab_row(0)
                .status_row(outer_size.rows.saturating_sub(1));
        }
    }

    /// The layout change a freshly received message makes legible.
    ///
    /// Only resolved layout changes start motion. Damage, attention, and copy
    /// state all arrive on data clocks and must never turn a busy child into an
    /// animation source.
    fn transition_for(&self, message: &ServerMessage) -> Option<MotionKind> {
        let ServerMessage::Layout(next) = message else {
            return None;
        };
        let previous = self.layout.as_ref()?;
        if previous.panes.len() < next.panes.len() {
            return Some(MotionKind::Split);
        }
        if previous.panes.len() > next.panes.len() {
            return Some(MotionKind::Close);
        }
        (previous.focused != next.focused).then_some(MotionKind::Focus)
    }

    /// Rebuilds client-owned areas from the server's one resolved layout pass.
    fn set_layout(&mut self, layout: LayoutSnapshot) {
        let mut areas = BTreeMap::new();
        let mut live = BTreeMap::new();
        for rect in &layout.panes {
            // The server was given the area between the fixed tab and status
            // rows. Its framed geometry already excludes every pane edge from
            // the child grid, so only the tab-row offset is added here.
            let area = PaneArea::new(rect.pane, rect.x, rect.y.saturating_add(1), rect.size);
            let grid = self.grids.remove(&rect.pane).map_or_else(
                || Grid::new(rect.size),
                |mut grid| {
                    if grid.size() != rect.size {
                        grid.resize(rect.size);
                    }
                    grid
                },
            );
            areas.insert(rect.pane, area);
            live.insert(rect.pane, grid);
        }
        self.grids = live;
        self.areas = areas;
        self.cursors.retain(|pane, _| self.areas.contains_key(pane));
        self.modes.retain(|pane, _| self.areas.contains_key(pane));
        self.layout = Some(layout.clone());

        let mut screen = ScreenLayout::new(self.outer_size)
            .tab_row(0)
            .status_row(self.outer_size.rows.saturating_sub(1))
            .focus(layout.focused);
        for area in self.areas.values().copied() {
            screen = screen.pane(area);
        }
        self.screen = screen;
    }

    /// The pane modes that decide whether a mouse report belongs to the app.
    fn focused_modes(&self) -> PaneModes {
        self.layout
            .as_ref()
            .and_then(|layout| layout.focused)
            .and_then(|pane| self.modes.get(&pane).copied())
            .unwrap_or_default()
    }

    /// Resolves a prefixed arrow against the focused pane's drawn edge.
    ///
    /// The divider is found from `ScreenLayout`, so an outer edge has no action
    /// and the highlighted cells are exactly those mouse dragging would claim.
    fn keyboard_resize(&mut self, side: Side, now: Instant) -> Option<Action> {
        let focused = self.screen.focused()?;
        let divider = self.screen.divider_toward(focused, side)?;
        let delta = match side {
            Side::Left | Side::Up => -1,
            Side::Right | Side::Down => 1,
        };
        self.begin_resize(divider, ResizeSource::Keyboard, now);
        Some(Action::ResizePane {
            pane: divider.pane,
            dir: divider.dir,
            delta,
        })
    }

    fn begin_resize(&mut self, divider: Divider, source: ResizeSource, now: Instant) {
        let ratio = self.screen.divider_ratio(divider).unwrap_or(0.5);
        self.resize = Some(ResizeActivity {
            divider,
            ratio,
            source,
            until: (source == ResizeSource::Keyboard).then_some(now + KEYBOARD_RESIZE_LINGER),
        });
    }

    fn sync_mouse_resize(&mut self, divider: Option<Divider>, now: Instant) {
        match divider {
            Some(divider) => self.begin_resize(divider, ResizeSource::Mouse, now),
            None if self
                .resize
                .is_some_and(|resize| resize.source == ResizeSource::Mouse) =>
            {
                self.resize = None;
            }
            None => {}
        }
    }

    fn refresh_resize_ratio(&mut self, now: Instant) {
        let Some(resize) = self.resize.as_mut() else {
            return;
        };
        if let Some(ratio) = self.screen.divider_ratio(resize.divider) {
            resize.ratio = ratio;
            if resize.source == ResizeSource::Keyboard {
                resize.until = Some(now + KEYBOARD_RESIZE_LINGER);
            }
        } else {
            self.resize = None;
        }
    }

    fn clear_resize(&mut self) -> bool {
        self.resize.take().is_some()
    }

    fn tick_resize(&mut self, now: Instant) -> bool {
        if self
            .resize
            .and_then(|resize| resize.until)
            .is_some_and(|until| now >= until)
        {
            return self.clear_resize();
        }
        false
    }

    /// Draws an attention queue only from the complete server projection.
    ///
    /// An open queue overlay is refreshed from the same pass, because the server
    /// owns both the states it lists and the acknowledgment that clears one: a
    /// row the user just acknowledged leaves when the projection saying so
    /// arrives, not when the client decided to hide it.
    fn rebuild_queue(&mut self) {
        self.queue = AttentionQueue::new();
        for (index, pane) in self.panes.values().enumerate() {
            let Some(state) = self.attention.get(&pane.pane) else {
                continue;
            };
            if !state.acknowledged {
                self.queue.record(
                    u16::try_from(index + 1).unwrap_or(u16::MAX),
                    &pane.name,
                    attention(state.state),
                );
            }
        }
        let entries = self.attention_entries();
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.refresh_attention(entries);
        }
        self.toasts
            .retain_within(u16::try_from(self.panes.len()).unwrap_or(u16::MAX));
    }

    /// Raises a toast for each pane whose actionable state is *new*.
    ///
    /// The wire carries state, not events, so the event is the difference
    /// between the projection about to be cached and the one already held: a
    /// pane that was not actionable and now is, or one whose actionable state
    /// changed, or one whose acknowledgment was cleared. An identical projection
    /// resent — which a client must tolerate — raises nothing at all, while a
    /// pane that genuinely raises the same state again coalesces into one notice
    /// with a growing count.
    ///
    /// Only two things take a notice away early: an acknowledgment, because the
    /// user has already seen the event wherever they acknowledged it, and the
    /// pane leaving the workspace. A pane settling back to quiet does *not* — the
    /// notice is a record of something that happened, and yanking it off the
    /// screen the moment the child calms down is how a user misses it. Its own
    /// deadline is what clears it.
    fn raise_toasts(&mut self, next: &BTreeMap<PaneId, PaneAttention>, now: Instant) {
        let panes: Vec<(u16, PaneId, String)> = self
            .panes
            .values()
            .enumerate()
            .map(|(position, info)| {
                (
                    u16::try_from(position + 1).unwrap_or(u16::MAX),
                    info.pane,
                    info.name.clone(),
                )
            })
            .collect();
        for (index, pane, name) in panes {
            let Some(state) = next.get(&pane) else {
                self.toasts.dismiss(index);
                continue;
            };
            if state.acknowledged {
                self.toasts.dismiss(index);
                continue;
            }
            if !attention(state.state).is_actionable() {
                continue;
            }
            let changed = self
                .attention
                .get(&pane)
                .is_none_or(|previous| previous.acknowledged || previous.state != state.state);
            if changed {
                self.toasts.push(index, name, attention(state.state), now);
            }
        }
    }

    /// Advances the toast stack's own clock, reporting whether the frame changed.
    ///
    /// Called from the render tick and from nowhere else, which is what keeps a
    /// notice's entrance and expiry off a pane's output clock.
    fn tick_toasts(&mut self, now: Instant) -> bool {
        self.toasts.tick(now)
    }

    /// The bounded toast stack, drawn in the upper-right safe area.
    ///
    /// The focused cursor's row is handed to the placement so a notice can pass
    /// in front of a harness without ever covering the line being typed into,
    /// and each toast is drawn at whatever step of its entrance it has reached.
    fn toast_spans(&self) -> Vec<crate::renderer::Span> {
        if self.toasts.is_empty() {
            return Vec::new();
        }
        let avoid = self.cursor().map(|cursor| cursor.pos.row);
        toast_rows(self.outer_size, self.toasts.len(), avoid)
            .into_iter()
            .zip(self.toasts.toasts())
            .map(|(row, toast)| {
                let span = toast_stack_span(self.outer_size, row, toast, self.theme);
                match toast.phase() {
                    Some(phase) => phase_span(&span, phase, self.theme.color(ThemeToken::Frame)),
                    None => span,
                }
            })
            .collect()
    }

    /// The queue's rows paired with the panes they name.
    ///
    /// The queue is keyed by the position a user refers to a pane by, which is
    /// the right key for coalescing and the wrong one to put on the wire: a
    /// closing pane renumbers its neighbours. Resolving the position back to the
    /// `PaneId` here — through the very map the numbering came from — is what
    /// lets a row act on the pane the user actually saw. A row whose pane has
    /// since gone is dropped rather than aimed at its successor.
    fn attention_entries(&self) -> Vec<AttentionEntry> {
        self.queue
            .entries()
            .iter()
            .filter_map(|entry| {
                let position = usize::from(entry.index).checked_sub(1)?;
                let info = self.panes.values().nth(position)?;
                Some(AttentionEntry::new(info.pane, entry))
            })
            .collect()
    }

    /// Composes the frame from the exact areas the mouse hit tester uses.
    fn spans(&self) -> Vec<crate::renderer::Span> {
        let Some(layout) = &self.layout else {
            return Vec::new();
        };
        let panes = layout
            .panes
            .iter()
            .enumerate()
            .filter_map(|(index, rect)| {
                let area = *self.areas.get(&rect.pane)?;
                let grid = self.grids.get(&rect.pane)?;
                let meta = self.panes.get(&rect.pane);
                let title = meta.map_or_else(|| "pane".to_owned(), |meta| meta.name.clone());
                let mut header =
                    PaneChrome::new(u16::try_from(index + 1).unwrap_or(u16::MAX), title)
                        .attention(
                            self.attention
                                .get(&rect.pane)
                                .map_or(Attention::Unknown, |state| attention(state.state)),
                        )
                        .focused(layout.focused == Some(rect.pane))
                        .zoomed(layout.zoomed == Some(rect.pane));
                if let Some(task) = meta.and_then(|meta| meta.task.clone()) {
                    header = header.task(task);
                }
                Some(FramePane::new(area, grid, header))
            })
            .collect::<Vec<_>>();
        // The badge and the client count are the daemon's projection; the pane
        // count is the layout this client is already drawing. A field the
        // daemon has not published yet stays absent rather than invented.
        let mut bar = TabBar::new(&self.tabs).panes(panes.len());
        if let Some(status) = &self.status {
            bar = bar.clients(status.clients);
            if !status.name.is_empty() {
                bar = bar.session(&status.name);
            }
        }
        let hint = self.prefix_hint();
        let mut status_bar = StatusBar::new(&self.tabs, &self.queue, &hint);
        if let Some(status) = &self.status {
            status_bar = status_bar.clients(status.clients);
            if !status.name.is_empty() {
                status_bar = status_bar.session(&status.name);
            }
        }
        if let Some(repository) = self.local_status.repository() {
            status_bar = status_bar.repository(repository);
        }
        if let Some(clock) = self.local_status.clock() {
            status_bar = status_bar.clock(clock);
        }
        let mut spans = compose_frame(
            self.outer_size,
            bar,
            &panes,
            status_bar,
            self.chrome_options(),
        );
        if let Some(copy_mode) = &self.copy_mode {
            if let (Some(area), Some(grid)) = (
                self.areas.get(&copy_mode.pane),
                self.grids.get(&copy_mode.pane),
            ) {
                spans.extend(highlight_spans(
                    Point::new(area.x, area.y),
                    grid,
                    copy_mode,
                    self.theme,
                ));
                spans.push(copy_status_span(
                    Point::new(0, self.outer_size.rows.saturating_sub(1)),
                    copy_mode,
                    self.outer_size.cols,
                    self.theme,
                ));
            }
        }
        // Last, so a launch this client is still waiting on — or one the
        // workspace never made — takes the status row from whatever was
        // otherwise on it. It is transient, and it is the more urgent thing.
        if let Some(notice) = &self.launch {
            spans.push(launch_notice_span(
                Point::new(0, self.outer_size.rows.saturating_sub(1)),
                notice,
                self.outer_size.cols,
                self.theme,
            ));
        }
        if let Some(resize) = self.resize {
            let points = self.screen.divider_points(resize.divider);
            spans.extend(resize_affordance_spans(
                &points,
                resize.divider.dir,
                resize.ratio,
                self.outer_size,
                self.theme,
            ));
        }
        spans
    }

    /// Composes normal pane contents plus client-owned visual layers.
    fn frame(&self) -> RenderFrame {
        let mut base = self.spans();
        let mut chrome: Vec<bool> = base.iter().map(|span| !self.is_body_span(span)).collect();
        // A toast floats *over* a pane, so it is client chrome wherever it lands
        // rather than something the geometry test could tell apart from pane
        // content. It still dims under an open overlay like the rest of the
        // frame, which is why it is layered here and not painted afterwards.
        let toasts = self.toast_spans();
        chrome.extend(std::iter::repeat_n(true, toasts.len()));
        base.extend(toasts);
        let mut spans = if self.overlay.is_some() {
            base.iter()
                .map(|span| backdrop_span(span.at, &span.cells, self.theme))
                .collect()
        } else {
            base
        };

        if let Some(overlay) = &self.overlay {
            let size = self.overlay_size(overlay);
            let at = Point::new(
                self.outer_size.cols.saturating_sub(size.cols) / 2,
                self.outer_size.rows.saturating_sub(size.rows) / 2,
            );
            let overlays = overlay_spans(at, overlay, size, self.theme);
            chrome.extend(std::iter::repeat_n(true, overlays.len()));
            spans.extend(overlays);
        }

        RenderFrame { spans, chrome }
    }

    /// Starts a client-local overlay after an otherwise unbound prefix chord.
    ///
    /// These shortcuts never cross the wire. The help surface is read from the
    /// very keymap the router resolved this chord with, the session switcher
    /// lists only independently inspected local daemons, pane details are
    /// built solely from the server metadata and attention already cached
    /// locally, and the launcher offers only the configured profiles this client
    /// resolved. A chord only reaches here when the keymap left it unbound, which
    /// is why a user who binds `?` keeps their binding — and why ordinary text
    /// typed for a shell can never open any of them.
    fn open_overlay(&mut self, chord: &[u8], keymap: &Keymap) -> bool {
        let key = (chord.len() == 1).then(|| char::from(chord[0]));
        let next = match key {
            Some(HELP_KEY) => Some(Overlay::palette(keymap)),
            Some(key) => ClientSurface::from_key(key).and_then(|surface| self.surface(surface)),
            None => None,
        };
        if let Some(next) = next {
            self.overlay = Some(next);
            true
        } else {
            false
        }
    }

    /// Builds one client-local surface out of the state this client has cached.
    ///
    /// The one place a [`ClientSurface`] becomes an overlay, so a chord and a
    /// confirmed palette row reach exactly the same surface: the session
    /// switcher lists only independently inspected local daemons, pane
    /// details are built solely from server metadata and attention already
    /// cached locally, and the launcher offers only the configured profiles
    /// this client resolved.
    fn surface(&self, surface: ClientSurface) -> Option<Overlay> {
        match surface {
            ClientSurface::Launcher => Some(Overlay::launcher(&self.profiles)),
            ClientSurface::Sessions => Some(Overlay::sessions(self.sessions.clone())),
            ClientSurface::Attention => Some(Overlay::attention(self.attention_entries())),
            // The only surface that can fail to open: with no focused pane
            // there is nothing to describe, and a blank details box would be a
            // claim about a pane that is not there.
            ClientSurface::Details => self
                .layout
                .as_ref()
                .and_then(|layout| layout.focused)
                .and_then(|pane| {
                    self.panes.get(&pane).map(|info| {
                        Overlay::details(PaneDetails::from_info(
                            info,
                            self.attention
                                .get(&pane)
                                .map_or(Attention::Unknown, |state| attention(state.state)),
                        ))
                    })
                }),
        }
    }

    /// Applies an open overlay's own keyboard vocabulary.
    ///
    /// Every key an open overlay understands is consumed here and never reaches
    /// a pane, including the confirmation that closes it. Three vocabularies
    /// meet here because three surfaces genuinely differ: the queue is decoded
    /// with [`queue_action`] for the verb the shared vocabulary has no word for
    /// — acknowledging a row is not confirming it — the palette with
    /// [`palette_actions`] because a printable key there is query text rather
    /// than a command, and everything else with [`overlay_action`]. A launch
    /// and the session actions a row produces are handed back rather than
    /// swallowed, because only the caller holds the connection.
    fn apply_overlay_keys(&mut self, keys: &[u8]) -> OverlayKeys {
        let Some(outcome) = self.overlay_outcome(keys) else {
            return OverlayKeys::Ignored;
        };
        match outcome {
            OverlayOutcome::Open => OverlayKeys::Consumed,
            OverlayOutcome::Dismissed => {
                self.overlay = None;
                OverlayKeys::Consumed
            }
            OverlayOutcome::SwitchSession(socket) => OverlayKeys::SwitchSession(socket),
            OverlayOutcome::Launch(request) => {
                self.overlay = None;
                OverlayKeys::Launch(request)
            }
            // A palette row that named a keymap action is the bound chord by
            // another route, so it leaves exactly as that chord would have.
            OverlayOutcome::RunAction(action) => {
                self.overlay = None;
                OverlayKeys::Command(action)
            }
            // A palette row that named a client surface never reaches the wire
            // at all: it replaces the palette with the surface it named, and a
            // surface this client cannot build leaves the palette up rather
            // than closing onto nothing.
            OverlayOutcome::OpenSurface(surface) => {
                if let Some(next) = self.surface(surface) {
                    self.overlay = Some(next);
                }
                OverlayKeys::Consumed
            }
            // Focusing the pane a row names is the queue's whole purpose, so it
            // closes; acknowledging leaves the surface up, because the row goes
            // away when the server's projection says it has and a user usually
            // has more than one to clear.
            OverlayOutcome::FocusPane(pane) => {
                self.overlay = None;
                OverlayKeys::Command(Action::FocusPane(pane))
            }
            OverlayOutcome::Acknowledge(pane) => {
                OverlayKeys::Command(Action::AcknowledgeAttention(pane))
            }
        }
    }

    /// What one chunk of keyboard bytes did to the open overlay, if anything.
    ///
    /// `None` only when there is no overlay or the keys spell nothing that
    /// surface knows. The palette answers even to keys it cannot use, because
    /// its query owns every printable byte and a fall-through would be the one
    /// path by which a search term could reach a child.
    fn overlay_outcome(&mut self, keys: &[u8]) -> Option<OverlayOutcome> {
        let overlay = self.overlay.as_mut()?;
        if matches!(overlay.kind(), OverlayKind::Attention(_)) {
            return Some(overlay.apply_queue(queue_action(keys)?));
        }
        if matches!(overlay.kind(), OverlayKind::Palette(_)) {
            // A run of typed bytes is a run of edits, and the first one that is
            // not `Open` ends the palette — nothing after a confirmation
            // belongs to a surface that has closed.
            let mut outcome = OverlayOutcome::Open;
            for action in palette_actions(keys) {
                outcome = overlay.apply_palette(action);
                if !matches!(outcome, OverlayOutcome::Open) {
                    break;
                }
            }
            return Some(outcome);
        }
        Some(overlay.apply(overlay_action(keys)?))
    }

    /// Records a launch this client has just put on the wire.
    ///
    /// The pane set is captured *here*, from the client's own cache, so the
    /// notice can tell the pane this request produces from one that already
    /// existed rather than from anything it reads off a grid.
    fn sent_launch(&mut self, request: &LaunchRequest, now: Instant) {
        self.launch = Some(LaunchNotice::sent(
            request,
            self.panes.keys().copied().collect::<BTreeSet<_>>(),
            now,
        ));
    }

    /// Advances the launch notice's clock, reporting whether the frame changed.
    fn tick_launch(&mut self, now: Instant) -> bool {
        let Some(notice) = self.launch.as_mut() else {
            return false;
        };
        if notice.settle(now) {
            return true;
        }
        if notice.finished(now) {
            self.launch = None;
            return true;
        }
        false
    }

    /// Whether a span is pane content or a client-owned visual layer.
    fn is_body_span(&self, span: &crate::renderer::Span) -> bool {
        self.areas.values().any(|area| {
            let end = span
                .at
                .col
                .saturating_add(u16::try_from(span.cells.len()).unwrap_or(u16::MAX));
            span.at.row >= area.y
                && span.at.row < area.y.saturating_add(area.size.rows)
                && span.at.col >= area.x
                && end <= area.x.saturating_add(area.size.cols)
        })
    }

    /// The box an overlay is drawn in: as tall as its list plus its chrome,
    /// within bounds.
    ///
    /// The cap is generous enough for the whole command list on an ordinary
    /// terminal — a user who pressed `?` because they do not know the keys is
    /// the last person who should have to scroll to find `detach` — and the
    /// window still follows the cursor on a short one.
    fn overlay_size(&self, overlay: &Overlay) -> Size {
        let rows = u16::try_from(overlay.preferred_rows())
            .unwrap_or(u16::MAX)
            .clamp(2, 20)
            .min(self.outer_size.rows);
        Size::new(self.outer_size.cols.min(60), rows)
    }

    /// The focused pane's cursor, translated into outer-terminal coordinates.
    fn cursor(&self) -> Option<Cursor> {
        let pane = self.layout.as_ref()?.focused?;
        let area = self.areas.get(&pane)?;
        let (pos, shape, visible) = self.cursors.get(&pane).copied()?;
        if !visible || pos.col >= area.size.cols || pos.row >= area.size.rows {
            return None;
        }
        Some(Cursor::new(
            Point::new(
                area.x.saturating_add(pos.col),
                area.y.saturating_add(pos.row),
            ),
            shape,
        ))
    }
}

/// What an open overlay did with one chunk of keyboard bytes.
///
/// A three-way answer rather than a bool because "the overlay consumed it" and
/// "the overlay consumed it *and* the user launched something" are different
/// facts, and only the caller holds the connection to act on the second.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OverlayKeys {
    /// Not an overlay action, or no overlay open. The keys are still the
    /// router's to deal with.
    Ignored,
    /// The overlay handled it, and nothing crosses the wire.
    Consumed,
    /// The overlay closed on a confirmed profile.
    Launch(LaunchRequest),
    /// The overlay produced one typed session action to send.
    Command(Action),
    /// The switcher confirmed one independently verified daemon socket.
    SwitchSession(PathBuf),
}

#[cfg(test)]
impl OverlayKeys {
    /// Whether these keys were the overlay's and must not reach a pane.
    const fn consumed(&self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

/// One frame, with the server-owned pane cells marked apart from chrome.
struct RenderFrame {
    spans: Vec<crate::renderer::Span>,
    chrome: Vec<bool>,
}

/// Turns the wire's explicit attention state into the chrome vocabulary.
const fn attention(state: AttentionState) -> Attention {
    match state {
        AttentionState::Unknown => Attention::Unknown,
        AttentionState::Working => Attention::Working,
        AttentionState::NeedsInput => Attention::NeedsInput,
        AttentionState::Ready => Attention::Ready,
        AttentionState::Failed => Attention::Failed,
        AttentionState::Quiet => Attention::Quiet,
    }
}

/// Writes one complete composed frame.
fn draw(
    out: &mut io::Stdout,
    renderer: &mut Renderer,
    state: &LiveState,
    phase: Option<Phase>,
) -> Result<(), AttachRunError> {
    let frame = state.frame();
    let rendered = match phase {
        Some(phase) => renderer.render_layered_transition(
            &frame.spans,
            &frame.chrome,
            phase,
            state.theme.color(ThemeToken::Frame),
            state.cursor(),
        ),
        None => renderer.render_spans(&frame.spans, state.cursor()),
    };
    out.write_all(rendered).map_err(AttachRunError::Output)?;
    out.flush().map_err(AttachRunError::Output)
}

/// Sends one decoded event through the correct client or application path.
async fn route(
    attached: &mut Attached<UnixStream>,
    state: &mut LiveState,
    chrome: &mut ChromeMouse,
    keys: &mut KeyRouter,
    decoder: &mut InputDecoder,
    bytes: Vec<u8>,
) -> Result<RouteOutcome, AttachRunError> {
    for event in decoder.feed(&bytes) {
        match event {
            InputEvent::Keys(bytes) => {
                let outcome = route_keys(attached, keys, state, bytes).await?;
                if !matches!(outcome, RouteOutcome::Continue) {
                    return Ok(outcome);
                }
            }
            InputEvent::Paste(text) => {
                state.clear_resize();
                attached.send_paste(text).await.map_err(AttachError::from)?
            }
            InputEvent::Focus(focused) => {
                state.clear_resize();
                if !focused {
                    keys.reset();
                }
                attached
                    .send_focus(focused)
                    .await
                    .map_err(AttachError::from)?;
            }
            InputEvent::Mouse(report) => {
                state.clear_resize();
                if chrome.is_dragging() {
                    let action = chrome.feed(&state.screen, ChromeTarget::Gutter, &report);
                    state.sync_mouse_resize(chrome.active_divider(), Instant::now());
                    if let Some(action) = action {
                        if apply_chrome(
                            attached,
                            action,
                            state.copy_mode.as_ref().map(|copy_mode| copy_mode.pane),
                        )
                        .await?
                        {
                            return Ok(RouteOutcome::Detach);
                        }
                    }
                    continue;
                }
                match route_mouse(&state.screen, state.focused_modes(), &report) {
                    MouseRoute::Application(event) => {
                        attached
                            .send_mouse(event)
                            .await
                            .map_err(AttachError::from)?;
                    }
                    MouseRoute::Chrome(target) => {
                        let action = chrome.feed(&state.screen, target, &report);
                        state.sync_mouse_resize(chrome.active_divider(), Instant::now());
                        if let Some(action) = action {
                            if apply_chrome(
                                attached,
                                action,
                                state.copy_mode.as_ref().map(|copy_mode| copy_mode.pane),
                            )
                            .await?
                            {
                                return Ok(RouteOutcome::Detach);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(RouteOutcome::Continue)
}

/// Resolves prefix chords and sends the resulting application bytes or actions.
async fn route_keys(
    attached: &mut Attached<UnixStream>,
    keys: &mut KeyRouter,
    state: &mut LiveState,
    bytes: Vec<u8>,
) -> Result<RouteOutcome, AttachRunError> {
    state.clear_resize();
    if state.overlay.is_some() {
        keys.reset();
        // Everything that leaves the client from an overlay is typed: a profile
        // *identifier* the daemon resolves against its own table, a keymap
        // action the palette named, and the focus or acknowledgment a queue row
        // names by pane. Nothing a user typed reaches the wire, and no
        // keystroke ever does.
        match state.apply_overlay_keys(&bytes) {
            OverlayKeys::Launch(request) => {
                attached
                    .send_command(Action::LaunchProfile(request.profile().as_str().to_owned()))
                    .await
                    .map_err(AttachError::from)?;
                state.sent_launch(&request, Instant::now());
            }
            OverlayKeys::Command(action) => {
                // Detaching from the palette is still detaching: the caller has
                // to leave the loop, or the client would send the command and
                // then keep rendering a session it has left.
                let detach = action == Action::DetachClient;
                attached
                    .send_command(action)
                    .await
                    .map_err(AttachError::from)?;
                return Ok(if detach {
                    RouteOutcome::Detach
                } else {
                    RouteOutcome::Continue
                });
            }
            OverlayKeys::SwitchSession(socket) => return Ok(RouteOutcome::Switch(socket)),
            OverlayKeys::Ignored | OverlayKeys::Consumed => {}
        }
        return Ok(RouteOutcome::Continue);
    }

    for route in keys.feed(&bytes) {
        match route {
            KeyRoute::Pane(bytes) => attached
                .send_input(bytes)
                .await
                .map_err(AttachError::from)?,
            KeyRoute::Command(Action::DetachClient) => {
                attached
                    .send_command(Action::DetachClient)
                    .await
                    .map_err(AttachError::from)?;
                return Ok(RouteOutcome::Detach);
            }
            KeyRoute::Command(action) => attached
                .send_command(action)
                .await
                .map_err(AttachError::from)?,
            KeyRoute::Resize(side) => {
                if let Some(action) = state.keyboard_resize(side, Instant::now()) {
                    attached
                        .send_command(action)
                        .await
                        .map_err(AttachError::from)?;
                }
            }
            KeyRoute::Unbound(chord) => {
                let _ = state.open_overlay(&chord, keys.keymap());
            }
            KeyRoute::Pending => {}
        }
    }
    Ok(RouteOutcome::Continue)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteOutcome {
    Continue,
    Detach,
    Switch(PathBuf),
}

/// Sends chrome gestures through the same command vocabulary as the keyboard.
async fn apply_chrome(
    attached: &mut Attached<UnixStream>,
    action: ChromeAction,
    copy_mode: Option<PaneId>,
) -> Result<bool, AttachRunError> {
    let mut detach = false;
    for command in action.commands(copy_mode) {
        if command == Action::DetachClient {
            detach = true;
        }
        attached
            .send_command(command)
            .await
            .map_err(AttachError::from)?;
    }
    Ok(detach)
}

/// Asks the outer terminal to enable only the reporting modes it negotiated.
fn enable_modes(modes: OuterModes) -> Result<(), AttachRunError> {
    let mut out = io::stdout();
    out.write_all(&modes.enable())
        .map_err(AttachRunError::Output)?;
    out.flush().map_err(AttachRunError::Output)
}

/// Reads stdin without changing the shell's shared descriptor flags.
fn spawn_input_reader() -> tokio::sync::mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0_u8; INPUT_BUF_LEN];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(read) if tx.send(buf[..read].to_vec()).is_err() => break,
                Ok(_) => {}
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
    use cloo_proto::{Cell, CellAttrs, CopySelection, PaneRect, RowUpdate, ScrollPoint, TabId};
    use tokio::io::duplex;

    /// The server half of a handshake, scripted by the test.
    async fn reply_with(server: tokio::io::DuplexStream, reply: Option<ServerMessage>) {
        let mut conn = FrameStream::new(server);
        let attach = conn
            .recv::<ClientMessage>()
            .await
            .expect("the attach arrives");
        assert!(
            matches!(attach, Some(ClientMessage::Attach { .. })),
            "the first frame must be an attach, got {attach:?}"
        );
        if let Some(reply) = reply {
            conn.send(&reply).await.expect("the reply sends");
            // Hold the connection open so a clean close is not mistaken for the
            // reply itself.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn hello() -> ServerMessage {
        ServerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            session: SessionId::new(7),
            tabs: vec![TabSummary {
                tab: TabId::new(1),
                title: "shell".into(),
                active: true,
            }],
            size: Size::new(80, 24),
        }
    }

    #[tokio::test]
    async fn a_hello_completes_the_attach() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(reply_with(server, Some(hello())));

        let attached = handshake(
            FrameStream::new(client),
            Size::new(100, 30),
            TermCaps::default(),
            None,
        )
        .await
        .expect("a matching hello attaches");

        assert_eq!(attached.session(), SessionId::new(7));
        assert_eq!(attached.size(), Size::new(80, 24));
        assert_eq!(attached.tabs().len(), 1);
        scripted.await.expect("the scripted server finishes");
    }

    #[tokio::test]
    async fn session_catalog_inspection_sends_only_the_versioned_read_only_request() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(async move {
            let mut conn = FrameStream::new(server);
            let request = conn
                .recv::<ClientMessage>()
                .await
                .expect("the inspection arrives");
            assert_eq!(
                request,
                Some(ClientMessage::InspectSession {
                    protocol_version: PROTOCOL_VERSION,
                }),
                "inspection must carry no size, capabilities, or attachment"
            );
            conn.send(&ServerMessage::SessionSummary(SessionSummary {
                name: "agents".to_owned(),
                tabs: 2,
                panes: 4,
                clients: 1,
                uptime_secs: 9,
            }))
            .await
            .expect("the summary sends");
        });

        let summary = inspect_handshake(FrameStream::new(client))
            .await
            .expect("a summary completes inspection");
        assert_eq!(summary.name, "agents");
        assert_eq!((summary.tabs, summary.panes, summary.clients), (2, 4, 1));
        scripted.await.expect("the scripted peer finishes");
    }

    #[tokio::test]
    async fn session_catalog_inspection_rejects_attach_and_grid_replies() {
        for reply in [
            hello(),
            ServerMessage::Damage {
                pane: PaneId::new(1),
                rows: Vec::new(),
            },
        ] {
            let (client, server) = duplex(4096);
            let scripted = tokio::spawn(async move {
                let mut conn = FrameStream::new(server);
                let _request = conn
                    .recv::<ClientMessage>()
                    .await
                    .expect("the inspection arrives");
                conn.send(&reply).await.expect("the wrong reply sends");
            });

            let err = inspect_handshake(FrameStream::new(client))
                .await
                .expect_err("inspection must never become an attach or grid reader");
            assert!(matches!(err, InspectError::UnexpectedReply), "got {err}");
            scripted.await.expect("the scripted peer finishes");
        }
    }

    #[tokio::test]
    async fn a_tab_update_replaces_the_cached_bar_and_commands_reach_the_server() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(async move {
            let mut conn = FrameStream::new(server);
            let _attach = conn.recv::<ClientMessage>().await.expect("attach arrives");
            conn.send(&hello()).await.expect("hello sends");
            conn.send(&ServerMessage::Tabs(vec![
                TabSummary {
                    tab: TabId::new(1),
                    title: "shell".into(),
                    active: false,
                },
                TabSummary {
                    tab: TabId::new(2),
                    title: "build".into(),
                    active: true,
                },
            ]))
            .await
            .expect("tab update sends");
            conn.recv::<ClientMessage>().await.expect("command arrives")
        });

        let mut attached = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect("the attach succeeds");
        assert!(matches!(
            attached.recv().await,
            Ok(Some(ServerMessage::Tabs(_)))
        ));
        assert_eq!(attached.tabs().len(), 2);
        assert_eq!(attached.tabs()[1].title, "build");
        assert!(attached.tabs()[1].active);

        attached
            .send_command(Action::NextTab)
            .await
            .expect("command sends");
        assert_eq!(
            scripted.await.expect("the scripted server finishes"),
            Some(ClientMessage::Command(Action::NextTab))
        );
    }

    #[tokio::test]
    async fn the_reported_capabilities_reach_the_server_unchanged() {
        // Every field distinct from `TermCaps::default()`, so a handshake that
        // dropped or defaulted one is caught rather than passing by coincidence.
        let sent = TermCaps {
            truecolor: true,
            bracketed_paste: true,
            sgr_mouse: true,
            focus_events: true,
            extended_keys: true,
            clipboard_osc52: true,
            hyperlinks: true,
            graphics: true,
        };
        assert_ne!(sent, TermCaps::default());

        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(async move {
            let mut conn = FrameStream::new(server);
            let attach = conn.recv::<ClientMessage>().await.expect("attach arrives");
            let Some(ClientMessage::Attach { term_caps, .. }) = attach else {
                panic!("expected an attach, got {attach:?}");
            };
            conn.send(&hello()).await.expect("hello sends");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            term_caps
        });

        handshake(FrameStream::new(client), Size::new(80, 24), sent, None)
            .await
            .expect("the attach succeeds");
        let received = scripted.await.expect("the scripted server finishes");
        assert_eq!(
            received, sent,
            "TermCaps must round-trip over the handshake"
        );
    }

    #[test]
    fn an_unresolvable_term_is_an_attach_failure_with_the_capability_reason() {
        let err = AttachError::from(CapsError::TermDumb);
        assert!(matches!(err, AttachError::Capabilities(_)), "got {err}");
        assert!(err.to_string().contains("set TERM"), "got: {err}");
    }

    #[tokio::test]
    async fn a_refusal_surfaces_the_servers_own_reason() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(reply_with(
            server,
            Some(ServerMessage::Refused {
                reason: "cloo protocol version mismatch: reattach with a matching build".into(),
            }),
        ));

        let err = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect_err("a refusal must not attach");

        let AttachError::Refused(reason) = &err else {
            panic!("expected Refused, got {err}");
        };
        assert!(reason.contains("version mismatch"), "got: {reason}");
        assert!(
            err.to_string().contains("version mismatch"),
            "the reason must survive into the message the user sees"
        );
        scripted.await.expect("the scripted server finishes");
    }

    #[test]
    fn a_live_state_replaces_workspace_status_on_its_own_projection_clock() {
        let mut state = LiveState::new(
            Size::new(80, 24),
            SessionId::new(7),
            hello_tabs(),
            "C-b".to_owned(),
        );
        let initial = WorkspaceStatus {
            name: "agents".into(),
            clients: 1,
            effective_size: Size::new(80, 22),
        };
        assert!(
            state
                .apply(ServerMessage::WorkspaceStatus(initial.clone()))
                .expect("status applies")
        );
        assert_eq!(state.status.as_ref(), Some(&initial));
        assert!(
            !state
                .apply(ServerMessage::WorkspaceStatus(initial))
                .expect("a repeated status is harmless"),
            "an identical projection must not dirty the frame"
        );

        let replacement = WorkspaceStatus {
            name: "agents".into(),
            clients: 2,
            effective_size: Size::new(72, 18),
        };
        assert!(
            state
                .apply(ServerMessage::WorkspaceStatus(replacement.clone()))
                .expect("replacement status applies")
        );
        assert_eq!(state.status.as_ref(), Some(&replacement));
    }

    #[test]
    fn client_local_status_discards_a_repository_answer_after_focus_changes() {
        let first = PaneId::new(1);
        let second = PaneId::new(2);
        let mut state = LiveState::new(
            Size::new(40, 8),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        state
            .apply(ServerMessage::Panes(vec![
                PaneInfo {
                    pane: first,
                    profile: "generic".to_owned(),
                    name: "one".to_owned(),
                    task: None,
                    cwd: "/repo/one".to_owned(),
                },
                PaneInfo {
                    pane: second,
                    profile: "generic".to_owned(),
                    name: "two".to_owned(),
                    task: None,
                    cwd: "/repo/two".to_owned(),
                },
            ]))
            .expect("pane metadata applies");
        let layout = |focused| LayoutSnapshot {
            tab: TabId::new(1),
            panes: vec![
                PaneRect {
                    pane: first,
                    x: 1,
                    y: 1,
                    size: Size::new(18, 4),
                },
                PaneRect {
                    pane: second,
                    x: 21,
                    y: 1,
                    size: Size::new(18, 4),
                },
            ],
            focused: Some(focused),
            zoomed: None,
        };

        state
            .apply(ServerMessage::Layout(layout(first)))
            .expect("first focus applies");
        assert_eq!(
            state.refresh_repository_target(),
            Some(PathBuf::from("/repo/one"))
        );
        state
            .apply(ServerMessage::Layout(layout(second)))
            .expect("second focus applies");
        assert_eq!(
            state.refresh_repository_target(),
            Some(PathBuf::from("/repo/two"))
        );

        let stale = RepositoryStatus {
            branch: Some("stale".to_owned()),
            changes: 9,
        };
        assert!(!state.apply_repository(PathBuf::from("/repo/one"), Some(stale)));
        assert_eq!(
            state.local_status.repository(),
            None,
            "the old pane's answer is omitted instead of shown for the new focus"
        );
    }

    #[test]
    fn live_status_bar_draws_projected_and_client_local_values_without_placeholders() {
        struct FixedClock;

        impl crate::status::LocalClock for FixedClock {
            fn now(&self) -> Option<crate::status::LocalTime> {
                crate::status::LocalTime::new(14, 38)
            }
        }

        let mut state = hinted_state(96, 2, "C-b");
        state
            .apply(ServerMessage::WorkspaceStatus(WorkspaceStatus {
                name: "agents".to_owned(),
                clients: 2,
                effective_size: Size::new(96, 6),
            }))
            .expect("workspace status applies");
        state.local_status.refresh_clock(&FixedClock);
        state.local_status.set_repository(Some(RepositoryStatus {
            branch: Some("feature/status".to_owned()),
            changes: 2,
        }));

        let row = status_text(&state);
        for field in [
            "s agents",
            ">1 shell",
            "0!",
            "git feature/status +2",
            "2 clients",
            "C-b",
            "14:38",
        ] {
            assert!(row.contains(field), "missing {field:?} in {row:?}");
        }
        assert!(
            !row.contains("session:"),
            "numeric placeholders are gone: {row:?}"
        );
    }

    #[tokio::test]
    async fn a_hello_from_a_future_server_is_caught_client_side() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(reply_with(
            server,
            Some(ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION.wrapping_add(1),
                session: SessionId::new(1),
                tabs: Vec::new(),
                size: Size::new(80, 24),
            }),
        ));

        let err = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect_err("a rebuilt server must be caught");

        assert!(matches!(err, AttachError::Version(_)), "got {err}");
        assert!(err.to_string().contains("reattach"), "got: {err}");
        scripted.await.expect("the scripted server finishes");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_hello_is_refused() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(reply_with(
            server,
            Some(ServerMessage::Bell(PaneId::new(1))),
        ));

        let err = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect_err("a bell is not a handshake");
        assert!(matches!(err, AttachError::UnexpectedReply), "got {err}");
        scripted.await.expect("the scripted server finishes");
    }

    #[tokio::test]
    async fn a_server_that_says_nothing_is_reported_as_a_close() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(reply_with(server, None));

        let err = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect_err("silence is not a handshake");
        assert!(matches!(err, AttachError::Closed), "got {err}");
        scripted.await.expect("the scripted server finishes");
    }

    #[tokio::test]
    async fn detach_asks_and_waits_for_the_acknowledgement() {
        let (client, server) = duplex(4096);
        let scripted = tokio::spawn(async move {
            let mut conn = FrameStream::new(server);
            let _attach = conn.recv::<ClientMessage>().await.expect("attach arrives");
            conn.send(&hello()).await.expect("hello sends");
            // Damage still in flight when the user hit the detach key.
            conn.send(&ServerMessage::Bell(PaneId::new(1)))
                .await
                .expect("a stray frame sends");
            let request = conn.recv::<ClientMessage>().await.expect("detach arrives");
            assert_eq!(request, Some(ClientMessage::Detach));
            conn.send(&ServerMessage::Detached)
                .await
                .expect("the acknowledgement sends");
        });

        let attached = handshake(
            FrameStream::new(client),
            Size::new(80, 24),
            TermCaps::default(),
            None,
        )
        .await
        .expect("the attach succeeds");
        attached.detach().await.expect("detach succeeds");
        scripted.await.expect("the scripted server finishes");
    }

    #[test]
    fn a_live_state_places_the_server_grid_below_attached_chrome() {
        let pane = PaneId::new(1);
        let mut state = LiveState::new(
            Size::new(8, 5),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![PaneRect {
                    pane,
                    x: 1,
                    y: 1,
                    size: Size::new(6, 1),
                }],
                focused: Some(pane),
                zoomed: None,
            }))
            .expect("the layout applies");
        state
            .apply(ServerMessage::Panes(vec![PaneInfo {
                pane,
                profile: "generic".into(),
                name: "build".into(),
                task: Some("test it".into()),
                cwd: "/".into(),
            }]))
            .expect("the pane metadata applies");
        state
            .apply(ServerMessage::Attention(vec![PaneAttention {
                pane,
                state: AttentionState::NeedsInput,
                source: cloo_proto::AttentionSource::Bell,
                acknowledged: false,
            }]))
            .expect("the attention applies");
        state
            .apply(ServerMessage::Damage {
                pane,
                rows: vec![RowUpdate {
                    row: 0,
                    cells: vec![
                        Cell {
                            ch: 'a',
                            ..Cell::default()
                        };
                        6
                    ],
                }],
            })
            .expect("the grid damage applies");

        let spans = state.spans();
        assert_eq!(spans[0].at, Point::new(0, 0), "the tab row is fixed");
        assert_eq!(
            spans[1].at,
            Point::new(0, 1),
            "the header is above the grid"
        );
        assert_eq!(
            spans[3].at,
            Point::new(1, 2),
            "the grid starts below chrome"
        );
        assert_eq!(spans[3].cells[0].ch, 'a');
        assert_eq!(spans[5].at, Point::new(0, 3), "the bottom edge is visible");
        assert_eq!(
            spans.last().map(|span| span.at),
            Some(Point::new(0, 4)),
            "the status row owns the last outer-terminal row"
        );
        assert_eq!(state.screen.hit(1, 2).pane(), Some(pane));
        assert_eq!(
            state.screen.hit(0, 2),
            crate::input::MouseTarget::Chrome(crate::input::ChromeTarget::Frame { pane })
        );
    }

    /// A live state showing `panes` panes on a `width`-wide terminal.
    fn hinted_state(width: u16, panes: usize, prefix: &str) -> LiveState {
        let mut state = LiveState::new(
            Size::new(width, 6),
            SessionId::new(1),
            hello_tabs(),
            prefix.to_owned(),
        );
        let cols = width / u16::try_from(panes).expect("a small pane count");
        let rects = (0..panes)
            .map(|index| {
                let index = u16::try_from(index).expect("a small pane count");
                PaneRect {
                    pane: PaneId::new(u64::from(index) + 1),
                    x: index * cols,
                    y: 0,
                    size: Size::new(cols, 2),
                }
            })
            .collect::<Vec<_>>();
        let focused = rects.first().map(|rect| rect.pane);
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: rects,
                focused,
                zoomed: None,
            }))
            .expect("the layout applies");
        state
    }

    /// The text of the frame's status row.
    fn status_text(state: &LiveState) -> String {
        state
            .spans()
            .last()
            .expect("a status row")
            .cells
            .iter()
            .map(|cell| cell.ch)
            .collect()
    }

    /// The text of the frame's tab row.
    fn tab_row_text(state: &LiveState) -> String {
        state
            .spans()
            .first()
            .expect("a tab row")
            .cells
            .iter()
            .map(|cell| cell.ch)
            .collect()
    }

    #[test]
    fn the_attached_tab_row_draws_the_daemon_projection_and_invents_nothing() {
        let mut state = hinted_state(60, 2, "C-b");

        let row = tab_row_text(&state);
        assert!(
            row.starts_with(">1 shell"),
            "no badge before a projection arrives: {row:?}"
        );
        assert!(
            row.contains("2 panes"),
            "the pane count is the layout this client drew: {row:?}"
        );
        assert!(
            !row.contains("client"),
            "an unprojected client count is omitted, never invented: {row:?}"
        );

        state
            .apply(ServerMessage::WorkspaceStatus(WorkspaceStatus {
                name: "agents".into(),
                clients: 3,
                effective_size: Size::new(60, 6),
            }))
            .expect("the projection applies");

        let row = tab_row_text(&state);
        assert!(
            row.starts_with(" agents >1 shell"),
            "the badge names the session the daemon projected: {row:?}"
        );
        assert!(
            row.contains("2 panes  3 clients"),
            "the client count is the daemon's, not a pane-cell guess: {row:?}"
        );
    }

    #[test]
    fn a_one_pane_attached_frame_offers_the_configured_prefix_and_its_clues() {
        let state = hinted_state(60, 1, "M-a");
        let row = status_text(&state);
        assert!(
            row.contains("M-a split % stack \" help ?"),
            "the first frame must explain how to act: {row:?}"
        );
        assert!(
            !row.contains("C-b"),
            "the row must name the configured chord, not the default: {row:?}"
        );
    }

    #[test]
    fn a_second_pane_returns_the_status_row_to_its_ordinary_summary() {
        let row = status_text(&hinted_state(60, 2, "C-b"));
        assert!(
            row.contains("C-b") && !row.contains("split %"),
            "past the first pane the clues yield their width back: {row:?}"
        );
    }

    #[test]
    fn a_pending_prefix_is_drawn_distinctly_in_the_status_row() {
        let mut state = hinted_state(60, 2, "C-b");
        let settled = status_text(&state);
        state.prefix_pending = true;
        let pending = status_text(&state);
        assert!(!settled.contains("[C-b]"), "settled row: {settled:?}");
        assert!(
            pending.contains("[C-b] split % stack \" help ?"),
            "a held prefix says what the next key can be: {pending:?}"
        );
    }

    fn resize_state() -> LiveState {
        let mut state = LiveState::new(
            Size::new(40, 10),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![
                    PaneRect {
                        pane: PaneId::new(1),
                        x: 1,
                        y: 1,
                        size: Size::new(18, 6),
                    },
                    PaneRect {
                        pane: PaneId::new(2),
                        x: 21,
                        y: 1,
                        size: Size::new(18, 6),
                    },
                ],
                focused: Some(PaneId::new(1)),
                zoomed: None,
            }))
            .expect("the split applies");
        state
    }

    #[test]
    fn keyboard_resize_uses_the_focused_divider_and_an_edge_is_a_noop() {
        let now = Instant::now();
        let mut state = resize_state();
        assert_eq!(state.keyboard_resize(Side::Left, now), None);
        assert!(state.resize.is_none(), "an outer edge lights nothing");
        assert_eq!(
            state.keyboard_resize(Side::Right, now),
            Some(Action::ResizePane {
                pane: PaneId::new(1),
                dir: cloo_proto::Direction::Horizontal,
                delta: 1,
            })
        );
        assert_eq!(state.resize.map(|resize| resize.ratio), Some(0.5));
    }

    #[test]
    fn keyboard_and_mouse_resize_affordances_show_the_result_and_clear_at_their_boundary() {
        let now = Instant::now();
        let mut state = resize_state();
        let _ = state.keyboard_resize(Side::Right, now);
        state
            .apply_at(
                ServerMessage::Layout(LayoutSnapshot {
                    tab: TabId::new(1),
                    panes: vec![
                        PaneRect {
                            pane: PaneId::new(1),
                            x: 1,
                            y: 1,
                            size: Size::new(23, 6),
                        },
                        PaneRect {
                            pane: PaneId::new(2),
                            x: 26,
                            y: 1,
                            size: Size::new(13, 6),
                        },
                    ],
                    focused: Some(PaneId::new(1)),
                    zoomed: None,
                }),
                now,
            )
            .expect("the resized layout applies");
        let drawn = frame_text(&state);
        assert!(drawn.iter().any(|row| row == "resize · ratio 0.62"));
        let spans = state.spans();
        let lit = spans
            .iter()
            .filter(|span| {
                span.cells
                    .as_slice()
                    .first()
                    .is_some_and(|cell| cell.ch == '│' && cell.attrs.contains(CellAttrs::BOLD))
            })
            .collect::<Vec<_>>();
        assert!(!lit.is_empty(), "the changed divider must be lit");
        assert!(
            lit.iter()
                .all(|span| span.at.col == 24 || span.at.col == 25),
            "only the changed divider is lit: {lit:?}"
        );
        assert!(!state.tick_resize(now + KEYBOARD_RESIZE_LINGER - Duration::from_millis(1)));
        assert!(state.tick_resize(now + KEYBOARD_RESIZE_LINGER));
        assert!(state.resize.is_none());

        let divider = Divider {
            pane: PaneId::new(1),
            dir: cloo_proto::Direction::Horizontal,
        };
        state.sync_mouse_resize(Some(divider), now);
        assert!(state.resize.is_some());
        state.sync_mouse_resize(None, now);
        assert!(state.resize.is_none(), "mouse release ends the affordance");
    }

    #[test]
    fn a_live_copy_state_layers_highlights_and_its_status_over_the_composed_frame() {
        let pane = PaneId::new(1);
        let mut state = LiveState::new(
            Size::new(9, 5),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![PaneRect {
                    pane,
                    x: 1,
                    y: 1,
                    size: Size::new(7, 1),
                }],
                focused: Some(pane),
                zoomed: None,
            }))
            .expect("the layout applies");
        state
            .apply(ServerMessage::Damage {
                pane,
                rows: vec![RowUpdate {
                    row: 0,
                    cells: "copy th"
                        .chars()
                        .map(|ch| Cell {
                            ch,
                            ..Cell::default()
                        })
                        .collect(),
                }],
            })
            .expect("the grid damage applies");
        state
            .apply(ServerMessage::CopyMode(Some(CopyModeState {
                pane,
                viewport_top: 12,
                cursor: ScrollPoint::new(12, 5),
                selection: Some(CopySelection {
                    anchor: ScrollPoint::new(12, 0),
                    head: ScrollPoint::new(12, 3),
                }),
                query: None,
                matches: Vec::new(),
            })))
            .expect("copy state applies");

        let spans = state.spans();
        assert!(spans.iter().any(|span| {
            span.at == Point::new(1, 2)
                && span.cells.iter().map(|cell| cell.ch).collect::<String>() == "copy"
        }));
        assert_eq!(
            spans
                .last()
                .expect("copy mode replaces the status row")
                .cells
                .iter()
                .take(4)
                .map(|cell| cell.ch)
                .collect::<String>(),
            "COPY"
        );
    }

    #[test]
    fn an_open_overlay_dims_the_composed_frame_and_keeps_its_keys_client_side() {
        let pane = PaneId::new(1);
        let mut state = LiveState::new(
            Size::new(20, 6),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![PaneRect {
                    pane,
                    x: 0,
                    y: 0,
                    size: Size::new(20, 3),
                }],
                focused: Some(pane),
                zoomed: None,
            }))
            .expect("the layout applies");
        state
            .apply(ServerMessage::Damage {
                pane,
                rows: vec![RowUpdate {
                    row: 0,
                    cells: vec![
                        Cell {
                            ch: 'x',
                            ..Cell::default()
                        };
                        20
                    ],
                }],
            })
            .expect("the grid damage applies");
        assert!(state.open_overlay(b"s", &Keymap::defaults()));
        assert!(
            state.apply_overlay_keys(b"j").consumed(),
            "an overlay owns navigation"
        );

        let frame = state.frame();
        assert!(
            frame.spans.iter().any(|span| {
                span.cells
                    .iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .starts_with("  sessions")
            }),
            "the switcher opens without inventing a current-session row"
        );
        assert!(
            frame.chrome.iter().rev().take(3).all(|chrome| *chrome),
            "the overlay is client chrome, never a pane span"
        );
        assert!(
            frame.spans.iter().any(|span| {
                span.at == Point::new(0, 2)
                    && span
                        .cells
                        .first()
                        .is_some_and(|cell| cell.attrs.contains(CellAttrs::DIM))
            }),
            "the pane body remains visible beneath a dimmed backdrop"
        );
    }

    /// A one-pane live state with server-supplied identity for that pane, which
    /// is what the details surface is allowed to draw from.
    fn overlay_state(rows: u16, prefix: &str) -> LiveState {
        let pane = PaneId::new(1);
        let mut state = LiveState::new(
            Size::new(40, rows),
            SessionId::new(1),
            hello_tabs(),
            prefix.to_owned(),
        );
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![PaneRect {
                    pane,
                    x: 1,
                    y: 1,
                    size: Size::new(38, rows.saturating_sub(4)),
                }],
                focused: Some(pane),
                zoomed: None,
            }))
            .expect("the layout applies");
        state
            .apply(ServerMessage::Panes(vec![PaneInfo {
                pane,
                profile: "generic".to_owned(),
                name: "shell".to_owned(),
                task: None,
                cwd: "/home/dev".to_owned(),
            }]))
            .expect("the identity applies");
        state
    }

    /// The milestone's routing change: `?` is the command palette and `i` is
    /// still pane details, rather than both landing on details.
    #[test]
    fn the_prefix_key_opens_the_command_palette_and_details_keeps_its_own_key() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(8, "C-b");

        assert!(state.open_overlay(b"?", &keymap));
        assert!(
            matches!(
                state.overlay.as_ref().map(Overlay::kind),
                Some(OverlayKind::Palette(_))
            ),
            "the palette key must no longer land on pane details"
        );
        assert!(
            state.apply_overlay_keys(b"\x1b").consumed(),
            "escape dismisses"
        );
        assert!(state.overlay.is_none());

        assert!(state.open_overlay(b"i", &keymap));
        assert!(matches!(
            state.overlay.as_ref().map(Overlay::kind),
            Some(OverlayKind::Details(_))
        ));
    }

    /// The palette is read from the client's live keymap, so the frame a user
    /// actually sees names the chord they actually configured.
    #[test]
    fn the_open_command_palette_draws_the_configured_prefix_and_its_controls() {
        let mut keymap = Keymap::defaults();
        keymap.set_prefix(cloo_core::keymap::Key::parse("M-a").expect("a spelling"));
        let mut state = overlay_state(24, "M-a");
        assert!(state.open_overlay(b"?", &keymap));

        let drawn: Vec<String> = state
            .frame()
            .spans
            .iter()
            .map(|span| span.cells.iter().map(|cell| cell.ch).collect())
            .collect();
        for expected in ["commands - prefix M-a", "split right", "detach", "add pane"] {
            assert!(
                drawn.iter().any(|row| row.contains(expected)),
                "the palette frame must show {expected:?}: {drawn:?}"
            );
        }
    }

    /// An open palette owns the keyboard *including ordinary text*: a query is
    /// the one thing a user types into an overlay, and the byte that would have
    /// reached a shell must become a filter instead.
    #[test]
    fn an_open_command_palette_consumes_typed_query_bytes_locally() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(12, "C-b");
        assert!(state.open_overlay(b"?", &keymap));
        for key in [b"s".as_slice(), b"plit", b"\x1b[B", b"\x7f"] {
            assert!(
                state.apply_overlay_keys(key).consumed(),
                "the palette owns {key:?}"
            );
        }
        let Some(OverlayKind::Palette(palette)) = state.overlay.as_ref().map(Overlay::kind) else {
            panic!("a typed query leaves the palette open");
        };
        assert_eq!(palette.query(), "spli");

        // Enter runs the selected command, which leaves as a typed action —
        // and the row the arrow moved to is still the row the backspace left
        // the cursor on, because the selection follows the command.
        assert_eq!(
            state.apply_overlay_keys(b"\r"),
            OverlayKeys::Command(Action::SplitHorizontal)
        );
        assert!(state.overlay.is_none());
    }

    /// A palette row naming a client surface opens it in place rather than
    /// crossing the wire, so both routes to the launcher end up in one surface.
    #[test]
    fn confirming_a_client_row_swaps_the_command_palette_for_that_surface() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(16, "C-b").profiles(Profile::built_ins());
        assert!(state.open_overlay(b"?", &keymap));
        assert!(state.apply_overlay_keys(b"add pane").consumed());
        assert_eq!(state.apply_overlay_keys(b"\r"), OverlayKeys::Consumed);
        assert!(
            matches!(
                state.overlay.as_ref().map(Overlay::kind),
                Some(OverlayKind::Launcher(_))
            ),
            "a client surface never reaches the wire"
        );
    }

    fn catalog_entry(
        path: &str,
        name: &str,
        tabs: u16,
        panes: u16,
        clients: u16,
    ) -> SessionCatalogEntry {
        SessionCatalogEntry {
            socket: PathBuf::from(path),
            summary: SessionSummary {
                name: name.to_owned(),
                tabs,
                panes,
                clients,
                uptime_secs: 10,
            },
        }
    }

    #[test]
    fn session_switcher_uses_verified_catalog_and_returns_the_selected_socket() {
        let current = Path::new("/run/cloo/main.sock");
        let mut state = overlay_state(10, "C-b");
        assert!(state.refresh_sessions(
            vec![
                catalog_entry("/run/cloo/main.sock", "main", 2, 3, 1),
                catalog_entry("/run/cloo/review.sock", "review", 1, 1, 0),
            ],
            current,
        ));
        assert!(state.open_overlay(b"s", &Keymap::defaults()));

        let drawn = frame_text(&state);
        for truthful in ["main attached", "2 tabs", "3 panes", "review"] {
            assert!(
                drawn.iter().any(|row| row.contains(truthful)),
                "the verified catalog must draw {truthful:?}: {drawn:?}"
            );
        }
        assert!(state.apply_overlay_keys(b"j").consumed());
        assert_eq!(
            state.apply_overlay_keys(b"\r"),
            OverlayKeys::SwitchSession(PathBuf::from("/run/cloo/review.sock"))
        );
        assert!(
            state.overlay.is_some(),
            "the current attachment keeps the switcher until the target accepts"
        );
    }

    #[test]
    fn session_switcher_refresh_drops_a_disappeared_daemon_and_keeps_a_live_selection() {
        let current = Path::new("/run/cloo/main.sock");
        let main = catalog_entry("/run/cloo/main.sock", "main", 1, 1, 1);
        let review = catalog_entry("/run/cloo/review.sock", "review", 1, 1, 0);
        let mut state = overlay_state(10, "C-b");
        state.refresh_sessions(vec![main.clone(), review], current);
        assert!(state.open_overlay(b"s", &Keymap::defaults()));
        assert!(state.apply_overlay_keys(b"j").consumed());

        assert!(state.refresh_sessions(vec![main], current));
        let Some(OverlayKind::Sessions(entries)) = state.overlay.as_ref().map(Overlay::kind) else {
            panic!("the switcher stays open while its catalog refreshes");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].socket(), current);
        assert_eq!(state.overlay.as_ref().map(Overlay::selection), Some(0));
    }

    /// The launcher is the client's own surface over the client's own profile
    /// list, and confirming a row is the one overlay outcome that leaves it.
    #[test]
    fn the_add_pane_key_opens_a_launcher_over_the_configured_profiles() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(12, "C-b").profiles(Profile::built_ins());

        assert!(state.open_overlay(b"a", &keymap));
        let Some(OverlayKind::Launcher(entries)) = state.overlay.as_ref().map(Overlay::kind) else {
            panic!("the add-pane key opens the profile launcher");
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.profile().as_str())
                .collect::<Vec<_>>(),
            ["generic", "codex", "claude"],
            "the launcher lists exactly the profiles this client resolved"
        );

        let OverlayKeys::Launch(request) = state.apply_overlay_keys(b"\r") else {
            panic!("confirming a launcher row is a launch the caller must send");
        };
        assert_eq!(request.profile().as_str(), "generic");
        assert!(
            state.overlay.is_none(),
            "the launcher closes on the confirmation that produced the launch"
        );
    }

    /// A client with nothing configured still opens a launcher, and it confirms
    /// to nothing — the surface must never invent a profile to offer.
    #[test]
    fn a_launcher_with_no_configured_profile_launches_nothing() {
        let mut state = overlay_state(12, "C-b");
        assert!(state.open_overlay(b"a", &Keymap::defaults()));
        assert_eq!(
            state.apply_overlay_keys(b"\r"),
            OverlayKeys::Consumed,
            "an empty launcher consumes the confirmation without naming a profile"
        );
    }

    /// Escape is the other half: the overlay closes, nothing is sent, and no
    /// notice claims a launch that never happened.
    #[test]
    fn dismissing_the_launcher_leaves_the_workspace_untouched() {
        let mut state = overlay_state(12, "C-b").profiles(Profile::built_ins());
        assert!(state.open_overlay(b"a", &Keymap::defaults()));
        assert_eq!(state.apply_overlay_keys(b"\x1b"), OverlayKeys::Consumed);
        assert!(state.overlay.is_none());
        assert!(
            state.launch.is_none(),
            "a dismissal must not leave a launch notice behind"
        );
    }

    // -- the attention queue -------------------------------------------------

    /// Two panes with server identity, so the queue has real rows to list and a
    /// real `PaneId` to name in the action a row produces.
    fn attention_queue_state() -> LiveState {
        let mut state = LiveState::new(
            Size::new(40, 14),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        );
        let panes = [PaneId::new(1), PaneId::new(2), PaneId::new(3)];
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: panes
                    .iter()
                    .enumerate()
                    .map(|(index, pane)| PaneRect {
                        pane: *pane,
                        x: 1,
                        y: u16::try_from(index).unwrap_or(0) * 4 + 1,
                        size: Size::new(38, 2),
                    })
                    .collect(),
                focused: Some(panes[0]),
                zoomed: None,
            }))
            .expect("the layout applies");
        state
            .apply(ServerMessage::Panes(
                [
                    (panes[0], "shell"),
                    (panes[1], "claude"),
                    (panes[2], "build"),
                ]
                .into_iter()
                .map(|(pane, name)| PaneInfo {
                    pane,
                    profile: "generic".to_owned(),
                    name: name.to_owned(),
                    task: None,
                    cwd: "/home/dev".to_owned(),
                })
                .collect(),
            ))
            .expect("the pane identity applies");
        state
            .apply(attention_projection(&[
                // Progress is not something a person is asked to act on, so this
                // pane must never reach the queue.
                (panes[0], AttentionState::Working, false),
                (panes[1], AttentionState::NeedsInput, false),
                (panes[2], AttentionState::Failed, false),
            ]))
            .expect("the attention applies");
        state
    }

    /// One `ServerMessage::Attention`, which is the only thing that ever changes
    /// what the queue holds.
    fn attention_projection(states: &[(PaneId, AttentionState, bool)]) -> ServerMessage {
        ServerMessage::Attention(
            states
                .iter()
                .map(|(pane, state, acknowledged)| PaneAttention {
                    pane: *pane,
                    state: *state,
                    source: cloo_proto::AttentionSource::Lifecycle,
                    acknowledged: *acknowledged,
                })
                .collect(),
        )
    }

    /// The rows an open queue is listing, as `(pane, title, state)`.
    fn attention_queue_rows(state: &LiveState) -> Vec<(PaneId, String, Attention)> {
        let Some(OverlayKind::Attention(entries)) = state.overlay.as_ref().map(Overlay::kind)
        else {
            panic!("the attention queue is open");
        };
        entries
            .iter()
            .map(|entry| (entry.pane, entry.title.clone(), entry.attention))
            .collect()
    }

    /// The surface is built from the server's own projection: one row per pane
    /// with a live actionable state, newest first, and nothing else at all.
    #[test]
    fn the_attention_key_opens_an_attention_queue_of_the_live_projection() {
        let mut state = attention_queue_state();
        assert!(state.open_overlay(b"!", &Keymap::defaults()));
        assert_eq!(
            attention_queue_rows(&state),
            vec![
                (PaneId::new(3), "build".to_owned(), Attention::Failed),
                (PaneId::new(2), "claude".to_owned(), Attention::NeedsInput),
            ],
            "a working pane is progress, not a queue row"
        );
    }

    /// The overlay owns every key it understands, and the two it acts on leave
    /// as *typed session actions naming a pane* — never as bytes for a child.
    #[test]
    fn an_open_attention_queue_produces_only_typed_pane_actions() {
        let mut state = attention_queue_state();
        assert!(state.open_overlay(b"!", &Keymap::defaults()));

        assert_eq!(state.apply_overlay_keys(b"j"), OverlayKeys::Consumed);
        assert_eq!(
            state.apply_overlay_keys(b" "),
            OverlayKeys::Command(Action::AcknowledgeAttention(PaneId::new(2))),
            "acknowledging names the pane the cursor is on"
        );
        assert!(
            state.overlay.is_some(),
            "the queue stays up: the row leaves when the server says it has"
        );
        assert_eq!(state.apply_overlay_keys(b"k"), OverlayKeys::Consumed);
        assert_eq!(
            state.apply_overlay_keys(b"\r"),
            OverlayKeys::Command(Action::FocusPane(PaneId::new(3)))
        );
        assert!(
            state.overlay.is_none(),
            "focusing the pane a row names is what the queue is for, so it closes"
        );
    }

    /// The single-owner rule made visible: acknowledgment is session state, so
    /// the row survives the keypress and leaves on the projection that says the
    /// server applied it.
    #[test]
    fn an_acknowledged_attention_queue_row_leaves_on_the_server_projection() {
        let mut state = attention_queue_state();
        assert!(state.open_overlay(b"!", &Keymap::defaults()));
        assert_eq!(
            state.apply_overlay_keys(b"a"),
            OverlayKeys::Command(Action::AcknowledgeAttention(PaneId::new(3)))
        );
        assert_eq!(
            attention_queue_rows(&state).len(),
            2,
            "a client that hid the row itself would be a second source of truth"
        );

        state
            .apply(attention_projection(&[
                (PaneId::new(1), AttentionState::Working, false),
                (PaneId::new(2), AttentionState::NeedsInput, false),
                (PaneId::new(3), AttentionState::Failed, true),
            ]))
            .expect("the acknowledgment comes back as a projection");
        assert_eq!(
            attention_queue_rows(&state),
            vec![(PaneId::new(2), "claude".to_owned(), Attention::NeedsInput)],
            "the open queue follows the projection while it is open"
        );
        assert_eq!(
            state.apply_overlay_keys(b"\r"),
            OverlayKeys::Command(Action::FocusPane(PaneId::new(2))),
            "the cursor lands on the row that is left, not on a stale position"
        );
    }

    /// The rest of the overlay contract, on the live surface: it is drawn as
    /// client chrome over a dimmed frame, and Escape always closes it.
    #[test]
    fn an_open_attention_queue_is_dimmed_chrome_and_escapes_cleanly() {
        let mut state = attention_queue_state();
        assert!(state.open_overlay(b"!", &Keymap::defaults()));
        let frame = state.frame();
        assert!(
            frame.spans.iter().any(|span| {
                span.cells
                    .iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .starts_with("  attention 1/2")
            }),
            "the queue draws through the shared overlay treatment"
        );
        assert!(
            frame.chrome.iter().rev().take(4).all(|chrome| *chrome),
            "an overlay is client chrome, never a pane span"
        );
        assert_eq!(state.apply_overlay_keys(b"\x1b"), OverlayKeys::Consumed);
        assert!(state.overlay.is_none());
    }

    /// A workspace with nothing waiting still answers the key: an empty queue is
    /// a legible answer, and a key that appeared to do nothing is the worse one.
    #[test]
    fn an_empty_attention_queue_opens_and_acts_on_nothing() {
        let mut state = overlay_state(12, "C-b");
        assert!(state.open_overlay(b"!", &Keymap::defaults()));
        assert!(attention_queue_rows(&state).is_empty());
        for key in [b"j".as_slice(), b"k", b"a", b"\r"] {
            assert_eq!(
                state.apply_overlay_keys(key),
                OverlayKeys::Consumed,
                "{key:?} must name no pane at all"
            );
        }
        assert!(state.overlay.is_some());
    }

    // -- the live toast stack ------------------------------------------------

    /// A terminal that negotiated 24-bit colour.
    fn truecolor_caps() -> TermCaps {
        TermCaps {
            truecolor: true,
            ..TermCaps::default()
        }
    }

    /// A nested workspace: two stacked panes beside a tall one, which is the
    /// geometry card 03 draws and the one a floating notice has to survive.
    ///
    /// `cols`/`rows` are the outer terminal, so a narrow frame is the same
    /// fixture with a different size.
    fn nested_toast_state(outer: Size, caps: TermCaps) -> LiveState {
        let panes = [PaneId::new(1), PaneId::new(2), PaneId::new(3)];
        let split = outer.cols / 2;
        let body = outer.rows.saturating_sub(FIXED_CHROME_ROWS);
        let mut state = LiveState::new(outer, SessionId::new(1), hello_tabs(), "C-b".to_owned())
            .preferences(caps, VisualConfig::defaults());
        state
            .apply(ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![
                    PaneRect {
                        pane: panes[0],
                        x: 0,
                        y: 0,
                        size: Size::new(split, body),
                    },
                    PaneRect {
                        pane: panes[1],
                        x: split,
                        y: 0,
                        size: Size::new(outer.cols - split, body / 2),
                    },
                    PaneRect {
                        pane: panes[2],
                        x: split,
                        y: body / 2,
                        size: Size::new(outer.cols - split, body - body / 2),
                    },
                ],
                focused: Some(panes[0]),
                zoomed: None,
            }))
            .expect("the nested layout applies");
        state
            .apply(ServerMessage::Panes(
                [
                    (panes[0], "shell"),
                    (panes[1], "claude"),
                    (panes[2], "build"),
                ]
                .into_iter()
                .map(|(pane, name)| PaneInfo {
                    pane,
                    profile: "generic".to_owned(),
                    name: name.to_owned(),
                    task: None,
                    cwd: "/home/dev".to_owned(),
                })
                .collect(),
            ))
            .expect("the pane identity applies");
        state
    }

    /// The toasts showing, oldest first, as `(pane index, title, state, repeats)`.
    fn toast_stack(state: &LiveState) -> Vec<(u16, String, Attention, u32)> {
        state
            .toasts
            .toasts()
            .map(|toast| {
                (
                    toast.index,
                    toast.title.clone(),
                    toast.attention,
                    toast.repeats,
                )
            })
            .collect()
    }

    /// The text of every span in a composed frame.
    fn frame_text(state: &LiveState) -> Vec<String> {
        state
            .frame()
            .spans
            .iter()
            .map(|span| span.cells.iter().map(|cell| cell.ch).collect())
            .collect()
    }

    /// A new actionable state is the event; the wire's repeated *state* is not.
    #[test]
    fn a_new_actionable_attention_event_raises_one_toast_and_repeats_coalesce() {
        let now = Instant::now();
        let mut state = nested_toast_state(Size::new(60, 14), truecolor_caps());
        state
            .apply_at(
                attention_projection(&[
                    // Progress is not something a person is asked to act on.
                    (PaneId::new(1), AttentionState::Working, false),
                    (PaneId::new(2), AttentionState::NeedsInput, false),
                ]),
                now,
            )
            .expect("the attention applies");
        assert_eq!(
            toast_stack(&state),
            vec![(2, "claude".to_owned(), Attention::NeedsInput, 1)],
            "one actionable event, one notice"
        );

        // A daemon may resend an unchanged projection at any time; that is not a
        // second event and must not touch the count.
        state
            .apply_at(
                attention_projection(&[
                    (PaneId::new(1), AttentionState::Working, false),
                    (PaneId::new(2), AttentionState::NeedsInput, false),
                ]),
                now,
            )
            .expect("the resend applies");
        assert_eq!(toast_stack(&state)[0].3, 1, "a resend is not a repeat");

        // The pane genuinely raising it again is, and it coalesces.
        state
            .apply_at(
                attention_projection(&[(PaneId::new(2), AttentionState::Quiet, false)]),
                now,
            )
            .expect("the lull applies");
        state
            .apply_at(
                attention_projection(&[(PaneId::new(2), AttentionState::NeedsInput, false)]),
                now,
            )
            .expect("the second event applies");
        assert_eq!(
            toast_stack(&state),
            vec![(2, "claude".to_owned(), Attention::NeedsInput, 2)],
            "a repeat is one notice with a count, never two"
        );

        // A changed state refreshes the same notice rather than stacking one.
        state
            .apply_at(
                attention_projection(&[(PaneId::new(2), AttentionState::Failed, false)]),
                now,
            )
            .expect("the changed state applies");
        assert_eq!(
            toast_stack(&state),
            vec![(2, "claude".to_owned(), Attention::Failed, 3)]
        );
    }

    /// The other half of the diff: an acknowledgment takes the notice away at
    /// once, while a pane merely settling down leaves it to its own deadline.
    #[test]
    fn an_acknowledged_pane_takes_its_toast_away_and_a_settled_one_does_not() {
        let now = Instant::now();
        let mut state = nested_toast_state(Size::new(60, 14), truecolor_caps());
        state
            .apply_at(
                attention_projection(&[
                    (PaneId::new(2), AttentionState::NeedsInput, false),
                    (PaneId::new(3), AttentionState::Failed, false),
                ]),
                now,
            )
            .expect("the attention applies");
        assert_eq!(state.toasts.len(), 2);

        state
            .apply_at(
                attention_projection(&[
                    // Acknowledged in another client, or through the queue.
                    (PaneId::new(2), AttentionState::NeedsInput, true),
                    (PaneId::new(3), AttentionState::Quiet, false),
                ]),
                now,
            )
            .expect("the projection applies");
        assert_eq!(
            toast_stack(&state),
            vec![(3, "build".to_owned(), Attention::Failed, 1)],
            "an acknowledged event is gone; a finished one is still worth reading"
        );

        // The pane closing is the other thing that retires a notice, because a
        // position that no longer exists names nothing.
        state
            .apply(ServerMessage::Panes(vec![PaneInfo {
                pane: PaneId::new(1),
                profile: "generic".to_owned(),
                name: "shell".to_owned(),
                task: None,
                cwd: "/home/dev".to_owned(),
            }]))
            .expect("the closes apply");
        assert!(state.toasts.is_empty());
    }

    /// Bounded in both axes: capacity caps the stack, and every notice leaves on
    /// its own deadline rather than sitting over the workspace.
    #[test]
    fn a_toast_stack_is_bounded_and_clears_itself_on_the_render_clock() {
        let now = Instant::now();
        let mut state = nested_toast_state(Size::new(60, 14), truecolor_caps());
        state
            .apply(ServerMessage::Panes(
                (1..=4)
                    .map(|n| PaneInfo {
                        pane: PaneId::new(n),
                        profile: "generic".to_owned(),
                        name: format!("pane{n}"),
                        task: None,
                        cwd: "/home/dev".to_owned(),
                    })
                    .collect(),
            ))
            .expect("a fourth pane applies");
        state
            .apply_at(
                attention_projection(
                    &(1..=4)
                        .map(|n| (PaneId::new(n), AttentionState::NeedsInput, false))
                        .collect::<Vec<_>>(),
                ),
                now,
            )
            .expect("four actionable events apply");
        assert_eq!(state.toasts.len(), TOAST_CAPACITY, "a burst never grows");
        assert_eq!(
            toast_stack(&state)
                .iter()
                .map(|toast| toast.0)
                .collect::<Vec<_>>(),
            vec![2, 3, 4],
            "the oldest is the one evicted"
        );
        let raised = frame_text(&state);
        assert!(
            raised.iter().any(|row| row == "pane4 ! needs input"),
            "a raised notice reaches the frame as its own floating span: {raised:?}"
        );

        assert!(
            !state.tick_toasts(now),
            "a notice inside its lifetime asks for no repaint"
        );
        assert!(
            state.tick_toasts(now + crate::chrome::TOAST_LIFETIME),
            "the deadline passing changes the frame"
        );
        assert!(state.toasts.is_empty());
        assert!(state.toast_spans().is_empty());
        assert!(
            !frame_text(&state)
                .iter()
                .any(|row| row == "pane4 ! needs input"),
            "an expired notice leaves the frame with it"
        );
    }

    /// The rule the frame cap exists for, applied to notices: a pane's output
    /// raises nothing and animates nothing.
    #[test]
    fn a_toast_never_rides_a_panes_output_clock() {
        let now = Instant::now();
        let mut state = nested_toast_state(Size::new(60, 14), truecolor_caps());
        state
            .apply_at(
                attention_projection(&[(PaneId::new(2), AttentionState::NeedsInput, false)]),
                now,
            )
            .expect("the attention applies");
        let raised = toast_stack(&state);

        for _ in 0..500 {
            state
                .apply_at(
                    ServerMessage::Damage {
                        pane: PaneId::new(1),
                        rows: vec![RowUpdate {
                            row: 0,
                            cells: vec![Cell::default(); 30],
                        }],
                    },
                    now,
                )
                .expect("the damage applies");
        }
        assert_eq!(
            toast_stack(&state),
            raised,
            "pane output is not an attention event"
        );

        // And the entrance advances at most once per frame budget however often
        // the client is asked to draw.
        let mut frames = 0;
        for n in 0..1000 {
            if state.tick_toasts(now + Duration::from_micros(n * 200)) {
                frames += 1;
            }
        }
        assert!(
            frames <= usize::from(crate::motion::MOTION_STEPS),
            "{frames} frames for one entrance"
        );
    }

    /// Card 03's floating stack: upper-right, inside the two always-on chrome
    /// rows, and never on the line the focused harness is being typed into.
    #[test]
    fn a_nested_live_frame_stacks_toasts_in_the_upper_right_safe_area() {
        let now = Instant::now();
        let outer = Size::new(60, 14);
        let mut state = nested_toast_state(outer, truecolor_caps());
        state
            .apply(ServerMessage::CursorMoved {
                pane: PaneId::new(1),
                pos: Point::new(0, 0),
                shape: CursorShape::Block,
                visible: true,
            })
            .expect("the cursor applies");
        let cursor = state.cursor().expect("the focused pane has a cursor");
        state
            .apply_at(
                attention_projection(&[
                    (PaneId::new(2), AttentionState::NeedsInput, false),
                    (PaneId::new(3), AttentionState::Failed, false),
                ]),
                now,
            )
            .expect("the attention applies");

        let spans = state.toast_spans();
        assert_eq!(spans.len(), 2, "both notices are placed");
        for span in &spans {
            let len = u16::try_from(span.cells.len()).expect("a short notice");
            assert!(span.at.row > 0, "the tab row is never covered");
            assert!(
                span.at.row < outer.rows - 1,
                "neither is the status row: {span:?}"
            );
            assert_ne!(
                span.at.row, cursor.pos.row,
                "a notice never covers the focused input row"
            );
            assert_eq!(
                span.at.col + len + crate::chrome::TOAST_MARGIN,
                outer.cols,
                "the stack floats against the right edge"
            );
        }

        let frame = state.frame();
        let toasts = frame.chrome.len() - spans.len();
        assert!(
            frame.chrome[toasts..].iter().all(|chrome| *chrome),
            "a notice floating over a pane is still client chrome, never pane content"
        );
    }

    /// The 16-colour fallback says the same thing, and neither palette gives the
    /// stack a keyboard: a notice owns no keys, so input still reaches the pane.
    #[test]
    fn stacked_toasts_degrade_to_sixteen_colours_and_leak_no_input_to_a_pane() {
        let now = Instant::now();
        let raise = attention_projection(&[
            (PaneId::new(2), AttentionState::NeedsInput, false),
            (PaneId::new(3), AttentionState::Failed, false),
        ]);
        let mut truecolor = nested_toast_state(Size::new(60, 14), truecolor_caps());
        truecolor
            .apply_at(raise.clone(), now)
            .expect("the attention applies");
        let mut ansi = nested_toast_state(Size::new(60, 14), TermCaps::default());
        ansi.apply_at(raise, now).expect("the attention applies");

        assert_eq!(
            ansi.toast_spans()
                .iter()
                .map(|span| span.cells.iter().map(|cell| cell.ch).collect::<String>())
                .collect::<Vec<_>>(),
            truecolor
                .toast_spans()
                .iter()
                .map(|span| span.cells.iter().map(|cell| cell.ch).collect::<String>())
                .collect::<Vec<_>>(),
            "glyph and label never depend on the palette"
        );
        assert!(
            ansi.toast_spans()[0]
                .cells
                .iter()
                .any(|cell| matches!(cell.fg, cloo_proto::Color::Indexed(_))),
            "a 16-colour terminal gets indexed colour, not invented truecolour"
        );

        for keys in [b"j".as_slice(), b"\r", b"\x1b", b"a"] {
            assert_eq!(
                ansi.apply_overlay_keys(keys),
                OverlayKeys::Ignored,
                "{keys:?} belongs to the pane: a toast has no keyboard"
            );
        }
        assert!(ansi.overlay.is_none());
    }

    /// The client tracks its own request rather than reading a grid: the pane it
    /// asked for arriving clears the notice, and silence past the deadline turns
    /// it into a refusal the status row says out loud.
    #[test]
    fn a_launch_the_workspace_never_makes_becomes_a_visible_refusal() {
        let mut state = overlay_state(12, "C-b").profiles(Profile::built_ins());
        assert!(state.open_overlay(b"a", &Keymap::defaults()));
        let OverlayKeys::Launch(request) = state.apply_overlay_keys(b"\r") else {
            panic!("the first row confirms to a launch");
        };
        let sent = Instant::now();
        state.sent_launch(&request, sent);

        assert!(
            !state.tick_launch(sent),
            "a launch still inside its deadline is not a refusal"
        );
        assert!(
            state.tick_launch(sent + crate::overlay::LAUNCH_DEADLINE),
            "the deadline passing changes what the notice says"
        );
        let notice = state.launch.as_ref().expect("the refusal is still showing");
        assert!(notice.refused());
        assert_eq!(notice.text(), "generic did not start");
        let drawn: Vec<String> = state
            .frame()
            .spans
            .iter()
            .map(|span| span.cells.iter().map(|cell| cell.ch).collect())
            .collect();
        assert!(
            drawn
                .iter()
                .any(|row| row.contains("generic did not start")),
            "the refusal must reach the frame: {drawn:?}"
        );

        // And it does not stay forever: the linger is what keeps a notice from
        // covering a harness the user is typing into.
        assert!(
            state.tick_launch(
                sent + crate::overlay::LAUNCH_DEADLINE + crate::overlay::NOTICE_LINGER
            )
        );
        assert!(state.launch.is_none());
    }

    /// The other outcome: a *new* pane carrying the profile is the launch
    /// answering for itself, while a pane that was already there is not.
    #[test]
    fn only_a_pane_the_client_had_not_seen_settles_its_own_launch() {
        let mut state = overlay_state(12, "C-b").profiles(Profile::built_ins());
        assert!(state.open_overlay(b"a", &Keymap::defaults()));
        let OverlayKeys::Launch(request) = state.apply_overlay_keys(b"\r") else {
            panic!("the first row confirms to a launch");
        };
        // `overlay_state` already cached pane 1 as a `generic` pane, so a
        // resend of that same list must not be read as the launch arriving.
        state.sent_launch(&request, Instant::now());
        state
            .apply(ServerMessage::Panes(vec![launched(PaneId::new(1))]))
            .expect("the identity applies");
        assert!(
            state.launch.is_some(),
            "a pane the client already knew about cannot answer for a new launch"
        );

        state
            .apply(ServerMessage::Panes(vec![
                launched(PaneId::new(1)),
                launched(PaneId::new(2)),
            ]))
            .expect("the identity applies");
        assert!(
            state.launch.is_none(),
            "the pane the launch asked for arriving retires the notice"
        );
    }

    #[test]
    fn a_fresh_client_keeps_every_resolved_visual_preference() {
        let visual = VisualConfig {
            theme: cloo_core::ThemeChoice::Named(cloo_core::ThemeName::Nord),
            dim_unfocused: false,
            status: cloo_core::StatusMode::Powerline,
            motion: false,
            reduce_motion: false,
        };
        let caps = TermCaps {
            truecolor: true,
            ..TermCaps::default()
        };
        let state = LiveState::new(
            Size::new(40, 8),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        )
        .preferences(caps, visual);

        assert_eq!(state.visual, visual);
        assert_eq!(state.theme.choice(), visual.theme);
        assert!(!state.chrome_options().dim_unfocused);
        assert_eq!(state.visual.status, cloo_core::StatusMode::Powerline);
        assert_eq!(state.motion_settings(), MotionSettings::reduced());
    }

    #[test]
    fn rejected_and_duplicate_reload_revisions_preserve_the_previous_preferences() {
        let caps = TermCaps {
            truecolor: true,
            ..TermCaps::default()
        };
        let initial = VisualConfig {
            theme: cloo_core::ThemeChoice::Named(cloo_core::ThemeName::Night),
            ..VisualConfig::defaults()
        };
        let mut state = LiveState::new(
            Size::new(40, 8),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        )
        .preferences(caps, initial);

        assert!(!state.reload_visual(1, None));
        assert_eq!(state.visual, initial);
        assert_eq!(state.theme.choice(), initial.theme);
        assert!(
            !state.reload_visual(1, Some(VisualConfig::defaults())),
            "fixing a file after a rejected revision needs a newer daemon revision"
        );
        assert_eq!(state.visual, initial);
    }

    #[test]
    fn a_new_valid_revision_replaces_visual_preferences_as_one_value() {
        let caps = TermCaps {
            truecolor: true,
            ..TermCaps::default()
        };
        let mut state = LiveState::new(
            Size::new(40, 8),
            SessionId::new(1),
            hello_tabs(),
            "C-b".to_owned(),
        )
        .preferences(caps, VisualConfig::defaults());
        let replacement = VisualConfig {
            theme: cloo_core::ThemeChoice::Terminal,
            dim_unfocused: false,
            status: cloo_core::StatusMode::Powerline,
            motion: true,
            reduce_motion: true,
        };

        assert!(state.reload_visual(7, Some(replacement)));
        assert_eq!(state.visual, replacement);
        assert_eq!(state.theme.choice(), cloo_core::ThemeChoice::Terminal);
        assert!(!state.chrome_options().dim_unfocused);
        assert_eq!(state.motion_settings(), MotionSettings::reduced());
        assert_eq!(state.config_revision, 7);
    }

    #[test]
    fn two_clients_can_resolve_different_themes_for_one_session() {
        let caps = TermCaps {
            truecolor: true,
            ..TermCaps::default()
        };
        let session = SessionId::new(7);
        let storm = LiveState::new(Size::new(40, 8), session, hello_tabs(), "C-b".to_owned())
            .preferences(caps, VisualConfig::defaults());
        let nord_visual = VisualConfig {
            theme: cloo_core::ThemeChoice::Named(cloo_core::ThemeName::Nord),
            ..VisualConfig::defaults()
        };
        let nord = LiveState::new(Size::new(40, 8), session, hello_tabs(), "C-b".to_owned())
            .preferences(caps, nord_visual);

        assert_eq!(storm.tabs, nord.tabs);
        assert_ne!(storm.theme, nord.theme);
        assert_eq!(storm.visual.theme.as_str(), "storm");
        assert_eq!(nord.visual.theme.as_str(), "nord");
    }

    /// One `generic` pane as the server would report it.
    fn launched(pane: PaneId) -> PaneInfo {
        PaneInfo {
            pane,
            profile: "generic".to_owned(),
            name: "shell".to_owned(),
            task: None,
            cwd: "/home/dev".to_owned(),
        }
    }

    fn hello_tabs() -> Vec<TabSummary> {
        vec![TabSummary {
            tab: TabId::new(1),
            title: "shell".into(),
            active: true,
        }]
    }
}
