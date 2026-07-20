//! Session, tab, and pane model: the layout tree, keymap, profiles, pane
//! metadata, and configuration.
//!
//! `cloo-core` performs **no I/O**. Anything that reads a file or a socket
//! belongs in `cloo-server` or `cloo-client` instead. Everything here is pure
//! and unit-testable without a terminal.
//!
//! Three modules today:
//!
//! - [`layout`] — the ratio-based binary layout tree and its single layout pass.
//! - [`id`] — monotonic allocators for the `cloo-proto` newtype IDs.
//! - [`error`] — the crate-local [`LayoutError`].
//!
//! Layout is always stored as ratios, never as cell counts. Cell counts are
//! derived by [`Layout::resolve`] on every pass, which is what lets a layout
//! survive an outer-terminal resize.

#![forbid(unsafe_code)]

pub mod error;
pub mod id;
pub mod layout;

pub use error::LayoutError;
pub use id::{PaneIdAllocator, SessionIdAllocator, TabIdAllocator};
pub use layout::{Layout, MIN_PANE_SIZE, Node};
