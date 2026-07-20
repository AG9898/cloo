//! The conversion from emulator cells to wire cells.
//!
//! `cloo-term` and `cloo-proto` deliberately declare their own [`Cell`],
//! [`Color`], and [`CellAttrs`] types: the emulation wrapper has no
//! intra-workspace dependencies, so reusing the wire types would put
//! `cloo-proto` underneath it. `cloo-core` is the one crate that sees both, and
//! this module is the only place the two vocabularies meet.
//!
//! Everything here is a pure function of its input. The attribute bit layouts
//! are identical on both sides, which is what keeps [`wire_cell`] a field copy
//! — change one and the other must change in the same commit.
//!
//! ```
//! use cloo_core::grid::wire_size;
//! use cloo_term::TermSize;
//!
//! let size = TermSize::new(80, 24).expect("80x24 is a valid size");
//! assert_eq!(wire_size(size), cloo_proto::Size::new(80, 24));
//! ```

use cloo_proto::{Cell, CellAttrs, Color, CursorShape, Point, RowUpdate, Size};
use cloo_term::{
    Cell as TermCell, CellAttrs as TermCellAttrs, Color as TermColor,
    CursorShape as TermCursorShape, CursorState, TermSize,
};

/// Converts a grid geometry to its wire form.
#[must_use]
pub fn wire_size(size: TermSize) -> Size {
    Size::new(size.cols(), size.rows())
}

/// Converts one emulator cell to its wire form.
#[must_use]
pub fn wire_cell(cell: TermCell) -> Cell {
    Cell {
        ch: cell.ch,
        fg: wire_color(cell.fg),
        bg: wire_color(cell.bg),
        attrs: wire_attrs(cell.attrs),
    }
}

/// Converts one emulator colour to its wire form.
#[must_use]
pub fn wire_color(color: TermColor) -> Color {
    match color {
        TermColor::Default => Color::Default,
        TermColor::Indexed(index) => Color::Indexed(index),
        TermColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Converts a rendition mask to its wire form.
///
/// The bit positions match on both sides, so this is a copy rather than a
/// remap. It is written out flag by flag anyway: a bare `Self(mask.0)` would
/// keep compiling after somebody renumbered one side.
#[must_use]
pub fn wire_attrs(attrs: TermCellAttrs) -> CellAttrs {
    let mut wire = CellAttrs::NONE;
    for (term_flag, wire_flag) in [
        (TermCellAttrs::BOLD, CellAttrs::BOLD),
        (TermCellAttrs::DIM, CellAttrs::DIM),
        (TermCellAttrs::ITALIC, CellAttrs::ITALIC),
        (TermCellAttrs::UNDERLINE, CellAttrs::UNDERLINE),
        (TermCellAttrs::REVERSE, CellAttrs::REVERSE),
        (TermCellAttrs::HIDDEN, CellAttrs::HIDDEN),
        (TermCellAttrs::STRIKETHROUGH, CellAttrs::STRIKETHROUGH),
    ] {
        if attrs.contains(term_flag) {
            wire = wire.union(wire_flag);
        }
    }
    wire
}

/// Packages one row of emulator cells as a damage message.
#[must_use]
pub fn wire_row(row: u16, cells: &[TermCell]) -> RowUpdate {
    RowUpdate {
        row,
        cells: cells.iter().copied().map(wire_cell).collect(),
    }
}

/// Converts a cursor to its wire form, or `None` if it should not be drawn.
///
/// An invisible cursor becomes `None` rather than a position with a "hidden"
/// shape, because the wire [`CursorShape`] has no hidden variant on purpose:
/// absence is how the client is told not to draw one. `HollowBlock` — the
/// unfocused-pane treatment — degrades to a block here; drawing an unfocused
/// pane differently is the client's decision and lands with chrome.
#[must_use]
pub fn wire_cursor(cursor: CursorState) -> Option<(Point, CursorShape)> {
    if !cursor.visible {
        return None;
    }
    let shape = match cursor.shape {
        TermCursorShape::Block | TermCursorShape::HollowBlock => CursorShape::Block,
        TermCursorShape::Underline => CursorShape::Underline,
        TermCursorShape::Beam => CursorShape::Beam,
        // Unreachable while `visible` is false for a hidden shape, but the
        // emulator owns that invariant, not this function.
        TermCursorShape::Hidden => return None,
    };
    Some((Point::new(cursor.col, cursor.row), shape))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_cell(ch: char) -> TermCell {
        TermCell {
            ch,
            ..TermCell::default()
        }
    }

    #[test]
    fn a_size_keeps_its_columns_and_rows_in_order() {
        let size = TermSize::new(120, 40).expect("120x40 is a valid size");
        assert_eq!(wire_size(size), Size::new(120, 40));
    }

    #[test]
    fn every_colour_form_survives_the_crossing() {
        assert_eq!(wire_color(TermColor::Default), Color::Default);
        assert_eq!(wire_color(TermColor::Indexed(9)), Color::Indexed(9));
        assert_eq!(wire_color(TermColor::Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn every_attribute_maps_to_the_same_bit() {
        for (term_flag, wire_flag) in [
            (TermCellAttrs::BOLD, CellAttrs::BOLD),
            (TermCellAttrs::DIM, CellAttrs::DIM),
            (TermCellAttrs::ITALIC, CellAttrs::ITALIC),
            (TermCellAttrs::UNDERLINE, CellAttrs::UNDERLINE),
            (TermCellAttrs::REVERSE, CellAttrs::REVERSE),
            (TermCellAttrs::HIDDEN, CellAttrs::HIDDEN),
            (TermCellAttrs::STRIKETHROUGH, CellAttrs::STRIKETHROUGH),
        ] {
            assert_eq!(wire_attrs(term_flag), wire_flag);
            assert_eq!(
                term_flag.0, wire_flag.0,
                "the two crates' bit layouts have drifted apart"
            );
        }
    }

    #[test]
    fn a_combined_mask_keeps_every_flag() {
        let attrs = TermCellAttrs::BOLD
            .union(TermCellAttrs::ITALIC)
            .union(TermCellAttrs::STRIKETHROUGH);
        let wire = wire_attrs(attrs);
        assert!(wire.contains(CellAttrs::BOLD));
        assert!(wire.contains(CellAttrs::ITALIC));
        assert!(wire.contains(CellAttrs::STRIKETHROUGH));
        assert!(!wire.contains(CellAttrs::DIM));
    }

    #[test]
    fn a_cell_carries_every_field_across() {
        let cell = TermCell {
            ch: '→',
            fg: TermColor::Indexed(4),
            bg: TermColor::Rgb(7, 8, 9),
            attrs: TermCellAttrs::BOLD,
        };
        assert_eq!(
            wire_cell(cell),
            Cell {
                ch: '→',
                fg: Color::Indexed(4),
                bg: Color::Rgb(7, 8, 9),
                attrs: CellAttrs::BOLD,
            }
        );
    }

    #[test]
    fn a_row_keeps_its_index_and_width() {
        let cells = [term_cell('a'), term_cell('b')];
        let update = wire_row(3, &cells);
        assert_eq!(update.row, 3);
        assert_eq!(update.cells.len(), 2);
        assert_eq!(update.cells[1].ch, 'b');
    }

    #[test]
    fn a_visible_cursor_becomes_a_point_and_a_shape() {
        let cursor = CursorState {
            col: 5,
            row: 2,
            shape: TermCursorShape::Beam,
            visible: true,
        };
        assert_eq!(
            wire_cursor(cursor),
            Some((Point::new(5, 2), CursorShape::Beam))
        );
    }

    #[test]
    fn an_invisible_cursor_becomes_nothing_to_draw() {
        let cursor = CursorState {
            col: 5,
            row: 2,
            shape: TermCursorShape::Block,
            visible: false,
        };
        assert_eq!(wire_cursor(cursor), None);
    }

    #[test]
    fn a_hollow_block_degrades_to_a_block_and_hidden_draws_nothing() {
        let hollow = CursorState {
            col: 0,
            row: 0,
            shape: TermCursorShape::HollowBlock,
            visible: true,
        };
        assert_eq!(
            wire_cursor(hollow),
            Some((Point::new(0, 0), CursorShape::Block))
        );

        let hidden = CursorState {
            shape: TermCursorShape::Hidden,
            ..hollow
        };
        assert_eq!(wire_cursor(hidden), None);
    }
}
