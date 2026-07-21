//! The crate-local error types for session model operations.

use core::fmt;

use cloo_proto::{Direction, PaneId, Size};

/// Everything a layout operation can refuse to do.
///
/// Every variant is a *rejection*, not a failure: the layout is always left
/// exactly as it was. Callers can surface these to the user and carry on.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// The named pane is not in this layout.
    UnknownPane(PaneId),
    /// The pane being added is already in this layout. IDs are never reused.
    DuplicatePane(PaneId),
    /// The split would produce a pane below the minimum size. Allowing it would
    /// create a zero- or near-zero-size PTY and a confusing shell.
    TooSmall {
        /// The pane that was going to be split.
        pane: PaneId,
        /// The area it currently occupies.
        available: Size,
        /// The smallest pane cloo will create.
        minimum: Size,
    },
    /// Closing the only pane in a layout. A tab with no panes is closed by the
    /// caller rather than represented as an empty layout.
    LastPane(PaneId),
    /// The pane has no ancestor split along the requested axis, so there is
    /// nothing to resize in that direction.
    NoSplit {
        /// The pane the resize was anchored to.
        pane: PaneId,
        /// The axis that has no split.
        dir: Direction,
    },
    /// A split ratio outside the open interval `(0.0, 1.0)`, or not finite.
    InvalidRatio(f32),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPane(pane) => write!(f, "no such pane in this layout: {pane}"),
            Self::DuplicatePane(pane) => write!(f, "{pane} is already in this layout"),
            Self::TooSmall {
                pane,
                available,
                minimum,
            } => write!(
                f,
                "cannot split {pane}: {}x{} leaves a pane smaller than the {}x{} minimum",
                available.cols, available.rows, minimum.cols, minimum.rows
            ),
            Self::LastPane(pane) => {
                write!(f, "cannot close {pane}: it is the last pane in the tab")
            }
            Self::NoSplit { pane, dir } => {
                let axis = match dir {
                    Direction::Horizontal => "horizontal",
                    Direction::Vertical => "vertical",
                };
                write!(f, "{pane} has no {axis} split to resize")
            }
            Self::InvalidRatio(ratio) => {
                write!(f, "split ratio {ratio} is not inside (0.0, 1.0)")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// Everything pane metadata or a profile definition can be rejected for.
///
/// Like [`LayoutError`], every variant is a *rejection* of a proposed value:
/// nothing is partially applied and the caller keeps whatever it had. Checking
/// is entirely pure — `cloo-core` performs no I/O, so a working directory is
/// validated as a *path*, never by asking the filesystem whether it exists, and
/// a command template is validated as a *shape*, never by looking for the
/// executable on `PATH`. Both of those are the launching server's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// A name, label, or profile ID with nothing in it. An empty pane name would
    /// render as a blank header rather than as an identity.
    Empty(&'static str),
    /// A value longer than the field allows.
    TooLong {
        /// Which field was rejected.
        field: &'static str,
        /// How many characters were supplied.
        len: usize,
        /// The most the field accepts.
        max: usize,
    },
    /// A character that cannot appear in this field. Control characters are
    /// rejected everywhere: they are what would let a pane name repaint the
    /// chrome that draws it.
    BadChar {
        /// Which field was rejected.
        field: &'static str,
        /// The offending character.
        ch: char,
    },
    /// A working directory that is not absolute. A relative path means something
    /// different to the daemon than it did to whoever typed it.
    RelativeCwd(String),
    /// A profile's recommended minimum is below the size cloo will ever create,
    /// so it could never be honored.
    MinSizeTooSmall {
        /// The recommendation that was rejected.
        recommended: Size,
        /// The floor it fell below.
        floor: Size,
    },
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "{field} cannot be empty"),
            Self::TooLong { field, len, max } => {
                write!(f, "{field} is {len} characters; the maximum is {max}")
            }
            Self::BadChar { field, ch } => {
                write!(f, "{field} cannot contain {ch:?}")
            }
            Self::RelativeCwd(path) => {
                write!(f, "working directory {path:?} must be an absolute path")
            }
            Self::MinSizeTooSmall { recommended, floor } => write!(
                f,
                "recommended minimum {}x{} is below cloo's {}x{} floor",
                recommended.cols, recommended.rows, floor.cols, floor.rows
            ),
        }
    }
}

impl std::error::Error for MetadataError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_pane_they_refused() {
        let msg = LayoutError::TooSmall {
            pane: PaneId::new(3),
            available: Size::new(20, 4),
            minimum: Size::new(20, 3),
        }
        .to_string();
        assert!(msg.contains("pane:3"), "{msg}");
        assert!(msg.contains("20x4"), "{msg}");
        assert!(msg.contains("20x3"), "{msg}");
    }

    #[test]
    fn the_last_pane_error_explains_why() {
        let msg = LayoutError::LastPane(PaneId::new(0)).to_string();
        assert!(msg.contains("last pane"), "{msg}");
    }

    #[test]
    fn the_no_split_error_names_the_axis() {
        let msg = LayoutError::NoSplit {
            pane: PaneId::new(1),
            dir: Direction::Vertical,
        }
        .to_string();
        assert!(msg.contains("vertical"), "{msg}");
    }

    #[test]
    fn metadata_errors_name_the_field_they_refused() {
        let msg = MetadataError::TooLong {
            field: "pane name",
            len: 99,
            max: 64,
        }
        .to_string();
        assert!(msg.contains("pane name"), "{msg}");
        assert!(msg.contains("64"), "{msg}");
    }

    #[test]
    fn a_rejected_character_is_shown_escaped() {
        // A bare control character in the message would do to the terminal
        // exactly what rejecting it prevents.
        let msg = MetadataError::BadChar {
            field: "task label",
            ch: '\u{1b}',
        }
        .to_string();
        assert!(!msg.contains('\u{1b}'), "{msg}");
        assert!(msg.contains("task label"), "{msg}");
    }
}
