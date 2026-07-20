//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Two modules today:
//!
//! - [`raw_mode`] — entering raw mode and restoring it on every exit path,
//!   including panic and signal.
//! - [`renderer`] — the client-side [`Grid`] cache and the escape sequences
//!   that draw it.
//!
//! Rendering is a pure function into a byte buffer rather than a write to a
//! descriptor, which is what makes a fake grid renderable in a unit test with an
//! exact expected string. Attach, input encoding, and theming land in M1.

pub mod raw_mode;
pub mod renderer;

pub use raw_mode::{RawMode, RawModeError};
pub use renderer::{Cursor, Grid, RenderError, Renderer};
