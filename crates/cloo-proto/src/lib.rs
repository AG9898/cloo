//! Wire types, length framing, and the versioned handshake for cloo.
//!
//! This crate is the bottom of the dependency graph: it knows nothing about
//! PTYs, terminal emulation, or rendering. Every type here crosses the Unix
//! socket between `cloo-server` and `cloo-client`.
//!
//! Five modules:
//!
//! - [`ids`] — newtype identifiers that cross the wire.
//! - [`message`] — the [`ClientMessage`] and [`ServerMessage`] enums.
//! - [`frame`] — length-prefixed postcard framing and the
//!   [`PROTOCOL_VERSION`] handshake check.
//! - [`stream`] — that framing paired with an async transport, so the
//!   drain-and-retry loop exists once instead of once per side.
//! - [`error`] — the crate-local [`ProtoError`].
//!
//! Bump [`PROTOCOL_VERSION`] on **every** change to a wire type. A stale client
//! attached to a rebuilt server must fail with a clear reattach error rather
//! than desync and present as a rendering bug.
//!
//! See `docs/ARCHITECTURE.md` for the protocol shape.

#![forbid(unsafe_code)]

pub mod error;
pub mod frame;
pub mod ids;
pub mod message;
pub mod stream;

pub use error::ProtoError;
pub use frame::{
    LENGTH_PREFIX_LEN, MAX_FRAME_LEN, PROTOCOL_VERSION, check_version, decode, decode_frame, encode,
};
pub use ids::{ClientId, PaneId, SessionId, TabId};
pub use message::{
    Action, Cell, CellAttrs, ClientMessage, Color, CursorShape, Direction, LayoutSnapshot,
    MouseButton, MouseEvent, MouseKind, MouseMods, MouseTracking, PaneModes, PaneRect, Point,
    RowUpdate, ServerMessage, Size, TabSummary, TermCaps,
};
pub use stream::{FrameStream, StreamError};
