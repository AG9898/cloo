//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Ten modules today:
//!
//! - [`raw_mode`] — entering raw mode and restoring it on every exit path,
//!   including panic and signal.
//! - [`renderer`] — the client-side [`Grid`] cache and the escape sequences
//!   that draw it, including the positioned [`Span`]s chrome is painted from.
//! - [`chrome`] — pane headers, the focus and attention treatment, the dimming
//!   policy, and the attention queue, summary, and toast deck, as pure functions
//!   into cells.
//! - [`outer`] — the outer terminal's geometry, which is the client's to know
//!   and never session state.
//! - [`capabilities`] — what that terminal can do, the documented fallback for
//!   everything it cannot, and the refusal for a `TERM` that resolves to
//!   nothing at all.
//! - [`resize`] — `SIGWINCH`, turned into an awaitable report of the outer
//!   terminal's new geometry.
//! - [`input`] — the reporting modes cloo asks the outer terminal for, the
//!   decoder that splits its byte stream back into typed events, the rule that
//!   decides whether a mouse event is chrome's or the application's, and the
//!   keyboard actions that drive the attention queue overlay.
//! - [`copy_mode`] — highlights and the status row for server-owned copy mode,
//!   plus the explicit, policy-gated OSC 52 copy.
//! - [`effects`] — client-local policy and safe rendering for allowlisted
//!   outer-terminal effects.
//! - [`attach`] — connecting to a daemon, the versioned handshake, and
//!   detaching without taking the session with it.
//!
//! Rendering is a pure function into a byte buffer rather than a write to a
//! descriptor, which is what makes a fake grid renderable in a unit test with an
//! exact expected string. Named themes resolve into client-local tokens, so an
//! attached terminal can inherit its palette or use a deliberate ANSI fallback.

pub mod attach;
pub mod capabilities;
pub mod chrome;
pub mod copy_mode;
pub mod effects;
pub mod input;
pub mod outer;
pub mod raw_mode;
pub mod renderer;
pub mod resize;
pub mod theme;

pub use attach::{AttachError, Attached, attach, handshake};
pub use capabilities::{
    Capability, CapsError, Degradation, Fallback, attach_caps, caps_from_env, degradations,
    detect_attach_caps, detect_caps,
};
pub use chrome::{
    Attention, AttentionQueue, ChromeOptions, DEFAULT_PREFIX_HINT, PaneChrome, QueueEntry, Toast,
    ToastDeck, dim_cell, dim_cell_with_theme, dim_cells, header_cells, header_span,
    queue_row_cells, queue_row_span, status_bar_cells, status_bar_span, summary_cells,
    summary_span, tab_row_cells, tab_row_span, toast_cells, toast_span,
};
pub use copy_mode::{
    Highlight, apply_copy, copy_request, highlight_spans, status_cells as copy_status_cells,
    status_span as copy_status_span, viewport_row,
};
pub use effects::{EffectPolicy, apply_effect, effect_bytes};
pub use input::{
    InputDecoder, InputEvent, MouseOwner, MouseReport, OuterModes, QueueAction, mouse_owner,
    queue_action,
};
pub use outer::{current_size, window_size};
pub use raw_mode::{RawMode, RawModeError};
pub use renderer::{Cursor, Grid, RenderError, Renderer, Span};
pub use resize::ResizeWatch;
pub use theme::{Theme, ThemeToken};
