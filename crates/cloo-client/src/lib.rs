//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Six modules today:
//!
//! - [`raw_mode`] — entering raw mode and restoring it on every exit path,
//!   including panic and signal.
//! - [`renderer`] — the client-side [`Grid`] cache and the escape sequences
//!   that draw it.
//! - [`outer`] — the outer terminal's geometry, which is the client's to know
//!   and never session state.
//! - [`capabilities`] — what that terminal can do, the documented fallback for
//!   everything it cannot, and the refusal for a `TERM` that resolves to
//!   nothing at all.
//! - [`resize`] — `SIGWINCH`, turned into an awaitable report of the outer
//!   terminal's new geometry.
//! - [`attach`] — connecting to a daemon, the versioned handshake, and
//!   detaching without taking the session with it.
//!
//! Rendering is a pure function into a byte buffer rather than a write to a
//! descriptor, which is what makes a fake grid renderable in a unit test with an
//! exact expected string. Input encoding and theming land later in M1.

pub mod attach;
pub mod capabilities;
pub mod outer;
pub mod raw_mode;
pub mod renderer;
pub mod resize;

pub use attach::{AttachError, Attached, attach, handshake};
pub use capabilities::{
    Capability, CapsError, Degradation, Fallback, attach_caps, caps_from_env, degradations,
    detect_attach_caps, detect_caps,
};
pub use outer::{current_size, window_size};
pub use raw_mode::{RawMode, RawModeError};
pub use renderer::{Cursor, Grid, RenderError, Renderer};
pub use resize::ResizeWatch;
