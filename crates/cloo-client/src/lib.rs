//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Seven modules today:
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
//! - [`input`] — the reporting modes cloo asks the outer terminal for, the
//!   decoder that splits its byte stream back into typed events, and the rule
//!   that decides whether a mouse event is chrome's or the application's.
//! - [`effects`] — client-local policy and safe rendering for allowlisted
//!   outer-terminal effects.
//! - [`attach`] — connecting to a daemon, the versioned handshake, and
//!   detaching without taking the session with it.
//!
//! Rendering is a pure function into a byte buffer rather than a write to a
//! descriptor, which is what makes a fake grid renderable in a unit test with an
//! exact expected string. Theming lands later in M1.

pub mod attach;
pub mod capabilities;
pub mod effects;
pub mod input;
pub mod outer;
pub mod raw_mode;
pub mod renderer;
pub mod resize;

pub use attach::{AttachError, Attached, attach, handshake};
pub use capabilities::{
    Capability, CapsError, Degradation, Fallback, attach_caps, caps_from_env, degradations,
    detect_attach_caps, detect_caps,
};
pub use effects::{EffectPolicy, apply_effect, effect_bytes};
pub use input::{InputDecoder, InputEvent, MouseOwner, MouseReport, OuterModes, mouse_owner};
pub use outer::{current_size, window_size};
pub use raw_mode::{RawMode, RawModeError};
pub use renderer::{Cursor, Grid, RenderError, Renderer};
pub use resize::ResizeWatch;
