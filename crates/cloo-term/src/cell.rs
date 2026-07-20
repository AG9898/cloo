//! cloo-owned grid value types.
//!
//! These deliberately duplicate the shape of the equivalent `cloo-proto` types
//! rather than reusing them. `cloo-term` sits at the bottom of the dependency
//! graph alongside `cloo-proto` and has no intra-workspace dependencies, which
//! is what keeps the emulation backend swappable without touching the wire.
//! `cloo-core` owns the conversion between the two.
//!
//! The [`CellAttrs`] bit positions match `cloo_proto::CellAttrs` exactly, so
//! that conversion stays a field copy rather than a re-encode. Changing a bit
//! here requires changing it there in the same commit.

use crate::error::TermError;

/// A grid size in cells.
///
/// Both dimensions must be non-zero; an emulator cannot exist with a zero-area
/// grid, so [`TermSize::new`] is the only constructor and it validates. Every
/// size that reaches the backend has already been through here, which is why
/// neither [`Emulator::new`] nor [`Emulator::resize`] can fail on geometry.
///
/// [`Emulator::new`]: crate::Emulator::new
/// [`Emulator::resize`]: crate::Emulator::resize
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TermSize {
    cols: u16,
    rows: u16,
}

impl TermSize {
    /// Builds a size, rejecting a zero dimension.
    ///
    /// # Errors
    ///
    /// Returns [`TermError::ZeroSize`] if either dimension is zero.
    pub const fn new(cols: u16, rows: u16) -> Result<Self, TermError> {
        if cols == 0 || rows == 0 {
            Err(TermError::ZeroSize { cols, rows })
        } else {
            Ok(Self { cols, rows })
        }
    }

    /// Width in columns. Always non-zero.
    #[must_use]
    pub const fn cols(self) -> u16 {
        self.cols
    }

    /// Height in rows. Always non-zero.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }
}

/// A cell colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's own default foreground or background.
    #[default]
    Default,
    /// An index into the 256-colour palette.
    Indexed(u8),
    /// A 24-bit colour.
    Rgb(u8, u8, u8),
}

/// Rendition flags for a cell, packed into a bitfield.
///
/// Bit positions mirror `cloo_proto::CellAttrs`. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    /// No rendition applied.
    pub const NONE: Self = Self(0);
    /// Bold.
    pub const BOLD: Self = Self(1 << 0);
    /// Dim / faint.
    pub const DIM: Self = Self(1 << 1);
    /// Italic.
    pub const ITALIC: Self = Self(1 << 2);
    /// Underline.
    pub const UNDERLINE: Self = Self(1 << 3);
    /// Reverse video.
    pub const REVERSE: Self = Self(1 << 4);
    /// Hidden / concealed.
    pub const HIDDEN: Self = Self(1 << 5);
    /// Strikethrough.
    pub const STRIKETHROUGH: Self = Self(1 << 6);

    /// Combines two sets of flags.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// True when every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One rendered character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character occupying the cell.
    pub ch: char,
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Rendition flags.
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        }
    }
}

/// How the cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// A filled block.
    #[default]
    Block,
    /// An underscore.
    Underline,
    /// A vertical bar.
    Beam,
    /// An unfilled block, conventionally used for an unfocused pane.
    HollowBlock,
    /// Not drawn at all.
    Hidden,
}

/// Where the cursor is and how it should be drawn.
///
/// `col` and `row` are viewport coordinates: `row` is measured from the top of
/// the *visible* grid, so scrolling back moves the cursor down and eventually
/// out of view, at which point `visible` is false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    /// Column, from the left edge of the grid.
    pub col: u16,
    /// Row, from the top of the visible grid.
    pub row: u16,
    /// How to draw it.
    pub shape: CursorShape,
    /// Whether it should be drawn at all. False when the child hid it with
    /// DECTCEM, or when scrollback has pushed it out of the viewport.
    pub visible: bool,
}
