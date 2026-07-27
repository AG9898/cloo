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

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cloo_core::keymap::Keymap;
use cloo_proto::{
    Action, AttentionState, ClientMessage, CopyModeState, CursorShape, FrameStream, LayoutSnapshot,
    MouseEvent, PROTOCOL_VERSION, PaneAttention, PaneId, PaneInfo, PaneModes, Point, ProtoError,
    ServerMessage, SessionId, Size, StreamError, TabSummary, TermCaps, check_version,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::capabilities::{CapsError, detect_attach_caps};
use crate::chrome::{Attention, AttentionQueue, ChromeOptions, PaneChrome, PrefixHint};
use crate::copy_mode::{highlight_spans, status_span as copy_status_span};
use crate::effects::{EffectPolicy, apply_effect};
use crate::input::{
    ChromeAction, ChromeMouse, InputDecoder, InputEvent, KeyRoute, KeyRouter, MouseRoute,
    OuterModes, PaneArea, ScreenLayout, overlay_action, route_mouse,
};
use crate::motion::{Motion, MotionKind, Phase};
use crate::outer::current_size;
use crate::overlay::{
    DETAILS_KEY, HELP_KEY, Overlay, OverlayOutcome, PaneDetails, SESSIONS_KEY, SessionEntry,
    backdrop_span, overlay_spans,
};
use crate::raw_mode::{RawMode, RawModeError};
use crate::renderer::{Cursor, FramePane, Grid, RenderError, Renderer, compose_frame};
use crate::resize::ResizeWatch;
use crate::theme::{Theme, ThemeToken};

/// The render tick shared with the daemon: roughly 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Size of a single blocking stdin read on the helper thread.
const INPUT_BUF_LEN: usize = 1024;
/// The chrome rows a one-pane attached frame reserves: tab, header, status.
const CHROME_ROWS: u16 = 3;

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
        if let Some(ServerMessage::Tabs(tabs)) = &message {
            self.tabs = tabs.clone();
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
        self.size = size;
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
/// The caller supplies the resolved keymap so the client applies the same
/// configurable prefix table it advertises in its chrome. Entering raw mode,
/// enabling outer-terminal reporting, restoring both on every exit path, and
/// owning the render loop all live here because they are client concerns — the
/// daemon never writes a byte to the user's terminal.
///
/// # Errors
///
/// Returns an actionable attach, terminal, rendering, or signal error. The raw
/// mode guard restores the terminal before an error reaches the caller.
pub fn run(path: &Path, keymap: Keymap) -> Result<i32, AttachRunError> {
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
    let result = runtime.block_on(live_loop(path, outer_size, caps, modes, keymap));

    // Restore before `main` prints an error: diagnostics written while raw are
    // unreadable, and `RawMode` also turns reporting modes back off here.
    let restored = raw.restore().map_err(AttachRunError::RawMode);
    let status = result?;
    restored?;
    Ok(status)
}

/// A terminal's usable session area after the frame's fixed chrome rows.
///
/// A client reports the pane area rather than its full outer-terminal height:
/// tab, pane-header, and status rows are client-owned and must not become a
/// child's last three grid rows. The server remains authoritative for how that
/// usable rectangle is divided among panes.
const fn session_size(outer: Size) -> Size {
    Size::new(outer.cols, outer.rows.saturating_sub(CHROME_ROWS))
}

/// The async attached-client body, entered only after raw mode is armed.
async fn live_loop(
    path: &Path,
    outer_size: Size,
    caps: TermCaps,
    modes: OuterModes,
    keymap: Keymap,
) -> Result<i32, AttachRunError> {
    // Install this before attaching, so the one `SIGWINCH` that happens while
    // the socket handshake is in flight cannot be lost.
    let mut resizes = ResizeWatch::new(outer_size).map_err(AttachRunError::Signal)?;
    // The chrome shows the chord this client actually resolved, so a rebound
    // prefix is discoverable from the first frame rather than only from the
    // configuration file.
    let prefix = keymap.prefix().to_string();
    let mut attached = attach(path, session_size(outer_size), caps, None).await?;
    let mut state = LiveState::new(
        outer_size,
        attached.session(),
        attached.tabs().to_vec(),
        prefix,
    );
    let mut renderer = Renderer::new(caps);
    let mut out = io::stdout();
    let policy = EffectPolicy::default();
    let mut input = spawn_input_reader();
    let mut input_open = true;
    let mut decoder = InputDecoder::new(modes);
    let mut keys = KeyRouter::new(keymap);
    let mut chrome = ChromeMouse::new();
    let mut frames = tokio::time::interval(FRAME_INTERVAL);
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut motion = Motion::default();
    let mut phase = None;
    let mut dirty = true;
    let mut detaching = false;

    loop {
        let step = tokio::select! {
            message = attached.recv() => Step::Server(message.map_err(AttachError::from)?),
            received = input.recv(), if input_open && !detaching => match received {
                Some(bytes) => Step::Input(bytes),
                None => Step::InputClosed,
            },
            resized = resizes.changed(), if !detaching => Step::Resized(resized),
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
                return Ok(status);
            }
            Step::Server(Some(ServerMessage::Detached)) | Step::Server(None) => return Ok(0),
            Step::Server(Some(message)) => {
                let transition = state.transition_for(&message);
                dirty |= state.apply(message)?;
                state.tabs = attached.tabs().to_vec();
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
                if route(
                    &mut attached,
                    &mut state,
                    &mut chrome,
                    &mut keys,
                    &mut decoder,
                    bytes,
                )
                .await?
                {
                    detaching = true;
                }
                state.prefix_pending = keys.is_pending();
                if overlay_was_open != state.overlay.is_some() {
                    phase = Some(motion.start(MotionKind::Overlay, Instant::now()));
                }
                dirty = true;
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
                    if route_keys(&mut attached, &mut keys, &mut state, bytes).await? {
                        detaching = true;
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
                if dirty {
                    draw(&mut out, &mut renderer, &state, phase)?;
                    dirty = false;
                }
            }
        }
    }
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
}

/// The client-owned projection of the server's latest frame.
///
/// This is deliberately a cache, not another session model. The server decides
/// every pane rectangle and state transition; this structure only joins those
/// independent wire clocks into the grids, headers, cursor, and hit-test map
/// that one terminal frame needs.
struct LiveState {
    outer_size: Size,
    session: SessionId,
    tabs: Vec<TabSummary>,
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
    screen: ScreenLayout,
    /// The configured prefix's spelling, as the status row must draw it.
    prefix: String,
    /// Whether the router is holding a prefix and owns the next chord.
    prefix_pending: bool,
}

impl LiveState {
    fn new(outer_size: Size, session: SessionId, tabs: Vec<TabSummary>, prefix: String) -> Self {
        Self {
            outer_size,
            session,
            tabs,
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
            screen: ScreenLayout::new(outer_size)
                .tab_row(0)
                .status_row(outer_size.rows.saturating_sub(1)),
            prefix,
            prefix_pending: false,
        }
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

    /// Applies one server clock tick and reports whether it changes the frame.
    fn apply(&mut self, message: ServerMessage) -> Result<bool, RenderError> {
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
                Ok(true)
            }
            ServerMessage::Panes(panes) => {
                self.panes = panes.into_iter().map(|pane| (pane.pane, pane)).collect();
                self.rebuild_queue();
                Ok(true)
            }
            ServerMessage::Attention(attention) => {
                self.attention = attention
                    .into_iter()
                    .map(|state| (state.pane, state))
                    .collect();
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
            ServerMessage::Hello { .. }
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
            // The server was given the usable area (without the fixed tab,
            // header, and status rows), so its grid origin is shifted below the
            // first pane header when it is painted in the outer terminal.
            let area = PaneArea::new(
                rect.pane,
                rect.x,
                rect.y.saturating_add(CHROME_ROWS - 1),
                rect.size,
            );
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

    /// Draws an attention queue only from the complete server projection.
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
        let mut spans = compose_frame(
            self.outer_size,
            &self.tabs,
            self.session,
            &panes,
            &self.queue,
            &self.prefix_hint(),
            ChromeOptions::default(),
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
                    Theme::storm(),
                ));
                spans.push(copy_status_span(
                    Point::new(0, self.outer_size.rows.saturating_sub(1)),
                    copy_mode,
                    self.outer_size.cols,
                    Theme::storm(),
                ));
            }
        }
        spans
    }

    /// Composes normal pane contents plus client-owned visual layers.
    fn frame(&self) -> RenderFrame {
        let base = self.spans();
        let mut chrome: Vec<bool> = base.iter().map(|span| !self.is_body_span(span)).collect();
        let mut spans = if self.overlay.is_some() {
            base.iter()
                .map(|span| backdrop_span(span.at, &span.cells, Theme::storm()))
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
            let overlays = overlay_spans(at, overlay, size, Theme::storm());
            chrome.extend(std::iter::repeat_n(true, overlays.len()));
            spans.extend(overlays);
        }

        RenderFrame { spans, chrome }
    }

    /// Starts a client-local overlay after an otherwise unbound prefix chord.
    ///
    /// These shortcuts never cross the wire. The help surface is read from the
    /// very keymap the router resolved this chord with, the session switcher can
    /// honestly list only the session this daemon exposed, and pane details are
    /// built solely from the server metadata and attention already cached
    /// locally. A chord only reaches here when the keymap left it unbound, which
    /// is why a user who binds `?` keeps their binding.
    fn open_overlay(&mut self, chord: &[u8], keymap: &Keymap) -> bool {
        let key = (chord.len() == 1).then(|| char::from(chord[0]));
        let next = match key {
            Some(HELP_KEY) => Some(Overlay::help(keymap)),
            Some(SESSIONS_KEY) => Some(Overlay::sessions(vec![
                SessionEntry::new(self.session, "current", self.areas.len()).attached(true),
            ])),
            Some(DETAILS_KEY) => self
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
            _ => None,
        };
        if let Some(next) = next {
            self.overlay = Some(next);
            true
        } else {
            false
        }
    }

    /// Applies an open overlay's own keyboard vocabulary.
    fn apply_overlay_keys(&mut self, keys: &[u8]) -> bool {
        let Some(action) = overlay_action(keys) else {
            return false;
        };
        let Some(overlay) = self.overlay.as_mut() else {
            return false;
        };
        match overlay.apply(action) {
            OverlayOutcome::Open => true,
            OverlayOutcome::Dismissed
            | OverlayOutcome::SwitchSession(_)
            | OverlayOutcome::Launch(_) => {
                self.overlay = None;
                true
            }
        }
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

    /// The box an overlay is drawn in: as tall as its list, within bounds.
    ///
    /// The cap is generous enough for the whole help surface on an ordinary
    /// terminal — a user who pressed `?` because they do not know the keys is
    /// the last person who should have to scroll to find `detach` — and the
    /// window still follows the cursor on a short one.
    fn overlay_size(&self, overlay: &Overlay) -> Size {
        let rows = u16::try_from(overlay.len().saturating_add(2))
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
            Theme::storm().color(ThemeToken::Frame),
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
) -> Result<bool, AttachRunError> {
    let mut detach = false;
    for event in decoder.feed(&bytes) {
        match event {
            InputEvent::Keys(bytes) => detach |= route_keys(attached, keys, state, bytes).await?,
            InputEvent::Paste(text) => {
                attached.send_paste(text).await.map_err(AttachError::from)?
            }
            InputEvent::Focus(focused) => {
                if !focused {
                    keys.reset();
                }
                attached
                    .send_focus(focused)
                    .await
                    .map_err(AttachError::from)?;
            }
            InputEvent::Mouse(report) => {
                match route_mouse(&state.screen, state.focused_modes(), &report) {
                    MouseRoute::Application(event) => {
                        attached
                            .send_mouse(event)
                            .await
                            .map_err(AttachError::from)?;
                    }
                    MouseRoute::Chrome(target) => {
                        if let Some(action) = chrome.feed(&state.screen, target, &report) {
                            detach |= apply_chrome(
                                attached,
                                action,
                                state.copy_mode.as_ref().map(|copy_mode| copy_mode.pane),
                            )
                            .await?;
                        }
                    }
                }
            }
        }
    }
    Ok(detach)
}

/// Resolves prefix chords and sends the resulting application bytes or actions.
async fn route_keys(
    attached: &mut Attached<UnixStream>,
    keys: &mut KeyRouter,
    state: &mut LiveState,
    bytes: Vec<u8>,
) -> Result<bool, AttachRunError> {
    if state.overlay.is_some() {
        keys.reset();
        let _ = state.apply_overlay_keys(&bytes);
        return Ok(false);
    }

    let mut detach = false;
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
                detach = true;
            }
            KeyRoute::Command(action) => attached
                .send_command(action)
                .await
                .map_err(AttachError::from)?,
            KeyRoute::Unbound(chord) => {
                let _ = state.open_overlay(&chord, keys.keymap());
            }
            KeyRoute::Pending => {}
        }
    }
    Ok(detach)
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
    use crate::overlay::OverlayKind;
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
                    x: 0,
                    y: 0,
                    size: Size::new(8, 2),
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
                rows: vec![
                    RowUpdate {
                        row: 0,
                        cells: vec![
                            Cell {
                                ch: 'a',
                                ..Cell::default()
                            };
                            8
                        ],
                    },
                    RowUpdate {
                        row: 1,
                        cells: vec![
                            Cell {
                                ch: 'b',
                                ..Cell::default()
                            };
                            8
                        ],
                    },
                ],
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
            spans[2].at,
            Point::new(0, 2),
            "the grid starts below chrome"
        );
        assert_eq!(spans[2].cells[0].ch, 'a');
        assert_eq!(spans[3].at, Point::new(0, 3));
        assert_eq!(spans[3].cells[0].ch, 'b');
        assert_eq!(
            spans.last().map(|span| span.at),
            Some(Point::new(0, 4)),
            "the status row owns the last outer-terminal row"
        );
        assert_eq!(state.screen.hit(0, 2).pane(), Some(pane));
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
            row.contains("C-b ?") && !row.contains("split %"),
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
                    x: 0,
                    y: 0,
                    size: Size::new(9, 2),
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
                    cells: "copy this"
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
            span.at == Point::new(0, 2)
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
        assert!(state.apply_overlay_keys(b"j"), "an overlay owns navigation");

        let frame = state.frame();
        assert!(frame.spans.iter().any(|span| {
            span.cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
                .starts_with("  sessions 1/1")
        }));
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
                    x: 0,
                    y: 0,
                    size: Size::new(40, rows.saturating_sub(CHROME_ROWS)),
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

    /// The milestone's routing change: `?` is the help surface and `i` is still
    /// pane details, rather than both landing on details.
    #[test]
    fn the_help_key_opens_help_and_details_keeps_its_own_key() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(8, "C-b");

        assert!(state.open_overlay(b"?", &keymap));
        assert!(
            matches!(
                state.overlay.as_ref().map(Overlay::kind),
                Some(OverlayKind::Help(_))
            ),
            "the help key must no longer land on pane details"
        );
        assert!(state.apply_overlay_keys(b"\x1b"), "escape dismisses");
        assert!(state.overlay.is_none());

        assert!(state.open_overlay(b"i", &keymap));
        assert!(matches!(
            state.overlay.as_ref().map(Overlay::kind),
            Some(OverlayKind::Details(_))
        ));
    }

    /// The help surface is read from the client's live keymap, so the frame a
    /// user actually sees names the chord they actually configured.
    #[test]
    fn the_open_help_surface_draws_the_configured_prefix_and_its_controls() {
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
        for expected in ["keys - prefix M-a", "split right", "detach", "add pane"] {
            assert!(
                drawn.iter().any(|row| row.contains(expected)),
                "the help frame must show {expected:?}: {drawn:?}"
            );
        }
    }

    /// An open overlay owns the keyboard: nothing it consumes may reach a pane,
    /// which is why the router is reset and the bytes never become input.
    #[test]
    fn an_open_help_surface_consumes_its_keys_locally() {
        let keymap = Keymap::defaults();
        let mut state = overlay_state(8, "C-b");
        assert!(state.open_overlay(b"?", &keymap));
        for key in [b"j".as_slice(), b"k", b"g", b"\r"] {
            let consumed = state.apply_overlay_keys(key);
            assert!(consumed, "the overlay owns {key:?}");
        }
        assert!(
            state.overlay.is_none(),
            "enter closes a reading surface rather than acting"
        );
    }

    fn hello_tabs() -> Vec<TabSummary> {
        vec![TabSummary {
            tab: TabId::new(1),
            title: "shell".into(),
            active: true,
        }]
    }
}
