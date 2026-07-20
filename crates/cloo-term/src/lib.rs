//! Thin wrapper over the terminal emulation backend.
//!
//! This is the **only** crate in the workspace permitted to import
//! `alacritty_terminal`, and it must never leak that crate's types across its
//! public API. The pin plus this boundary is the entire mitigation for upstream
//! API churn — see `docs/DECISIONS.md` RESOLVED-02.
//!
//! The public surface is deliberately narrow: feed bytes, read cells, resize,
//! access scrollback.
//!
//! ```
//! use cloo_term::{Emulator, TermSize};
//!
//! let size = TermSize::new(80, 24)?;
//! let mut term = Emulator::with_default_scrollback(size);
//! term.feed(b"\x1b[1mhello\x1b[0m");
//! assert_eq!(term.row_text(0).as_deref(), Some("hello"));
//! # Ok::<(), cloo_term::TermError>(())
//! ```
//!
//! Three modules:
//!
//! - [`emulator`] — the [`Emulator`] wrapper, and the only place the backend is
//!   named.
//! - [`cell`] — cloo-owned grid value types, mirroring the `cloo-proto` shapes
//!   without depending on them.
//! - [`error`] — the crate-local [`TermError`].
//!
//! `cloo-term` has no intra-workspace dependencies, by design. It sits at the
//! bottom of the graph next to `cloo-proto`, and `cloo-core` owns the
//! conversion between the two crates' cell types.

#![forbid(unsafe_code)]

pub mod cell;
pub mod emulator;
pub mod error;

pub use cell::{Cell, CellAttrs, Color, CursorShape, CursorState, TermSize};
pub use emulator::{DEFAULT_SCROLLBACK_LINES, Emulator};
pub use error::TermError;
