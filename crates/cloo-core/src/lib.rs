//! Session, tab, and pane model: the layout tree, keymap, profiles, pane
//! metadata, and configuration.
//!
//! `cloo-core` performs **no I/O**. Anything that reads a file or a socket
//! belongs in `cloo-server` or `cloo-client` instead. Everything here is pure
//! and unit-testable without a terminal.
//!
//! Seven modules today:
//!
//! - [`layout`] — the ratio-based binary layout tree, its single layout pass,
//!   geometric directional focus, and zoom as a view flag over an untouched tree.
//! - [`profile`] — launch profiles: the built-in `generic`, `codex`, and
//!   `claude` are three values of one struct, not three code paths.
//! - [`config`] — parsing `config.toml` *text* into a validated [`Config`],
//!   merging local profiles over the built-ins. Reading the file is the
//!   server's; a document error falls back to defaults and a single bad profile
//!   is dropped with a warning rather than costing the rest.
//! - [`pane`] — pane identity and the provenance-aware attention state.
//! - [`grid`] — the emulator-cell to wire-cell conversion, the only place the
//!   `cloo-term` and `cloo-proto` vocabularies meet.
//! - [`id`] — monotonic allocators for the `cloo-proto` newtype IDs.
//! - [`error`] — the crate-local [`LayoutError`] and [`MetadataError`].
//!
//! Layout is always stored as ratios, never as cell counts. Cell counts are
//! derived by [`Layout::resolve`] on every pass, which is what lets a layout
//! survive an outer-terminal resize.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod grid;
pub mod id;
pub mod layout;
pub mod pane;
pub mod profile;

pub use config::{Config, ConfigError, ConfigWarning};
pub use error::{LayoutError, MetadataError};
pub use grid::{
    wire_attrs, wire_cell, wire_color, wire_cursor, wire_modes, wire_mouse_tracking, wire_row,
    wire_size,
};
pub use id::{PaneIdAllocator, SessionIdAllocator, TabIdAllocator};
pub use layout::{Layout, MIN_PANE_SIZE, Node, Side};
pub use pane::{
    Attention, AttentionSource, AttentionState, PaneMeta, PaneName, TaskLabel, WorkingDir,
};
pub use profile::{AdapterId, Profile, ProfileCommand, ProfileId};
