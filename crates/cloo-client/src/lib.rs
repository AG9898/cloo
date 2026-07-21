//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Five modules today:
//!
//! - [`raw_mode`] — entering raw mode and restoring it on every exit path,
//!   including panic and signal.
//! - [`renderer`] — the client-side [`Grid`] cache and the escape sequences
//!   that draw it.
//! - [`outer`] — the outer terminal's geometry and capabilities, which are the
//!   client's to know and never session state.
//! - [`resize`] — `SIGWINCH`, turned into an awaitable report of the outer
//!   terminal's new geometry.
//! - [`attach`] — connecting to a daemon, the versioned handshake, and
//!   detaching without taking the session with it.
//!
//! Rendering is a pure function into a byte buffer rather than a write to a
//! descriptor, which is what makes a fake grid renderable in a unit test with an
//! exact expected string. Input encoding and theming land later in M1.

pub mod attach;
pub mod outer;
pub mod raw_mode;
pub mod renderer;
pub mod resize;

pub use attach::{AttachError, Attached, attach, handshake};
pub use outer::{caps_from_env, current_size, detect_caps, window_size};
pub use raw_mode::{RawMode, RawModeError};
pub use renderer::{Cursor, Grid, RenderError, Renderer};
pub use resize::ResizeWatch;
