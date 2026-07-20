//! The crate-local error type.

use std::fmt;

/// Everything `cloo-term` can refuse to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermError {
    /// A grid dimension was zero. The emulation backend has no meaningful
    /// behaviour for a zero-area grid, so it is rejected at the boundary rather
    /// than passed through.
    ZeroSize {
        /// Requested columns.
        cols: u16,
        /// Requested rows.
        rows: u16,
    },
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSize { cols, rows } => write!(
                f,
                "grid size {cols}x{rows} is invalid: both dimensions must be non-zero"
            ),
        }
    }
}

impl std::error::Error for TermError {}
