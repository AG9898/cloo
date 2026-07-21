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

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use cloo_proto::{
    ClientMessage, FrameStream, PROTOCOL_VERSION, ProtoError, ServerMessage, SessionId, Size,
    StreamError, TabSummary, TermCaps, check_version,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use crate::capabilities::CapsError;

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
        self.conn.recv().await
    }

    /// Sends keyboard bytes to the focused pane.
    ///
    /// # Errors
    ///
    /// Returns the transport failure.
    pub async fn send_input(&mut self, bytes: Vec<u8>) -> Result<(), StreamError> {
        self.conn.send(&ClientMessage::Input(bytes)).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_proto::{PaneId, TabId};
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
}
