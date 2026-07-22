//! Length-prefixed postcard framing, and the version handshake check.
//!
//! Every frame on the socket is a big-endian `u32` payload length followed by
//! that many bytes of postcard. Postcard is not self-delimiting for a stream, so
//! the prefix is what lets a reader know when it holds a whole message.
//!
//! ```text
//! +--------+--------+--------+--------+-- ... --+
//! |          len: u32 (BE)            | payload |
//! +--------+--------+--------+--------+-- ... --+
//! ```

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ProtoError;

/// The wire protocol version.
///
/// **Bump this on every change to a type in [`crate::message`] or
/// [`crate::adapter`].** A stale client attached to a rebuilt server is a
/// routine occurrence, and a clean "version mismatch, reattach" beats a desync
/// that presents as a rendering bug. Adapters share the number rather than
/// carrying one of their own: both protocols are built from this one crate, so
/// two versions could only ever disagree by accident.
pub const PROTOCOL_VERSION: u16 = 8;

/// Width of the length prefix, in bytes.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// The largest payload this build will accept, in bytes.
///
/// A frame claiming more than this is a desync or a hostile peer, so the length
/// is rejected before anything is allocated for it. 16 MiB is far above any
/// legitimate message: a full-screen damage burst on a very large terminal is
/// orders of magnitude smaller.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Verifies that a peer speaks the same protocol version this build does.
///
/// # Errors
///
/// Returns [`ProtoError::VersionMismatch`] when the versions differ. Its
/// `Display` output is the reattach message meant for the user.
pub fn check_version(theirs: u16) -> Result<(), ProtoError> {
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtoError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs,
        })
    }
}

/// Encodes a message into a complete length-prefixed frame.
///
/// # Errors
///
/// Returns [`ProtoError::Malformed`] if the value cannot be serialized, or
/// [`ProtoError::FrameTooLarge`] if the payload exceeds [`MAX_FRAME_LEN`].
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtoError> {
    let payload = postcard::to_stdvec(message)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge {
            len: payload.len(),
            max: MAX_FRAME_LEN,
        });
    }

    // The cast is bounded by the MAX_FRAME_LEN check above.
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(LENGTH_PREFIX_LEN + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Splits one complete frame off the front of `buf`, returning its payload and
/// the total number of bytes consumed including the prefix.
///
/// # Errors
///
/// Returns [`ProtoError::Incomplete`] when `buf` does not yet hold a whole
/// frame — the caller should read more bytes and retry with the same buffer —
/// or [`ProtoError::FrameTooLarge`] when the announced length is implausible.
pub fn decode_frame(buf: &[u8]) -> Result<(&[u8], usize), ProtoError> {
    let Some(prefix) = buf.get(..LENGTH_PREFIX_LEN) else {
        return Err(ProtoError::Incomplete);
    };

    let mut raw = [0u8; LENGTH_PREFIX_LEN];
    raw.copy_from_slice(prefix);
    let len = u32::from_be_bytes(raw) as usize;

    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }

    let end = LENGTH_PREFIX_LEN + len;
    match buf.get(LENGTH_PREFIX_LEN..end) {
        Some(payload) => Ok((payload, end)),
        None => Err(ProtoError::Incomplete),
    }
}

/// Decodes one message off the front of `buf`, returning it and the number of
/// bytes consumed.
///
/// The caller drains `consumed` bytes and may call again for the next frame.
///
/// # Errors
///
/// Returns [`ProtoError::Incomplete`] if the buffer holds a partial frame,
/// [`ProtoError::FrameTooLarge`] if the length prefix is implausible, or
/// [`ProtoError::Malformed`] if the payload is not valid postcard for `T`.
pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<(T, usize), ProtoError> {
    let (payload, consumed) = decode_frame(buf)?;
    let message = postcard::from_bytes(payload)?;
    Ok((message, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{PaneId, SessionId, TabId};
    use crate::message::{
        Action, AttentionSource, AttentionState, Cell, CellAttrs, ClientMessage, ClipboardTarget,
        Color, CopyModeState, CopyMotion, CopySelection, CursorShape, GraphicsEffect,
        LayoutSnapshot, MouseButton, MouseEvent, MouseKind, MouseMods, MouseTracking,
        OuterTerminalEffect, PaneAttention, PaneInfo, PaneModes, PaneRect, Point, ProgressState,
        RowUpdate, ScrollPoint, SearchDirection, SearchMatch, ServerMessage, Size, TabSummary,
        TermCaps,
    };

    /// Encodes, decodes, and asserts the value survives unchanged.
    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + core::fmt::Debug,
    {
        let frame = encode(value).expect("value encodes");
        let (decoded, consumed) = decode::<T>(&frame).expect("frame decodes");
        assert_eq!(&decoded, value, "value did not survive the round trip");
        assert_eq!(consumed, frame.len(), "decode did not consume the frame");
    }

    fn sample_row() -> RowUpdate {
        RowUpdate {
            row: 3,
            cells: vec![
                Cell::default(),
                Cell {
                    ch: 'ß',
                    fg: Color::Rgb(1, 2, 3),
                    bg: Color::Indexed(240),
                    attrs: CellAttrs::BOLD.union(CellAttrs::UNDERLINE),
                },
            ],
        }
    }

    fn every_client_message() -> Vec<ClientMessage> {
        vec![
            ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                size: Size::new(120, 40),
                term_caps: TermCaps {
                    truecolor: true,
                    sgr_mouse: true,
                    ..TermCaps::default()
                },
                session: Some(SessionId::new(1)),
            },
            ClientMessage::Attach {
                protocol_version: PROTOCOL_VERSION,
                size: Size::new(80, 24),
                term_caps: TermCaps::default(),
                session: None,
            },
            ClientMessage::Detach,
            ClientMessage::Input(vec![0x1b, b'[', b'A']),
            ClientMessage::Input(Vec::new()),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(10, 5),
                kind: MouseKind::Press(MouseButton::Left),
                mods: MouseMods::NONE,
            }),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(0, 0),
                kind: MouseKind::Motion(Some(MouseButton::Right)),
                mods: MouseMods {
                    shift: true,
                    ..MouseMods::NONE
                },
            }),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(1, 1),
                kind: MouseKind::Motion(None),
                mods: MouseMods {
                    alt: true,
                    ..MouseMods::NONE
                },
            }),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(1, 1),
                kind: MouseKind::Release(MouseButton::Middle),
                mods: MouseMods {
                    ctrl: true,
                    ..MouseMods::NONE
                },
            }),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(1, 1),
                kind: MouseKind::ScrollUp,
                mods: MouseMods {
                    shift: true,
                    alt: true,
                    ctrl: true,
                },
            }),
            ClientMessage::Mouse(MouseEvent {
                pane: PaneId::new(2),
                at: Point::new(1, 1),
                kind: MouseKind::ScrollDown,
                mods: MouseMods::NONE,
            }),
            ClientMessage::Paste(b"hello\r\nworld".to_vec()),
            ClientMessage::Paste(Vec::new()),
            ClientMessage::Focus { focused: true },
            ClientMessage::Focus { focused: false },
            ClientMessage::Resize(Size::new(200, 60)),
            ClientMessage::Command(Action::SplitVertical),
            ClientMessage::Command(Action::SplitHorizontal),
            ClientMessage::Command(Action::ClosePane),
            ClientMessage::Command(Action::FocusLeft),
            ClientMessage::Command(Action::FocusRight),
            ClientMessage::Command(Action::FocusUp),
            ClientMessage::Command(Action::FocusDown),
            ClientMessage::Command(Action::ToggleZoom),
            ClientMessage::Command(Action::NewTab),
            ClientMessage::Command(Action::CloseTab),
            ClientMessage::Command(Action::NextTab),
            ClientMessage::Command(Action::PrevTab),
            ClientMessage::Command(Action::RenameTab("agents".into())),
            ClientMessage::Command(Action::EnterCopyMode),
            ClientMessage::Command(Action::ExitCopyMode),
            ClientMessage::Command(Action::CopyMotion(CopyMotion::WordForward)),
            ClientMessage::Command(Action::CopyMotion(CopyMotion::LastLine)),
            ClientMessage::Command(Action::BeginCopySelection),
            ClientMessage::Command(Action::ClearCopySelection),
            ClientMessage::Command(Action::CopySearch {
                query: "error.*retry".into(),
                direction: SearchDirection::Backward,
            }),
            ClientMessage::Command(Action::NextCopyMatch(SearchDirection::Forward)),
            ClientMessage::Command(Action::CopySelection(ClipboardTarget::Clipboard)),
            ClientMessage::Command(Action::CopySelection(ClipboardTarget::PrimarySelection)),
            ClientMessage::Command(Action::DetachClient),
        ]
    }

    fn every_server_message() -> Vec<ServerMessage> {
        vec![
            ServerMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                session: SessionId::new(9),
                tabs: vec![TabSummary {
                    tab: TabId::new(1),
                    title: "shell".into(),
                    active: true,
                }],
                size: Size::new(80, 24),
            },
            ServerMessage::Refused {
                reason: "protocol version mismatch".into(),
            },
            ServerMessage::Damage {
                pane: PaneId::new(4),
                rows: vec![sample_row()],
            },
            ServerMessage::Damage {
                pane: PaneId::new(4),
                rows: Vec::new(),
            },
            ServerMessage::CursorMoved {
                pane: PaneId::new(4),
                pos: Point::new(7, 2),
                shape: CursorShape::Beam,
                visible: true,
            },
            ServerMessage::CursorMoved {
                pane: PaneId::new(4),
                pos: Point::new(0, 0),
                shape: CursorShape::Underline,
                visible: false,
            },
            ServerMessage::CursorMoved {
                pane: PaneId::new(4),
                pos: Point::new(0, 0),
                shape: CursorShape::Block,
                visible: true,
            },
            ServerMessage::Modes {
                pane: PaneId::new(4),
                modes: PaneModes::default(),
            },
            ServerMessage::Modes {
                pane: PaneId::new(4),
                modes: PaneModes {
                    mouse: MouseTracking::Motion,
                    sgr_mouse: true,
                    bracketed_paste: true,
                    focus_events: true,
                    extended_keys: true,
                },
            },
            ServerMessage::Effect {
                pane: PaneId::new(4),
                effect: OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::Clipboard,
                    text: "copied text".into(),
                },
            },
            ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: vec![PaneRect {
                    pane: PaneId::new(4),
                    x: 0,
                    y: 1,
                    size: Size::new(80, 23),
                }],
                focused: Some(PaneId::new(4)),
                zoomed: None,
            }),
            ServerMessage::Layout(LayoutSnapshot {
                tab: TabId::new(1),
                panes: Vec::new(),
                focused: None,
                zoomed: Some(PaneId::new(4)),
            }),
            ServerMessage::Panes(vec![
                PaneInfo {
                    pane: PaneId::new(4),
                    profile: "claude".into(),
                    name: "api".into(),
                    task: Some("fix the flaky test".into()),
                    cwd: "/home/dev/api".into(),
                },
                PaneInfo {
                    pane: PaneId::new(5),
                    profile: "generic".into(),
                    name: "shell".into(),
                    task: None,
                    cwd: "/home/dev".into(),
                },
            ]),
            ServerMessage::Panes(Vec::new()),
            ServerMessage::Attention(vec![
                PaneAttention {
                    pane: PaneId::new(4),
                    state: AttentionState::NeedsInput,
                    source: AttentionSource::Adapter("claude-adapter".into()),
                    acknowledged: false,
                },
                PaneAttention {
                    pane: PaneId::new(5),
                    state: AttentionState::Unknown,
                    source: AttentionSource::None,
                    acknowledged: false,
                },
            ]),
            ServerMessage::Attention(Vec::new()),
            ServerMessage::CopyMode(Some(CopyModeState {
                pane: PaneId::new(4),
                viewport_top: 8,
                cursor: ScrollPoint::new(12, 3),
                selection: Some(CopySelection {
                    anchor: ScrollPoint::new(10, 1),
                    head: ScrollPoint::new(12, 3),
                }),
                query: Some("error.*retry".into()),
                matches: vec![SearchMatch {
                    start: ScrollPoint::new(12, 0),
                    end: ScrollPoint::new(12, 5),
                }],
            })),
            ServerMessage::CopyMode(None),
            ServerMessage::Bell(PaneId::new(4)),
            ServerMessage::Tabs(Vec::new()),
            ServerMessage::Detached,
            ServerMessage::Exit(0),
            ServerMessage::Exit(-1),
        ]
    }

    #[test]
    fn every_client_message_round_trips() {
        for message in every_client_message() {
            round_trip(&message);
        }
    }

    #[test]
    fn every_server_message_round_trips() {
        for message in every_server_message() {
            round_trip(&message);
        }
    }

    #[test]
    fn value_types_round_trip_on_their_own() {
        round_trip(&Size::new(3, 4));
        round_trip(&Point::new(3, 4));
        round_trip(&TermCaps::default());
        round_trip(&Cell::default());
        round_trip(&CellAttrs::NONE);
        round_trip(&Color::Default);
        round_trip(&sample_row());
        round_trip(&TabSummary {
            tab: TabId::new(0),
            title: String::new(),
            active: false,
        });
        round_trip(&PaneId::new(11));
        round_trip(&TabId::new(11));
        round_trip(&SessionId::new(11));
        round_trip(&AttentionState::Unknown);
        round_trip(&AttentionState::Failed);
        round_trip(&AttentionSource::None);
        round_trip(&AttentionSource::Adapter("my-adapter".into()));
        round_trip(&PaneAttention {
            pane: PaneId::new(11),
            state: AttentionState::Ready,
            source: AttentionSource::Lifecycle,
            acknowledged: true,
        });
        round_trip(&CopyModeState {
            pane: PaneId::new(11),
            viewport_top: 0,
            cursor: ScrollPoint::new(2, 3),
            selection: None,
            query: None,
            matches: Vec::new(),
        });
    }

    #[test]
    fn every_outer_terminal_effect_round_trips_without_raw_passthrough() {
        let effects = [
            OuterTerminalEffect::SetTitle("agent task".into()),
            OuterTerminalEffect::ResetTitle,
            OuterTerminalEffect::ClipboardStore {
                target: ClipboardTarget::PrimarySelection,
                text: "copied text".into(),
            },
            OuterTerminalEffect::Hyperlink {
                uri: "https://example.invalid/task".into(),
            },
            OuterTerminalEffect::Notification {
                title: "needs input".into(),
                body: "review the diff".into(),
            },
            OuterTerminalEffect::Progress(ProgressState::Clear),
            OuterTerminalEffect::Progress(ProgressState::Indeterminate),
            OuterTerminalEffect::Progress(ProgressState::Value(75)),
            OuterTerminalEffect::Progress(ProgressState::Error),
            OuterTerminalEffect::Graphics(GraphicsEffect::Unavailable),
        ];

        for effect in effects {
            round_trip(&effect);
        }
    }

    #[test]
    fn frames_decode_back_to_back_from_one_buffer() {
        let messages = every_client_message();
        let mut buf = Vec::new();
        for message in &messages {
            buf.extend_from_slice(&encode(message).expect("message encodes"));
        }

        let mut rest = buf.as_slice();
        for expected in &messages {
            let (decoded, consumed) =
                decode::<ClientMessage>(rest).expect("each queued frame decodes");
            assert_eq!(&decoded, expected);
            rest = &rest[consumed..];
        }
        assert!(rest.is_empty(), "buffer had trailing bytes");
    }

    #[test]
    fn a_partial_frame_is_incomplete_rather_than_an_error() {
        let frame = encode(&ClientMessage::Resize(Size::new(80, 24))).expect("message encodes");

        for split in 0..frame.len() {
            assert_eq!(
                decode::<ClientMessage>(&frame[..split]),
                Err(ProtoError::Incomplete),
                "a {split}-byte prefix should read as incomplete"
            );
        }
        assert!(decode::<ClientMessage>(&frame).is_ok());
    }

    #[test]
    fn an_implausible_length_prefix_is_rejected_before_allocating() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());

        assert_eq!(
            decode::<ClientMessage>(&frame),
            Err(ProtoError::FrameTooLarge {
                len: MAX_FRAME_LEN + 1,
                max: MAX_FRAME_LEN,
            })
        );
    }

    #[test]
    fn a_corrupt_payload_is_malformed_not_a_panic() {
        let frame = encode(&ClientMessage::Command(Action::NextTab)).expect("message encodes");
        let mut corrupt = frame.clone();
        // 0xff is not a valid discriminant for either message enum.
        let last = corrupt.len() - 1;
        corrupt[LENGTH_PREFIX_LEN] = 0xff;
        corrupt[last] = 0xff;

        let result = decode::<ClientMessage>(&corrupt);
        assert!(
            matches!(
                result,
                Err(ProtoError::Malformed(_) | ProtoError::Incomplete)
            ),
            "expected a clean error, got {result:?}"
        );
    }

    #[test]
    fn a_matching_version_passes_the_handshake() {
        assert_eq!(check_version(PROTOCOL_VERSION), Ok(()));
    }

    #[test]
    fn a_mismatched_version_fails_with_a_reattach_error() {
        let stale = PROTOCOL_VERSION.wrapping_sub(1);
        let err = check_version(stale).expect_err("a stale version must be refused");

        assert_eq!(
            err,
            ProtoError::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: stale,
            }
        );

        let rendered = err.to_string();
        assert!(rendered.contains("version mismatch"), "got: {rendered}");
        assert!(rendered.contains("reattach"), "got: {rendered}");
        assert!(
            rendered.contains(&format!("v{PROTOCOL_VERSION}")),
            "the error must name both versions, got: {rendered}"
        );
    }

    #[test]
    fn an_attach_from_a_future_client_is_refused_end_to_end() {
        // A client built against a newer protocol attaches to this server.
        let future = PROTOCOL_VERSION + 1;
        let attach = ClientMessage::Attach {
            protocol_version: future,
            size: Size::new(80, 24),
            term_caps: TermCaps::default(),
            session: None,
        };
        let wire = encode(&attach).expect("attach encodes");

        let (decoded, _) = decode::<ClientMessage>(&wire).expect("attach decodes");
        let ClientMessage::Attach {
            protocol_version, ..
        } = decoded
        else {
            panic!("expected an Attach");
        };

        let err = check_version(protocol_version).expect_err("a future client must be refused");
        let refusal = ServerMessage::Refused {
            reason: err.to_string(),
        };
        round_trip(&refusal);
    }
}
