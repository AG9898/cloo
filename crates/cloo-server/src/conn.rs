//! One client connection: the handshake, and the snapshot that follows it.
//!
//! A connection is worthless until it has been through [`accept_attach`], which
//! is the only place the server decides whether a peer is speaking its
//! protocol. The rule is that nothing is interpreted before the version is
//! agreed: the first frame must be an [`ClientMessage::Attach`], and any
//! refusal is sent as [`ServerMessage::Refused`] with a rendered
//! [`ProtoError`] before the connection is dropped. A client that is told *why*
//! it was turned away can print something a user can act on; a client that just
//! sees a closed socket cannot.
//!
//! [`accept_adapter`] is the same rule for the other endpoint. An opt-in local
//! adapter connects to the control socket, announces itself, and is refused the
//! same way — but it speaks [`AdapterMessage`], a vocabulary with no variant
//! that could reach a child, so "an adapter may only report attention" needs no
//! enforcement branch anywhere in the server.
//!
//! [`session_snapshot`] is the other half of attach. A client caches the
//! visible grid and nothing else, so it needs a full picture the moment it
//! connects — geometry, contents, negotiated input modes, cursor — and it must
//! arrive as the same
//! message types an incremental update uses, so applying a resync and applying
//! damage stay one code path on the client.

use cloo_core::AdapterId;
use cloo_core::error::MetadataError;
use cloo_proto::{
    AdapterMessage, AdapterReply, ClientMessage, FrameStream, ProtoError, ServerMessage, SessionId,
    Size, StreamError, TermCaps, check_version,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::session::SessionSnapshot;

/// A client connection carrying framed wire messages.
pub type Connection<T> = FrameStream<T>;

/// What a client asked for when it attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachRequest {
    /// The client's terminal size.
    pub size: Size,
    /// What the client's terminal can do.
    pub term_caps: TermCaps,
    /// The session it asked for, or `None` for the default.
    pub session: Option<SessionId>,
}

/// Why an attach was refused.
#[derive(Debug)]
pub enum AttachRejection {
    /// The peer speaks a different protocol version.
    Version(ProtoError),
    /// The first frame was something other than an attach.
    NotAnAttach,
    /// The peer closed before saying anything.
    Closed,
    /// The connection failed while the handshake was in flight.
    Stream(StreamError),
}

impl core::fmt::Display for AttachRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Version(e) => write!(f, "{e}"),
            Self::NotAnAttach => f.write_str("the first message on a connection must be an attach"),
            Self::Closed => f.write_str("the client closed before attaching"),
            Self::Stream(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachRejection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Version(e) => Some(e),
            Self::Stream(e) => Some(e),
            Self::NotAnAttach | Self::Closed => None,
        }
    }
}

/// Reads the first frame and validates it as an attach.
///
/// On a version mismatch or a wrong first message, a
/// [`ServerMessage::Refused`] carrying the rendered reason is sent before the
/// error is returned, so the caller can simply drop the connection.
///
/// # Errors
///
/// Returns the [`AttachRejection`] that was reported to the client, or the
/// transport failure that prevented reporting one.
pub async fn accept_attach<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
) -> Result<AttachRequest, AttachRejection> {
    let first = match conn.recv::<ClientMessage>().await {
        Ok(Some(message)) => message,
        Ok(None) => return Err(AttachRejection::Closed),
        Err(err) => return Err(AttachRejection::Stream(err)),
    };

    let rejection = match first {
        ClientMessage::Attach {
            protocol_version,
            size,
            term_caps,
            session,
        } => match check_version(protocol_version) {
            Ok(()) => {
                return Ok(AttachRequest {
                    size,
                    term_caps,
                    session,
                });
            }
            Err(mismatch) => AttachRejection::Version(mismatch),
        },
        _ => AttachRejection::NotAnAttach,
    };

    // Best effort: the peer may already be gone, and the refusal it is being
    // sent is the more useful thing to report either way.
    let _ = refuse(conn, &rejection.to_string()).await;
    Err(rejection)
}

/// Why a control connection was refused.
#[derive(Debug)]
pub enum ControlRejection {
    /// The peer speaks a different protocol version.
    Version(ProtoError),
    /// The first frame was something other than a hello.
    NotAHello,
    /// The announced name is not a usable adapter ID.
    BadAdapterId(MetadataError),
    /// The peer closed before saying anything.
    Closed,
    /// The connection failed while the handshake was in flight.
    Stream(StreamError),
}

impl core::fmt::Display for ControlRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Version(e) => write!(f, "{e}"),
            Self::NotAHello => {
                f.write_str("the first message on a control connection must be a hello")
            }
            Self::BadAdapterId(e) => write!(f, "unusable adapter id: {e}"),
            Self::Closed => f.write_str("the adapter closed before announcing itself"),
            Self::Stream(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ControlRejection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Version(e) => Some(e),
            Self::BadAdapterId(e) => Some(e),
            Self::Stream(e) => Some(e),
            Self::NotAHello | Self::Closed => None,
        }
    }
}

/// Reads the first frame on the control socket and validates it as a hello.
///
/// The adapter's name is validated here, before it can be attached to any
/// claim: provenance is rendered in pane chrome, so a name is user-facing text
/// and goes through the same [`AdapterId`] alphabet a profile's does. Nothing
/// else about the adapter is checked — whether it may speak for a *pane* is the
/// session's answer, one report at a time, since panes come and go while the
/// connection stays open.
///
/// # Errors
///
/// Returns the [`ControlRejection`] that was reported to the adapter, or the
/// transport failure that prevented reporting one.
pub async fn accept_adapter<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
) -> Result<AdapterId, ControlRejection> {
    let first = match conn.recv::<AdapterMessage>().await {
        Ok(Some(message)) => message,
        Ok(None) => return Err(ControlRejection::Closed),
        Err(err) => return Err(ControlRejection::Stream(err)),
    };

    let rejection = match first {
        AdapterMessage::Hello {
            protocol_version,
            adapter,
        } => match check_version(protocol_version) {
            Ok(()) => match AdapterId::new(adapter) {
                Ok(id) => return Ok(id),
                Err(bad) => ControlRejection::BadAdapterId(bad),
            },
            Err(mismatch) => ControlRejection::Version(mismatch),
        },
        AdapterMessage::Report { .. } => ControlRejection::NotAHello,
    };

    // Best effort, as with an attach: the reason is the useful half, and the
    // peer may already be gone.
    let _ = refuse_adapter(conn, &rejection.to_string()).await;
    Err(rejection)
}

/// Tells an adapter why it is not being served, and closes the write half.
///
/// # Errors
///
/// Returns the transport failure. The caller drops the connection regardless.
pub async fn refuse_adapter<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    reason: &str,
) -> Result<(), StreamError> {
    conn.send(&AdapterReply::Refused {
        reason: reason.to_owned(),
    })
    .await?;
    conn.shutdown().await
}

/// Tells a client why it is not being served, and closes the write half.
///
/// # Errors
///
/// Returns the transport failure. The caller drops the connection regardless.
pub async fn refuse<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    reason: &str,
) -> Result<(), StreamError> {
    conn.send(&ServerMessage::Refused {
        reason: reason.to_owned(),
    })
    .await?;
    conn.shutdown().await
}

/// The messages that bring a freshly attached client fully up to date.
///
/// Tab state first, then geometry, then who the panes are, their attention and
/// copy state, then contents, the modes, and the cursor. That order
/// is what lets a
/// client apply the batch without ever holding rows it has nowhere to put: the
/// [`ServerMessage::Layout`] tells it how big the pane is before a single
/// [`ServerMessage::Damage`] row arrives, and the
/// [`ServerMessage::Panes`] tells it what to write in a pane header before it
/// has anything to draw one around.
///
/// The geometry is the session task's own layout pass, carried through
/// untouched. Recomputing it here would be a second answer to a question that
/// already has one, and the two could disagree mid-resize.
#[must_use]
pub fn session_snapshot(snapshot: &SessionSnapshot) -> Vec<ServerMessage> {
    let pane = snapshot.focused;
    let mut messages = vec![
        ServerMessage::Tabs(snapshot.tabs.clone()),
        ServerMessage::Layout(cloo_proto::LayoutSnapshot {
            tab: snapshot.tab,
            panes: snapshot.panes.clone(),
            focused: Some(pane),
            zoomed: snapshot.zoomed,
        }),
        ServerMessage::Panes(snapshot.metas.clone()),
        // Attention rides with identity on the resync: the chrome needs a pane's
        // state and provenance to draw its header, and cannot derive either from
        // the grid.
        ServerMessage::Attention(snapshot.attention.clone()),
        // Copy positions are server-owned scrollback state too. Sending an
        // explicit inactive value clears a stale copy view after a resync.
        ServerMessage::CopyMode(snapshot.copy_mode.clone()),
        ServerMessage::Damage {
            pane,
            rows: snapshot.pane.rows.clone(),
        },
        // Before the client can route a click: the modes are what tell it
        // whether the application owns the mouse or cloo's chrome does.
        ServerMessage::Modes {
            pane,
            modes: snapshot.modes,
        },
    ];
    if let Some((pos, shape)) = snapshot.pane.cursor {
        messages.push(ServerMessage::CursorMoved {
            pane,
            pos,
            shape,
            visible: true,
        });
    } else {
        messages.push(ServerMessage::CursorMoved {
            pane,
            pos: cloo_proto::Point::new(0, 0),
            shape: cloo_proto::CursorShape::default(),
            visible: false,
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::PaneSnapshot;
    use cloo_proto::{
        Cell, CursorShape, PROTOCOL_VERSION, PaneId, PaneRect, Point, RowUpdate, TabId, TabSummary,
    };
    use tokio::io::duplex;

    fn snapshot() -> SessionSnapshot {
        let pane = PaneId::new(2);
        SessionSnapshot {
            tab: TabId::new(0),
            tabs: vec![TabSummary {
                tab: TabId::new(0),
                title: "api".into(),
                active: true,
            }],
            area: Size::new(2, 1),
            panes: vec![PaneRect {
                pane,
                x: 0,
                y: 0,
                size: Size::new(2, 1),
            }],
            metas: vec![cloo_proto::PaneInfo {
                pane,
                profile: "codex".into(),
                name: "api".into(),
                task: Some("fix the flaky test".into()),
                cwd: "/home/dev/api".into(),
            }],
            attention: vec![cloo_proto::PaneAttention {
                pane,
                state: cloo_proto::AttentionState::Unknown,
                source: cloo_proto::AttentionSource::None,
                acknowledged: false,
            }],
            copy_mode: None,
            focused: pane,
            zoomed: None,
            pane: PaneSnapshot {
                size: Size::new(2, 1),
                rows: vec![RowUpdate {
                    row: 0,
                    cells: vec![Cell::default(), Cell::default()],
                }],
                cursor: Some((Point::new(1, 0), CursorShape::Block)),
            },
            modes: cloo_proto::PaneModes::default(),
        }
    }

    #[tokio::test]
    async fn a_matching_attach_is_accepted_with_its_capabilities_intact() {
        let (client, server) = duplex(1024);
        let mut client = Connection::new(client);
        let mut server = Connection::new(server);

        // A mixed set rather than all-true or the default, so a handshake that
        // dropped a field or substituted a default is caught here.
        let term_caps = TermCaps {
            truecolor: true,
            bracketed_paste: true,
            sgr_mouse: false,
            focus_events: true,
            extended_keys: false,
            clipboard_osc52: true,
            hyperlinks: false,
            graphics: false,
        };

        client
            .send(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                size: Size::new(100, 30),
                term_caps,
                session: Some(SessionId::new(3)),
            })
            .await
            .expect("attach sends");

        let request = accept_attach(&mut server)
            .await
            .expect("attach is accepted");
        assert_eq!(request.size, Size::new(100, 30));
        assert_eq!(
            request.term_caps, term_caps,
            "the server serves what the client reported, never a guess"
        );
        assert_eq!(request.session, Some(SessionId::new(3)));
    }

    #[tokio::test]
    async fn a_version_mismatch_is_refused_with_an_actionable_reason() {
        let (client, server) = duplex(1024);
        let mut client = Connection::new(client);
        let mut server = Connection::new(server);

        client
            .send(&ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION.wrapping_add(1),
                size: Size::new(80, 24),
                term_caps: TermCaps::default(),
                session: None,
            })
            .await
            .expect("attach sends");

        let err = accept_attach(&mut server)
            .await
            .expect_err("a mismatched version must be refused");
        assert!(matches!(err, AttachRejection::Version(_)), "got {err}");

        let reply: Option<ServerMessage> = client.recv().await.expect("a refusal arrives");
        let Some(ServerMessage::Refused { reason }) = reply else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert!(reason.contains("version mismatch"), "got: {reason}");
        assert!(reason.contains("reattach"), "got: {reason}");
    }

    #[tokio::test]
    async fn a_first_message_that_is_not_an_attach_is_refused() {
        let (client, server) = duplex(1024);
        let mut client = Connection::new(client);
        let mut server = Connection::new(server);

        client
            .send(&ClientMessage::Input(vec![b'x']))
            .await
            .expect("input sends");

        let err = accept_attach(&mut server)
            .await
            .expect_err("input before attach must be refused");
        assert!(matches!(err, AttachRejection::NotAnAttach), "got {err}");

        let reply: Option<ServerMessage> = client.recv().await.expect("a refusal arrives");
        let Some(ServerMessage::Refused { reason }) = reply else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert!(reason.contains("must be an attach"), "got: {reason}");
    }

    #[tokio::test]
    async fn a_peer_that_says_nothing_is_a_close_not_a_fault() {
        let (client, server) = duplex(1024);
        let mut server = Connection::new(server);
        drop(client);

        let err = accept_attach(&mut server)
            .await
            .expect_err("a silent close is not an attach");
        assert!(
            matches!(err, AttachRejection::Closed),
            "a peer that went away is not a refusal, got {err}"
        );
    }

    #[tokio::test]
    async fn an_adapter_that_announces_itself_is_accepted_under_its_own_name() {
        let (adapter, server) = duplex(1024);
        let mut adapter = Connection::new(adapter);
        let mut server = Connection::new(server);

        adapter
            .send(&cloo_proto::AdapterMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                adapter: "my-adapter".to_owned(),
            })
            .await
            .expect("hello sends");

        let id = accept_adapter(&mut server)
            .await
            .expect("a matching hello is accepted");
        assert_eq!(id.as_str(), "my-adapter");
    }

    #[tokio::test]
    async fn an_adapter_that_reports_before_saying_who_it_is_is_refused() {
        // Provenance is the whole value of an advisory claim, so a report from
        // an anonymous connection can never be attributed and is never applied.
        let (adapter, server) = duplex(1024);
        let mut adapter = Connection::new(adapter);
        let mut server = Connection::new(server);

        adapter
            .send(&cloo_proto::AdapterMessage::Report {
                pane: PaneId::new(1),
                state: cloo_proto::AdapterState::NeedsInput,
            })
            .await
            .expect("report sends");

        let err = accept_adapter(&mut server)
            .await
            .expect_err("a report before a hello must be refused");
        assert!(matches!(err, ControlRejection::NotAHello), "got {err}");

        let reply: Option<cloo_proto::AdapterReply> =
            adapter.recv().await.expect("a refusal arrives");
        let Some(cloo_proto::AdapterReply::Refused { reason }) = reply else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert!(reason.contains("must be a hello"), "got: {reason}");
    }

    #[tokio::test]
    async fn an_adapter_on_another_protocol_version_is_refused_with_a_reason() {
        let (adapter, server) = duplex(1024);
        let mut adapter = Connection::new(adapter);
        let mut server = Connection::new(server);

        adapter
            .send(&cloo_proto::AdapterMessage::Hello {
                protocol_version: PROTOCOL_VERSION.wrapping_add(1),
                adapter: "my-adapter".to_owned(),
            })
            .await
            .expect("hello sends");

        let err = accept_adapter(&mut server)
            .await
            .expect_err("a mismatched version must be refused");
        assert!(matches!(err, ControlRejection::Version(_)), "got {err}");

        let reply: Option<cloo_proto::AdapterReply> =
            adapter.recv().await.expect("a refusal arrives");
        let Some(cloo_proto::AdapterReply::Refused { reason }) = reply else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert!(reason.contains("version mismatch"), "got: {reason}");
    }

    #[tokio::test]
    async fn an_adapter_name_that_could_not_be_a_profile_id_is_refused() {
        // The name is rendered in pane chrome as provenance, so it goes through
        // the same alphabet a profile's adapter field does.
        let (adapter, server) = duplex(1024);
        let mut adapter = Connection::new(adapter);
        let mut server = Connection::new(server);

        adapter
            .send(&cloo_proto::AdapterMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                adapter: "Not An\u{1b}Id".to_owned(),
            })
            .await
            .expect("hello sends");

        let err = accept_adapter(&mut server)
            .await
            .expect_err("an unusable name must be refused");
        assert!(
            matches!(err, ControlRejection::BadAdapterId(_)),
            "got {err}"
        );
    }

    #[test]
    fn a_snapshot_describes_geometry_before_contents() {
        let messages = session_snapshot(&snapshot());
        assert!(
            matches!(messages.first(), Some(ServerMessage::Tabs(_))),
            "tabs must arrive before the active layout"
        );
        assert!(matches!(messages.get(1), Some(ServerMessage::Layout(_))));
        assert!(matches!(messages.get(2), Some(ServerMessage::Panes(_))));
        assert!(matches!(messages.get(3), Some(ServerMessage::Attention(_))));
        assert!(matches!(
            messages.get(4),
            Some(ServerMessage::CopyMode(None))
        ));
        assert!(matches!(
            messages.get(5),
            Some(ServerMessage::Damage { rows, .. }) if rows.len() == 1
        ));
        assert!(matches!(messages.get(6), Some(ServerMessage::Modes { .. })));
        assert!(matches!(
            messages.get(7),
            Some(ServerMessage::CursorMoved { visible: true, .. })
        ));
    }

    #[test]
    fn a_freshly_attached_client_is_told_who_every_pane_is() {
        // A client caches the visible grid and nothing else, so identity has to
        // arrive with the resync or its chrome has nothing to write.
        let snapshot = snapshot();
        let messages = session_snapshot(&snapshot);
        let Some(ServerMessage::Panes(panes)) = messages.get(2) else {
            panic!("the resync must carry pane identity");
        };
        assert_eq!(panes, &snapshot.metas);
        assert_eq!(panes[0].task.as_deref(), Some("fix the flaky test"));
    }

    #[test]
    fn the_layout_pass_is_carried_through_rather_than_recomputed() {
        let mut snapshot = snapshot();
        // Geometry the session resolved that a naive "one pane fills the area"
        // rebuild here would silently discard.
        snapshot.panes[0].x = 7;
        let messages = session_snapshot(&snapshot);
        let Some(ServerMessage::Layout(layout)) = messages.get(1) else {
            panic!("layout must come first");
        };
        assert_eq!(layout.panes, snapshot.panes);
        assert_eq!(layout.focused, Some(snapshot.focused));
    }

    #[test]
    fn a_hidden_cursor_is_still_reported() {
        let mut snapshot = snapshot();
        snapshot.pane.cursor = None;
        let messages = session_snapshot(&snapshot);
        assert!(
            matches!(
                messages.get(7),
                Some(ServerMessage::CursorMoved { visible: false, .. })
            ),
            "a client with a stale cursor must be told to stop drawing it"
        );
    }
}
