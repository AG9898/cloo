//! The client-side grid cache and the escape sequences that draw it.
//!
//! Two types live here. [`Grid`] is the client's cache of one pane's visible
//! cells — the *only* state a client holds, and never authoritative.
//! [`Renderer`] turns a grid into bytes for the outer terminal.
//!
//! Rendering is deliberately a pure function of (grid, cursor, capabilities)
//! into a byte buffer. Nothing here writes to a descriptor, which is what makes
//! a fake grid renderable in a unit test with an exact expected string. The
//! caller writes [`Renderer::output`] wherever it likes.
//!
//! Escape sequences are only ever emitted from this module — never printed ad
//! hoc from elsewhere in the client — and a pane's own bytes are re-rendered
//! from parsed cells rather than forwarded, so no pane can drive the user's
//! terminal through the renderer.
//!
//! ```
//! use cloo_client::renderer::{Grid, Renderer};
//! use cloo_proto::{Size, TermCaps};
//!
//! let grid = Grid::new(Size::new(2, 1));
//! let mut renderer = Renderer::new(TermCaps::default());
//! assert!(renderer.render_full(&grid, None).starts_with(b"\x1b[?25l"));
//! ```

use std::fmt;

use cloo_proto::{Cell, CellAttrs, Color, CursorShape, Point, RowUpdate, Size, TermCaps};

/// Everything the renderer can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// A [`RowUpdate`] named a row outside the grid. The server and the client
    /// disagree about geometry, which means a resize crossed a damage message
    /// in flight; the client should resync rather than draw a guess.
    RowOutOfRange {
        /// The row the update named.
        row: u16,
        /// How many rows the grid actually has.
        rows: u16,
    },
    /// A [`RowUpdate`] carried the wrong number of cells. A row is replaced
    /// wholesale, so a short row would silently leave stale cells behind.
    RowWidthMismatch {
        /// How many cells arrived.
        got: usize,
        /// How many the grid expects.
        expected: usize,
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowOutOfRange { row, rows } => {
                write!(f, "row {row} is outside a grid of {rows} rows")
            }
            Self::RowWidthMismatch { got, expected } => {
                write!(f, "row update carried {got} cells, expected {expected}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Where the cursor sits and how it should be drawn.
///
/// Separate from [`Grid`] because the cursor arrives on its own message and
/// moves far more often than cell contents do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Position within the grid.
    pub pos: Point,
    /// How to draw it.
    pub shape: CursorShape,
}

impl Cursor {
    /// Builds a cursor.
    #[must_use]
    pub const fn new(pos: Point, shape: CursorShape) -> Self {
        Self { pos, shape }
    }
}

/// The client's cache of one pane's visible cells.
///
/// Rows are replaced wholesale, matching the damage unit on the wire. Cells are
/// stored row-major and the grid is always exactly `size.rows * size.cols`
/// cells, so a render never has to reason about ragged rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    size: Size,
    cells: Vec<Cell>,
}

impl Grid {
    /// Builds a grid of blank cells.
    ///
    /// A zero dimension is representable here on purpose: the layout pass can
    /// hand a client a pane of zero width during a violent resize, and a
    /// renderer that panicked on one would be a worse failure than drawing
    /// nothing.
    #[must_use]
    pub fn new(size: Size) -> Self {
        Self {
            cells: vec![Cell::default(); cell_count(size)],
            size,
        }
    }

    /// The grid's geometry.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// One row of cells, or `None` if `row` is outside the grid.
    #[must_use]
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.size.rows || self.size.cols == 0 {
            return None;
        }
        let start = usize::from(row) * usize::from(self.size.cols);
        self.cells.get(start..start + usize::from(self.size.cols))
    }

    /// Replaces one row.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::RowOutOfRange`] if the row does not exist and
    /// [`RenderError::RowWidthMismatch`] if the update is not exactly one row
    /// wide. The grid is left unchanged in both cases.
    pub fn apply(&mut self, update: &RowUpdate) -> Result<(), RenderError> {
        if update.row >= self.size.rows {
            return Err(RenderError::RowOutOfRange {
                row: update.row,
                rows: self.size.rows,
            });
        }
        let width = usize::from(self.size.cols);
        if update.cells.len() != width {
            return Err(RenderError::RowWidthMismatch {
                got: update.cells.len(),
                expected: width,
            });
        }
        let start = usize::from(update.row) * width;
        self.cells[start..start + width].copy_from_slice(&update.cells);
        Ok(())
    }

    /// Resizes the cache, keeping the cells that still fit.
    ///
    /// The server is authoritative and will send damage for everything that
    /// actually changed; keeping the overlap only avoids a full-screen flash
    /// between the resize and the first damage message.
    pub fn resize(&mut self, size: Size) {
        let mut cells = vec![Cell::default(); cell_count(size)];
        let rows = size.rows.min(self.size.rows);
        let cols = usize::from(size.cols.min(self.size.cols));
        for row in 0..rows {
            let src = usize::from(row) * usize::from(self.size.cols);
            let dst = usize::from(row) * usize::from(size.cols);
            cells[dst..dst + cols].copy_from_slice(&self.cells[src..src + cols]);
        }
        self.cells = cells;
        self.size = size;
    }
}

/// How many cells a grid of `size` holds.
fn cell_count(size: Size) -> usize {
    usize::from(size.rows) * usize::from(size.cols)
}

/// A run of cells to paint at one place on the outer terminal.
///
/// The unit chrome is drawn in. A pane's *contents* arrive as whole rows of a
/// [`Grid`] and are painted from column zero; a header, a border, or a status
/// row belongs to the client alone and can sit anywhere, so it carries its own
/// origin. Building chrome as spans keeps [`crate::chrome`] a pure function
/// into cells and leaves this module the only place bytes are produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Where the run starts, in outer-terminal cells.
    pub at: Point,
    /// The cells to paint, left to right.
    pub cells: Vec<Cell>,
}

impl Span {
    /// Builds a span.
    #[must_use]
    pub const fn new(at: Point, cells: Vec<Cell>) -> Self {
        Self { at, cells }
    }
}

/// The rendition currently active on the outer terminal.
///
/// Tracked so a run of identically styled cells costs one SGR sequence rather
/// than one per cell. `None` means "unknown" — after a full clear, or before
/// anything has been drawn — which forces the first cell to emit its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
}

impl Style {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
        }
    }
}

/// Turns a [`Grid`] into escape sequences for the outer terminal.
///
/// The renderer owns its output buffer and reuses it across frames, so a steady
/// render loop does not allocate.
#[derive(Debug, Clone)]
pub struct Renderer {
    caps: TermCaps,
    out: String,
}

impl Renderer {
    /// Builds a renderer for a terminal with the given capabilities.
    #[must_use]
    pub fn new(caps: TermCaps) -> Self {
        Self {
            caps,
            out: String::new(),
        }
    }

    /// The capabilities this renderer targets.
    #[must_use]
    pub fn caps(&self) -> TermCaps {
        self.caps
    }

    /// The bytes produced by the most recent render.
    #[must_use]
    pub fn output(&self) -> &[u8] {
        self.out.as_bytes()
    }

    /// Draws every cell of `grid` and returns the bytes to write.
    ///
    /// This is the unconditional redraw: used on attach, after a resize, and
    /// whenever a client resyncs. Ordinary damage frames use
    /// [`render_rows`](Self::render_rows) instead.
    ///
    /// The frame is deliberately ordered so nothing is ever seen half-drawn:
    /// hide the cursor, clear, paint, reset the rendition, then place and show
    /// the cursor.
    pub fn render_full(&mut self, grid: &Grid, cursor: Option<Cursor>) -> &[u8] {
        self.out.clear();
        self.out.push_str("\x1b[?25l");
        self.out.push_str("\x1b[H\x1b[2J");

        let mut style = None;
        for row in 0..grid.size().rows {
            self.paint_row_with_style(grid, row, &mut style);
        }

        self.finish(cursor);

        self.output()
    }

    /// Draws only the rows named by coalesced server damage.
    ///
    /// Callers first apply and validate every [`RowUpdate`] in their [`Grid`],
    /// then pass its row indices here. A bad update is therefore refused before
    /// this method can draw a partial guess. Each invocation starts its own
    /// rendition from an absolute reset, so a dropped frame cannot make a row
    /// inherit an outer-terminal style from an earlier one.
    ///
    /// This never clears the screen. Resync and geometry changes remain the
    /// explicit full-redraw path above; ordinary output repaints only the rows
    /// the server found different at its frame boundary.
    pub fn render_rows(&mut self, grid: &Grid, rows: &[u16], cursor: Option<Cursor>) -> &[u8] {
        self.out.clear();
        self.out.push_str("\x1b[?25l");
        for &row in rows {
            self.paint_row(grid, row);
        }
        self.finish(cursor);

        self.output()
    }

    /// Draws chrome: positioned runs of cells the client composed itself.
    ///
    /// Used for pane headers, borders, and any other chrome — never for pane
    /// contents, which come from a [`Grid`] so a resize can be validated
    /// against the server's geometry. Each span starts its own rendition from
    /// an absolute reset for the same reason a damage row does: a dropped frame
    /// must not leave a header wearing a neighbour's style.
    pub fn render_spans(&mut self, spans: &[Span], cursor: Option<Cursor>) -> &[u8] {
        self.out.clear();
        self.out.push_str("\x1b[?25l");
        for span in spans {
            self.paint_span(span);
        }
        self.finish(cursor);

        self.output()
    }

    /// Draws one positioned run of chrome cells.
    fn paint_span(&mut self, span: &Span) {
        if span.cells.is_empty() {
            return;
        }
        let mut style = None;
        move_to(&mut self.out, span.at.row, span.at.col);
        for cell in &span.cells {
            let wanted = Style::of(cell);
            if style != Some(wanted) {
                push_sgr(&mut self.out, wanted, self.caps);
                style = Some(wanted);
            }
            self.out.push(cell.ch);
        }
    }

    /// Draws one complete damaged row from the cache.
    fn paint_row(&mut self, grid: &Grid, row: u16) {
        let mut style = None;
        self.paint_row_with_style(grid, row, &mut style);
    }

    /// Draws one row while carrying rendition state across a paint operation.
    fn paint_row_with_style(&mut self, grid: &Grid, row: u16, style: &mut Option<Style>) {
        let Some(cells) = grid.row(row) else {
            return;
        };
        move_to(&mut self.out, row, 0);
        for cell in cells {
            let wanted = Style::of(cell);
            if *style != Some(wanted) {
                push_sgr(&mut self.out, wanted, self.caps);
                *style = Some(wanted);
            }
            self.out.push(cell.ch);
        }
    }

    /// Resets rendition and restores the cursor after any paint operation.
    fn finish(&mut self, cursor: Option<Cursor>) {
        self.out.push_str("\x1b[0m");
        if let Some(cursor) = cursor {
            move_to(&mut self.out, cursor.pos.row, cursor.pos.col);
            self.out.push_str(shape_sequence(cursor.shape));
            self.out.push_str("\x1b[?25h");
        }
    }
}

/// Emits a CUP sequence. Escape coordinates are one-based; grid ones are not.
fn move_to(out: &mut String, row: u16, col: u16) {
    out.push_str("\x1b[");
    push_num(out, u32::from(row) + 1);
    out.push(';');
    push_num(out, u32::from(col) + 1);
    out.push('H');
}

/// Emits a full rendition, always leading with a reset.
///
/// Resetting first means the sequence describes the target style absolutely
/// rather than as a delta from whatever came before, so a dropped or reordered
/// frame cannot leave a cell wearing a stale attribute.
fn push_sgr(out: &mut String, style: Style, caps: TermCaps) {
    out.push_str("\x1b[0");
    for (flag, code) in [
        (CellAttrs::BOLD, 1),
        (CellAttrs::DIM, 2),
        (CellAttrs::ITALIC, 3),
        (CellAttrs::UNDERLINE, 4),
        (CellAttrs::REVERSE, 7),
        (CellAttrs::HIDDEN, 8),
        (CellAttrs::STRIKETHROUGH, 9),
    ] {
        if style.attrs.contains(flag) {
            out.push(';');
            push_num(out, code);
        }
    }
    push_color(out, style.fg, 38, caps);
    push_color(out, style.bg, 48, caps);
    out.push('m');
}

/// Emits one colour as SGR parameters, or nothing for the terminal default —
/// the leading reset has already restored it.
///
/// `selector` is 38 for foreground and 48 for background.
fn push_color(out: &mut String, color: Color, selector: u32, caps: TermCaps) {
    match color {
        Color::Default => {}
        Color::Indexed(index) => {
            out.push(';');
            push_num(out, selector);
            out.push_str(";5;");
            push_num(out, u32::from(index));
        }
        Color::Rgb(r, g, b) if caps.truecolor => {
            out.push(';');
            push_num(out, selector);
            out.push_str(";2;");
            push_num(out, u32::from(r));
            out.push(';');
            push_num(out, u32::from(g));
            out.push(';');
            push_num(out, u32::from(b));
        }
        // The documented fallback for a terminal without 24-bit colour: the
        // nearest 256-palette entry. Never emit a sequence the client said it
        // could not display and hope for the best.
        Color::Rgb(r, g, b) => {
            out.push(';');
            push_num(out, selector);
            out.push_str(";5;");
            push_num(out, u32::from(downsample_rgb(r, g, b)));
        }
    }
}

/// Maps a 24-bit colour onto the xterm 256-colour palette.
///
/// Near-grey values take the 24-step greyscale ramp (232..=255), which is much
/// finer than the colour cube's four grey steps; everything else quantizes into
/// the 6x6x6 cube at 16. The ramp only spans 8..=238, so true black and true
/// white still go to the cube, where they are exact.
fn downsample_rgb(r: u8, g: u8, b: u8) -> u8 {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max - min < 8 && (8..=238).contains(&max) {
        // The ramp runs from 8 to 238 in steps of 10.
        let level = u16::from(max).saturating_sub(3) / 10;
        return 232 + u8::try_from(level.min(23)).unwrap_or(23);
    }
    let axis = |v: u8| -> u16 {
        // Cube stops sit at 0, 95, 135, 175, 215, 255; the midpoints below
        // pick the nearest one.
        match v {
            0..=47 => 0,
            48..=114 => 1,
            115..=154 => 2,
            155..=194 => 3,
            195..=234 => 4,
            _ => 5,
        }
    };
    let index = 16 + 36 * axis(r) + 6 * axis(g) + axis(b);
    u8::try_from(index).unwrap_or(u8::MAX)
}

/// The DECSCUSR sequence for a cursor shape.
///
/// Steady rather than blinking: cloo draws its own attention treatment, and a
/// blinking cursor in every pane is noise.
fn shape_sequence(shape: CursorShape) -> &'static str {
    match shape {
        CursorShape::Block => "\x1b[2 q",
        CursorShape::Underline => "\x1b[4 q",
        CursorShape::Beam => "\x1b[6 q",
    }
}

/// Appends `n` in decimal without allocating.
///
/// `to_string` in the render path would allocate once per escape parameter,
/// and there are several per styled cell run.
fn push_num(out: &mut String, n: u32) {
    let mut digits = [0_u8; 10];
    let mut len = 0;
    let mut value = n;
    loop {
        digits[len] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for i in (0..len).rev() {
        out.push(char::from(digits[i]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a row of styled cells from a string.
    fn row_of(text: &str, fg: Color, attrs: CellAttrs) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell {
                ch,
                fg,
                bg: Color::Default,
                attrs,
            })
            .collect()
    }

    fn truecolor() -> TermCaps {
        TermCaps {
            truecolor: true,
            ..TermCaps::default()
        }
    }

    // -- push_num ---------------------------------------------------------

    #[test]
    fn numbers_render_without_allocating_wrong_digits() {
        for n in [0_u32, 1, 9, 10, 99, 100, 4294967295] {
            let mut out = String::new();
            push_num(&mut out, n);
            assert_eq!(out, n.to_string());
        }
    }

    // -- Grid -------------------------------------------------------------

    #[test]
    fn a_new_grid_is_blank_and_the_right_shape() {
        let grid = Grid::new(Size::new(3, 2));
        assert_eq!(grid.size(), Size::new(3, 2));
        assert_eq!(grid.row(0).map(<[Cell]>::len), Some(3));
        assert_eq!(grid.row(1).map(<[Cell]>::len), Some(3));
        assert_eq!(grid.row(2), None);
        assert!(
            grid.row(0)
                .is_some_and(|row| row.iter().all(|c| *c == Cell::default()))
        );
    }

    #[test]
    fn a_zero_sized_grid_is_representable_and_empty() {
        let grid = Grid::new(Size::new(0, 0));
        assert_eq!(grid.row(0), None);
        let mut renderer = Renderer::new(TermCaps::default());
        // The layout pass can produce this during a resize; it must not panic.
        assert_eq!(
            renderer.render_full(&grid, None),
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[0m"
        );
    }

    #[test]
    fn applying_a_row_replaces_it_wholesale() {
        let mut grid = Grid::new(Size::new(2, 2));
        grid.apply(&RowUpdate {
            row: 1,
            cells: row_of("hi", Color::Default, CellAttrs::NONE),
        })
        .expect("a 2-cell update fits a 2-column grid");
        assert_eq!(grid.row(1).map(|r| r[0].ch), Some('h'));
        assert_eq!(
            grid.row(0).map(|r| r[0].ch),
            Some(' '),
            "row 0 is untouched"
        );
    }

    #[test]
    fn an_out_of_range_row_is_rejected_and_changes_nothing() {
        let mut grid = Grid::new(Size::new(2, 2));
        let before = grid.clone();
        let err = grid
            .apply(&RowUpdate {
                row: 9,
                cells: row_of("hi", Color::Default, CellAttrs::NONE),
            })
            .expect_err("row 9 is outside a 2-row grid");
        assert_eq!(err, RenderError::RowOutOfRange { row: 9, rows: 2 });
        assert_eq!(grid, before);
    }

    #[test]
    fn a_short_row_is_rejected_rather_than_leaving_stale_cells() {
        let mut grid = Grid::new(Size::new(4, 1));
        let before = grid.clone();
        let err = grid
            .apply(&RowUpdate {
                row: 0,
                cells: row_of("hi", Color::Default, CellAttrs::NONE),
            })
            .expect_err("a 2-cell update cannot fill a 4-column grid");
        assert_eq!(
            err,
            RenderError::RowWidthMismatch {
                got: 2,
                expected: 4
            }
        );
        assert_eq!(grid, before);
    }

    #[test]
    fn resize_keeps_the_overlapping_cells() {
        let mut grid = Grid::new(Size::new(4, 2));
        grid.apply(&RowUpdate {
            row: 0,
            cells: row_of("abcd", Color::Default, CellAttrs::NONE),
        })
        .expect("a 4-cell update fits");
        grid.resize(Size::new(2, 3));
        assert_eq!(grid.size(), Size::new(2, 3));
        assert_eq!(
            grid.row(0)
                .map(|r| r.iter().map(|c| c.ch).collect::<String>()),
            Some("ab".to_owned()),
            "the surviving columns keep their content"
        );
        assert_eq!(
            grid.row(2)
                .map(|r| r.iter().map(|c| c.ch).collect::<String>()),
            Some("  ".to_owned()),
            "the new row is blank"
        );
    }

    // -- Renderer ---------------------------------------------------------

    #[test]
    fn a_blank_frame_is_byte_for_byte_deterministic() {
        let grid = Grid::new(Size::new(2, 2));
        let mut renderer = Renderer::new(TermCaps::default());
        assert_eq!(
            renderer.render_full(&grid, None),
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0m  \x1b[2;1H  \x1b[0m"
        );
    }

    #[test]
    fn rendering_twice_produces_the_same_bytes() {
        let mut grid = Grid::new(Size::new(3, 1));
        grid.apply(&RowUpdate {
            row: 0,
            cells: row_of("abc", Color::Indexed(4), CellAttrs::BOLD),
        })
        .expect("a 3-cell update fits");
        let mut renderer = Renderer::new(truecolor());
        let first = renderer.render_full(&grid, None).to_vec();
        let second = renderer.render_full(&grid, None).to_vec();
        assert_eq!(first, second, "the buffer must be cleared between frames");
    }

    #[test]
    fn incremental_damage_repaints_only_the_named_row() {
        let mut grid = Grid::new(Size::new(2, 2));
        grid.apply(&RowUpdate {
            row: 1,
            cells: row_of("hi", Color::Indexed(4), CellAttrs::BOLD),
        })
        .expect("the damage fits");
        let mut renderer = Renderer::new(TermCaps::default());
        assert_eq!(
            renderer.render_rows(
                &grid,
                &[1],
                Some(Cursor::new(Point::new(1, 1), CursorShape::Block)),
            ),
            b"\x1b[?25l\x1b[2;1H\x1b[0;1;38;5;4mhi\x1b[0m\x1b[2;2H\x1b[2 q\x1b[?25h"
        );
    }

    #[test]
    fn incremental_damage_does_not_clear_the_outer_terminal() {
        let grid = Grid::new(Size::new(1, 1));
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_rows(&grid, &[0], None);
        assert!(!frame.windows(3).any(|bytes| bytes == b"\x1b[2J"));
    }

    #[test]
    fn a_run_of_one_style_emits_one_sgr() {
        let mut grid = Grid::new(Size::new(3, 1));
        grid.apply(&RowUpdate {
            row: 0,
            cells: row_of("abc", Color::Indexed(4), CellAttrs::BOLD),
        })
        .expect("a 3-cell update fits");
        let mut renderer = Renderer::new(TermCaps::default());
        assert_eq!(
            renderer.render_full(&grid, None),
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0;1;38;5;4mabc\x1b[0m"
        );
    }

    #[test]
    fn a_style_change_mid_row_re_emits_absolutely() {
        let mut cells = row_of("ab", Color::Default, CellAttrs::NONE);
        cells[1].fg = Color::Indexed(1);
        cells[1].attrs = CellAttrs::UNDERLINE;
        let mut grid = Grid::new(Size::new(2, 1));
        grid.apply(&RowUpdate { row: 0, cells })
            .expect("a 2-cell update fits");
        let mut renderer = Renderer::new(TermCaps::default());
        // The second cell leads with `0`, so it never inherits the first's
        // rendition even if a frame is dropped.
        assert_eq!(
            renderer.render_full(&grid, None),
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0ma\x1b[0;4;38;5;1mb\x1b[0m"
        );
    }

    #[test]
    fn every_attribute_has_a_code_and_they_emit_in_order() {
        let attrs = CellAttrs::BOLD
            .union(CellAttrs::DIM)
            .union(CellAttrs::ITALIC)
            .union(CellAttrs::UNDERLINE)
            .union(CellAttrs::REVERSE)
            .union(CellAttrs::HIDDEN)
            .union(CellAttrs::STRIKETHROUGH);
        let mut out = String::new();
        push_sgr(
            &mut out,
            Style {
                fg: Color::Default,
                bg: Color::Default,
                attrs,
            },
            TermCaps::default(),
        );
        assert_eq!(out, "\x1b[0;1;2;3;4;7;8;9m");
    }

    #[test]
    fn a_background_uses_the_48_selector() {
        let mut out = String::new();
        push_sgr(
            &mut out,
            Style {
                fg: Color::Indexed(2),
                bg: Color::Indexed(3),
                attrs: CellAttrs::NONE,
            },
            TermCaps::default(),
        );
        assert_eq!(out, "\x1b[0;38;5;2;48;5;3m");
    }

    #[test]
    fn truecolor_is_emitted_only_when_the_terminal_claims_it() {
        let style = Style {
            fg: Color::Rgb(255, 0, 0),
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        };
        let mut rgb = String::new();
        push_sgr(&mut rgb, style, truecolor());
        assert_eq!(rgb, "\x1b[0;38;2;255;0;0m");

        let mut fallback = String::new();
        push_sgr(&mut fallback, style, TermCaps::default());
        assert_eq!(fallback, "\x1b[0;38;5;196m", "downsampled, not emitted raw");
    }

    #[test]
    fn rgb_downsampling_hits_the_expected_palette_entries() {
        // The endpoints are exact in the cube, so they skip the greyscale ramp.
        assert_eq!(downsample_rgb(0, 0, 0), 16, "cube black");
        assert_eq!(downsample_rgb(255, 255, 255), 231, "cube white");
        assert_eq!(downsample_rgb(255, 0, 0), 196);
        assert_eq!(downsample_rgb(0, 255, 0), 46);
        assert_eq!(downsample_rgb(0, 0, 255), 21);
        // Near-grey takes the finer 24-step ramp instead of the cube.
        assert_eq!(downsample_rgb(128, 130, 131), 244);
        assert!((232..=255).contains(&downsample_rgb(8, 8, 8)));
    }

    #[test]
    fn the_cursor_is_placed_and_shown_after_the_paint() {
        let grid = Grid::new(Size::new(2, 2));
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer
            .render_full(
                &grid,
                Some(Cursor::new(Point::new(1, 1), CursorShape::Beam)),
            )
            .to_vec();
        assert!(frame.starts_with(b"\x1b[?25l"), "hidden while painting");
        assert!(
            frame.ends_with(b"\x1b[0m\x1b[2;2H\x1b[6 q\x1b[?25h"),
            "reset, then place, then shape, then show"
        );
    }

    #[test]
    fn no_cursor_leaves_it_hidden() {
        let grid = Grid::new(Size::new(1, 1));
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_full(&grid, None).to_vec();
        assert!(
            !frame.ends_with(b"\x1b[?25h"),
            "nothing re-shows the cursor"
        );
    }

    #[test]
    fn every_cursor_shape_has_a_distinct_sequence() {
        let all = [
            shape_sequence(CursorShape::Block),
            shape_sequence(CursorShape::Underline),
            shape_sequence(CursorShape::Beam),
        ];
        assert_eq!(all, ["\x1b[2 q", "\x1b[4 q", "\x1b[6 q"]);
    }

    #[test]
    fn wide_characters_survive_the_render_intact() {
        let mut grid = Grid::new(Size::new(2, 1));
        grid.apply(&RowUpdate {
            row: 0,
            cells: row_of("→é", Color::Default, CellAttrs::NONE),
        })
        .expect("a 2-cell update fits");
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_full(&grid, None).to_vec();
        assert_eq!(
            String::from_utf8(frame).expect("output is valid utf-8"),
            "\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0m→é\x1b[0m"
        );
    }

    // -- Spans ------------------------------------------------------------

    #[test]
    fn a_span_paints_chrome_at_its_own_origin() {
        let mut renderer = Renderer::new(TermCaps::default());
        let span = Span::new(
            Point::new(4, 2),
            row_of("hi", Color::Indexed(5), CellAttrs::BOLD),
        );
        assert_eq!(
            renderer.render_spans(&[span], None),
            b"\x1b[?25l\x1b[3;5H\x1b[0;1;38;5;5mhi\x1b[0m"
        );
    }

    #[test]
    fn each_span_restates_its_style_absolutely() {
        let mut renderer = Renderer::new(TermCaps::default());
        let spans = [
            Span::new(
                Point::new(0, 0),
                row_of("a", Color::Indexed(1), CellAttrs::NONE),
            ),
            Span::new(
                Point::new(0, 1),
                row_of("b", Color::Indexed(1), CellAttrs::NONE),
            ),
        ];
        let frame = renderer.render_spans(&spans, None).to_vec();
        let sgr = frame.windows(4).filter(|bytes| bytes == b"\x1b[0;").count();
        assert_eq!(sgr, 2, "the second span must not inherit the first's style");
    }

    #[test]
    fn an_empty_span_moves_nothing() {
        let mut renderer = Renderer::new(TermCaps::default());
        assert_eq!(
            renderer.render_spans(&[Span::new(Point::new(9, 9), Vec::new())], None),
            b"\x1b[?25l\x1b[0m"
        );
    }

    #[test]
    fn spans_never_clear_the_outer_terminal() {
        let mut renderer = Renderer::new(TermCaps::default());
        let span = Span::new(
            Point::new(0, 0),
            row_of("x", Color::Default, CellAttrs::NONE),
        );
        let frame = renderer.render_spans(&[span], None).to_vec();
        assert!(!frame.windows(3).any(|bytes| bytes == b"\x1b[2J"));
    }

    #[test]
    fn a_status_bar_keeps_its_ascii_signals_without_truecolor() {
        let mut queue = crate::chrome::AttentionQueue::new();
        queue.record(1, "build", crate::chrome::Attention::NeedsInput);
        let tabs = [cloo_proto::TabSummary {
            tab: cloo_proto::TabId::new(4),
            title: "build".into(),
            active: true,
        }];
        let span = crate::chrome::status_bar_span(
            Point::new(0, 23),
            cloo_proto::SessionId::new(1),
            &tabs,
            &queue,
            40,
        );

        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_spans(&[span], None).to_vec();
        assert!(
            !frame.windows(3).any(|bytes| bytes == b";2;"),
            "a terminal without truecolor must not receive 24-bit SGR"
        );
        for token in [b"session:1".as_slice(), b">1 build", b"!", b"C-b ?"] {
            assert!(
                frame.windows(token.len()).any(|bytes| bytes == token),
                "missing ASCII status token {:?}",
                std::str::from_utf8(token).expect("test tokens are UTF-8")
            );
        }
    }

    #[test]
    fn output_matches_the_last_frame() {
        let grid = Grid::new(Size::new(1, 1));
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_full(&grid, None).to_vec();
        assert_eq!(renderer.output(), frame.as_slice());
    }
}
