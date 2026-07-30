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

use crate::chrome::{
    ChromeOptions, PaneChrome, StatusBar, TabBar, body_span, bottom_frame_cells, side_frame_cell,
    status_bar_span, tab_row_span, top_frame_cells,
};
use crate::input::PaneArea;
use crate::motion::{MotionKind, Phase, phase_cell};

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

/// One pane's place in a composed frame.
///
/// Pairs the geometry the client resolved — a [`PaneArea`], the very rect the
/// hit-tester answers a mouse report against — with the two things only the
/// client holds: its cached [`Grid`] and the [`PaneChrome`] its header reads
/// from. Composing a frame from these is what keeps the picture a user points at
/// and the picture the client drew one and the same.
#[derive(Debug, Clone)]
pub struct FramePane<'a> {
    /// Where the pane's grid sits, and whether a header was drawn above it.
    pub area: PaneArea,
    /// The client's cache of the pane's visible cells.
    pub grid: &'a Grid,
    /// What the pane's header row says, and whether the pane is focused.
    pub header: PaneChrome,
}

impl<'a> FramePane<'a> {
    /// Pairs a resolved area with its grid cache and header description.
    #[must_use]
    pub fn new(area: PaneArea, grid: &'a Grid, header: PaneChrome) -> Self {
        Self { area, grid, header }
    }
}

/// Composes the whole attached frame into positioned [`Span`]s.
///
/// One projection of one already-resolved layout: the tab bar owns row zero, the
/// status bar owns the last row, and every visible pane's grid drops into its
/// own [`PaneArea`] with a header on the row above it. Nothing here guesses an
/// offset — the tab and status rows are the frame's fixed edges, and each pane
/// carries the rect the same layout pass gave the hit-tester, so the drawn frame
/// and the mouse map cannot disagree. Chrome (headers, the tab row, the status
/// row and the attention summary it carries) and pane bodies alike come back as
/// spans [`Renderer::render_spans`] draws, which keeps this a pure function into
/// cells and leaves the renderer the only place bytes are produced.
///
/// The spans are ordered top to bottom — the tab row, then each pane's header
/// and body, then the status row — but they never overlap, so the order is for a
/// reader's sake rather than for correctness. A zero-width or zero-height frame,
/// which a violent resize can produce, composes nothing rather than panicking.
///
/// `bar` carries the tab ordering plus the workspace metadata the top row may
/// spend spare width on. It is passed in whole rather than reassembled here, so
/// the session name and client count keep their daemon provenance and the pane
/// count keeps the client's own.
///
/// `status` keeps each status field attached to its actual projection rather
/// than asking the frame composer to derive session or client-local values.
#[must_use]
pub fn compose_frame(
    size: Size,
    bar: TabBar<'_>,
    panes: &[FramePane<'_>],
    status: StatusBar<'_>,
    options: ChromeOptions,
) -> Vec<Span> {
    let mut spans = Vec::new();
    if size.cols == 0 || size.rows == 0 {
        return spans;
    }

    if !bar.tabs().is_empty() {
        spans.push(tab_row_span(Point::new(0, 0), bar, size.cols, options));
    }

    for pane in panes {
        let area = pane.area;
        if area.framed && area.y > 0 {
            spans.push(Span::new(
                Point::new(area.x.saturating_sub(1), area.y - 1),
                top_frame_cells(&pane.header, area.size.cols, options),
            ));
        }
        for row in 0..pane.grid.size().rows {
            let Some(cells) = pane.grid.row(row) else {
                continue;
            };
            if area.framed {
                spans.push(Span::new(
                    Point::new(area.x.saturating_sub(1), area.y.saturating_add(row)),
                    vec![side_frame_cell(pane.header.focused, options)],
                ));
            }
            spans.push(body_span(
                Point::new(area.x, area.y.saturating_add(row)),
                cells,
                pane.header.focused,
                options,
            ));
            if area.framed {
                spans.push(Span::new(
                    Point::new(
                        area.x.saturating_add(area.size.cols),
                        area.y.saturating_add(row),
                    ),
                    vec![side_frame_cell(pane.header.focused, options)],
                ));
            }
        }
        if area.framed {
            spans.push(Span::new(
                Point::new(
                    area.x.saturating_sub(1),
                    area.y.saturating_add(area.size.rows),
                ),
                bottom_frame_cells(pane.header.focused, area.size.cols, options),
            ));
        }
    }

    spans.push(status_bar_span(
        Point::new(0, size.rows - 1),
        status,
        size.cols,
        options,
    ));

    spans
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

    /// Draws one frame of an in-flight transition.
    ///
    /// The same chrome spans [`render_spans`](Self::render_spans) takes, with
    /// [`Phase`] applied to every cell on the way out — the colours ramp toward
    /// `frame` early in a transition and land exactly on the chrome's own by the
    /// end. Two properties follow, and both are tested here rather than trusted:
    /// a settled phase produces bytes *identical* to an ordinary chrome frame,
    /// which is what makes an interrupted transition indistinguishable from a
    /// client that animates nothing; and a transition frame paints chrome only,
    /// so motion can never repaint a pane's contents or clear the screen.
    ///
    /// `frame` is the theme's frame background, resolved by the caller — the
    /// renderer knows about capabilities, never about themes.
    pub fn render_transition(
        &mut self,
        spans: &[Span],
        phase: Phase,
        frame: Color,
        cursor: Option<Cursor>,
    ) -> &[u8] {
        self.out.clear();
        self.out.push_str("\x1b[?25l");
        for span in spans {
            self.paint_span_in_phase(span, phase, frame);
        }
        self.finish(cursor);

        self.output()
    }

    /// Draws a transition over chrome spans while leaving pane-content spans
    /// untouched.
    ///
    /// `chrome` is parallel to `spans`: `true` selects the transition painter
    /// and `false` keeps the server-owned pane cache at its ordinary rendition.
    /// Keeping one ordered list is important — a settled layered frame must be
    /// byte-identical to [`render_spans`](Self::render_spans), and partitioning
    /// the list into two paint passes would change that byte stream even where
    /// the painted cells did not overlap.
    pub fn render_layered_transition(
        &mut self,
        spans: &[Span],
        chrome: &[bool],
        phase: Phase,
        frame: Color,
        cursor: Option<Cursor>,
    ) -> &[u8] {
        debug_assert_eq!(spans.len(), chrome.len());
        self.out.clear();
        self.out.push_str("\x1b[?25l");
        for (span, chrome) in spans.iter().zip(chrome) {
            if *chrome {
                self.paint_span_in_phase(span, phase, frame);
            } else {
                self.paint_span(span);
            }
        }
        self.finish(cursor);

        self.output()
    }

    /// Draws one positioned run of chrome cells.
    fn paint_span(&mut self, span: &Span) {
        self.paint_span_in_phase(span, Phase::settled(MotionKind::Focus), Color::Default);
    }

    /// Draws one positioned run of chrome cells at a point in a transition.
    ///
    /// A settled phase returns each cell unchanged, so this is also the ordinary
    /// span path — one painter, which is why a transition frame cannot drift
    /// from the frame it settles into.
    fn paint_span_in_phase(&mut self, span: &Span, phase: Phase, frame: Color) {
        if span.cells.is_empty() {
            return;
        }
        let mut style = None;
        move_to(&mut self.out, span.at.row, span.at.col);
        for cell in &span.cells {
            let cell = phase_cell(*cell, phase, frame);
            let wanted = Style::of(&cell);
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
        Color::Indexed(index) => push_indexed_color(out, index, selector),
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
        Color::Rgb(r, g, b) => push_indexed_color(out, downsample_rgb(r, g, b), selector),
    }
}

/// Emits one palette colour with the narrowest standard SGR spelling.
///
/// Indices 0 through 15 are the ANSI palette, not merely the first sixteen
/// slots of a 256-colour terminal. Rendering them as `30..=37`/`90..=97` (and
/// their background counterparts) is what makes the theme resolver's
/// 16-colour fallback work on a terminal that does not understand `38;5` at
/// all. Higher indices retain the 256-colour form.
fn push_indexed_color(out: &mut String, index: u8, selector: u32) {
    out.push(';');
    if index < 16 {
        let background = selector == 48;
        let base = match (background, index < 8) {
            (false, true) => 30,
            (false, false) => 90,
            (true, true) => 40,
            (true, false) => 100,
        };
        push_num(out, base + u32::from(index % 8));
        return;
    }
    push_num(out, selector);
    out.push_str(";5;");
    push_num(out, u32::from(index));
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
            b"\x1b[?25l\x1b[2;1H\x1b[0;1;34mhi\x1b[0m\x1b[2;2H\x1b[2 q\x1b[?25h"
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
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0;1;34mabc\x1b[0m"
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
            b"\x1b[?25l\x1b[H\x1b[2J\x1b[1;1H\x1b[0ma\x1b[0;4;31mb\x1b[0m"
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
    fn ansi_palette_entries_use_basic_foreground_and_background_codes() {
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
        assert_eq!(out, "\x1b[0;32;43m");
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
            b"\x1b[?25l\x1b[3;5H\x1b[0;1;35mhi\x1b[0m"
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
        let hint = crate::chrome::PrefixHint::default();
        let span = crate::chrome::status_bar_span(
            Point::new(0, 23),
            crate::chrome::StatusBar::new(&tabs, &queue, &hint).session("main"),
            40,
            ChromeOptions::default().with_theme(crate::theme::Theme::named(
                cloo_core::ThemeName::Storm,
                TermCaps::default(),
            )),
        );

        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_spans(&[span], None).to_vec();
        assert!(
            !frame.windows(3).any(|bytes| bytes == b";2;"),
            "a terminal without truecolor must not receive 24-bit SGR"
        );
        let text = visible(&frame);
        for token in ["s main", ">1 build", "!", "C-b"] {
            assert!(
                text.contains(token),
                "missing ASCII status token {token:?} in {text:?}"
            );
        }
    }

    #[test]
    fn the_first_attach_clues_survive_a_sixteen_colour_terminal() {
        let tabs = [cloo_proto::TabSummary {
            tab: cloo_proto::TabId::new(4),
            title: "build".into(),
            active: true,
        }];
        let queue = crate::chrome::AttentionQueue::new();
        let hint = crate::chrome::PrefixHint::for_panes("M-a", 1).pending(true);
        let span = crate::chrome::status_bar_span(
            Point::new(0, 23),
            crate::chrome::StatusBar::new(&tabs, &queue, &hint).session("main"),
            60,
            ChromeOptions::default().with_theme(crate::theme::Theme::named(
                cloo_core::ThemeName::Storm,
                TermCaps::default(),
            )),
        );

        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_spans(&[span], None).to_vec();
        assert!(
            !frame.windows(3).any(|bytes| bytes == b";2;"),
            "a terminal without truecolor must not receive 24-bit SGR"
        );
        // The configured chord, its pending brackets, and every clue key are
        // characters, so a terminal with no colour at all loses none of them.
        let text = visible(&frame);
        assert!(
            text.contains("[M-a] split % stack \" help ?"),
            "the hint lost a text signal without truecolor: {text:?}"
        );
        assert!(
            frame.is_ascii(),
            "the first-attach hint must not need a non-ASCII glyph"
        );
    }

    /// The characters a terminal would show, with every escape sequence gone.
    ///
    /// A styled field is split across SGR sequences, so a byte-window search
    /// cannot tell "the text is missing" from "the text changed colour halfway".
    fn visible(frame: &[u8]) -> String {
        let rendered = String::from_utf8_lossy(frame);
        let mut text = String::new();
        let mut chars = rendered.chars();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                text.push(ch);
                continue;
            }
            if chars.next() != Some('[') {
                continue;
            }
            for byte in chars.by_ref() {
                if ('@'..='~').contains(&byte) {
                    break;
                }
            }
        }
        text
    }

    // -- Transitions ------------------------------------------------------

    #[test]
    fn a_settled_transition_frame_is_byte_identical_to_an_ordinary_one() {
        let spans = [Span::new(
            Point::new(0, 0),
            row_of("claude", Color::Rgb(0xbb, 0x9a, 0xf7), CellAttrs::BOLD),
        )];
        let mut renderer = Renderer::new(truecolor());
        let ordinary = renderer.render_spans(&spans, None).to_vec();
        let settled = renderer
            .render_transition(
                &spans,
                Phase::settled(MotionKind::Focus),
                Color::Rgb(0x0f, 0x0f, 0x16),
                None,
            )
            .to_vec();
        assert_eq!(
            settled, ordinary,
            "an interrupted transition must draw what no motion would"
        );
    }

    #[test]
    fn a_settled_layered_transition_keeps_pane_contents_out_of_motion() {
        let spans = [
            Span::new(
                Point::new(0, 0),
                row_of("header", Color::Rgb(0xbb, 0x9a, 0xf7), CellAttrs::BOLD),
            ),
            Span::new(
                Point::new(0, 1),
                row_of("child", Color::Rgb(0xc0, 0xca, 0xf5), CellAttrs::NONE),
            ),
        ];
        let mut renderer = Renderer::new(truecolor());
        let ordinary = renderer.render_spans(&spans, None).to_vec();
        let settled = renderer
            .render_layered_transition(
                &spans,
                &[true, false],
                Phase::settled(MotionKind::Focus),
                Color::Rgb(0x0f, 0x0f, 0x16),
                None,
            )
            .to_vec();
        assert_eq!(settled, ordinary);

        let start =
            crate::motion::Motion::default().start(MotionKind::Focus, std::time::Instant::now());
        let moving = renderer
            .render_layered_transition(
                &spans,
                &[true, false],
                start,
                Color::Rgb(0x0f, 0x0f, 0x16),
                None,
            )
            .to_vec();
        let moving = String::from_utf8(moving).expect("rendered terminal bytes are UTF-8");
        assert!(moving.contains("child"));
        assert!(
            moving.contains("38;2;192;202;245"),
            "the pane body keeps its original foreground"
        );
    }

    #[test]
    fn a_mid_transition_frame_ramps_the_colour_without_touching_the_text() {
        let spans = [Span::new(
            Point::new(0, 0),
            row_of("hi", Color::Rgb(0xbb, 0x9a, 0xf7), CellAttrs::NONE),
        )];
        let mut renderer = Renderer::new(truecolor());
        let mut motion = crate::motion::Motion::default();
        let phase = motion.start(MotionKind::Focus, std::time::Instant::now());
        let frame = renderer
            .render_transition(&spans, phase, Color::Rgb(0x0f, 0x0f, 0x16), None)
            .to_vec();
        let text = String::from_utf8(frame.clone()).expect("output is valid utf-8");
        assert!(text.ends_with("hi\x1b[0m"), "the characters are untouched");
        assert!(
            !text.contains("38;2;187;154;247"),
            "the accent has not landed yet"
        );
    }

    #[test]
    fn a_transition_frame_never_clears_the_outer_terminal() {
        let spans = [Span::new(
            Point::new(0, 0),
            row_of("x", Color::Default, CellAttrs::NONE),
        )];
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer
            .render_transition(
                &spans,
                Phase::settled(MotionKind::Split),
                Color::Rgb(0x0f, 0x0f, 0x16),
                None,
            )
            .to_vec();
        assert!(
            !frame.windows(3).any(|bytes| bytes == b"\x1b[2J"),
            "motion paints chrome, never a full repaint"
        );
    }

    #[test]
    fn output_matches_the_last_frame() {
        let grid = Grid::new(Size::new(1, 1));
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = renderer.render_full(&grid, None).to_vec();
        assert_eq!(renderer.output(), frame.as_slice());
    }

    // -- Frame composition ------------------------------------------------

    use cloo_proto::{PaneId, TabId, TabSummary};

    use crate::chrome::Attention;

    /// A grid every cell of which is `ch`.
    fn filled_grid(size: Size, ch: char) -> Grid {
        let mut grid = Grid::new(size);
        for row in 0..size.rows {
            grid.apply(&RowUpdate {
                row,
                cells: (0..size.cols)
                    .map(|_| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect(),
            })
            .expect("a full-width row fits");
        }
        grid
    }

    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    fn one_tab() -> Vec<TabSummary> {
        vec![TabSummary {
            tab: TabId::new(1),
            title: "work".into(),
            active: true,
        }]
    }

    fn compose_test_frame(
        size: Size,
        bar: TabBar<'_>,
        panes: &[FramePane<'_>],
        queue: &crate::chrome::AttentionQueue,
        hint: &crate::chrome::PrefixHint,
        options: ChromeOptions,
    ) -> Vec<Span> {
        let status = StatusBar::new(bar.tabs(), queue, hint).session("main");
        compose_frame(size, bar, panes, status, options)
    }

    #[test]
    fn compose_lays_every_pane_grid_into_its_rect_with_chrome_around_it() {
        let size = Size::new(20, 6);
        let tabs = one_tab();
        let left_grid = filled_grid(Size::new(8, 2), 'a');
        let right_grid = filled_grid(Size::new(8, 2), 'b');
        let panes = [
            FramePane::new(
                PaneArea::new(PaneId::new(1), 1, 2, Size::new(8, 2)),
                &left_grid,
                PaneChrome::new(1, "one").focused(true),
            ),
            FramePane::new(
                PaneArea::new(PaneId::new(2), 11, 2, Size::new(8, 2)),
                &right_grid,
                PaneChrome::new(2, "two").attention(Attention::NeedsInput),
            ),
        ];
        let queue = crate::chrome::AttentionQueue::new();

        let hint = crate::chrome::PrefixHint::default();
        let spans = compose_test_frame(
            size,
            TabBar::new(&tabs),
            &panes,
            &queue,
            &hint,
            ChromeOptions::default(),
        );

        // Tab row, then each complete frame (top, three spans per body row,
        // bottom), then status. No two spans occupy the same cell.
        assert_eq!(spans.len(), 1 + (1 + 3 * 2 + 1) * 2 + 1);

        // The tab row owns row zero, full width.
        assert_eq!(spans[0].at, Point::new(0, 0));
        assert_eq!(spans[0].cells.len(), 20);

        // Pane one's header sits on the row directly above its grid.
        assert_eq!(spans[1].at, Point::new(0, 1));
        assert_eq!(text_of(&spans[1].cells), "┌> 1 on ?┐");
        assert_eq!(
            spans[2].cells[0].fg,
            crate::theme::Theme::storm().color(crate::theme::ThemeToken::Accent),
            "focus accents the complete frame, not only its header"
        );

        // Its focused body drops in undimmed at exactly the pane's rect origin.
        // Default child colours resolve on the copied span while the cached
        // grid remains byte-for-byte unchanged.
        let left_before = left_grid.clone();
        for (offset, span) in [spans[3].clone(), spans[6].clone()].iter().enumerate() {
            let row = u16::try_from(offset).expect("small");
            assert_eq!(span.at, Point::new(1, 2 + row));
            let source = left_grid.row(row).expect("row exists");
            assert_eq!(
                span.cells,
                source
                    .iter()
                    .copied()
                    .map(|cell| crate::theme::Theme::storm().map_child_cell(cell))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(left_grid, left_before);

        // Pane two starts at its own left edge, header above grid.
        assert_eq!(spans[9].at, Point::new(10, 1));
        assert!(text_of(&spans[9].cells).contains("tw"));
        assert!(text_of(&spans[9].cells).contains('!'));
        assert_eq!(spans[11].at, Point::new(11, 2));
        assert_ne!(
            spans[10].cells[0].fg,
            crate::theme::Theme::storm().color(crate::theme::ThemeToken::Border),
            "the unfocused frame follows the dimming preference"
        );

        // The status row owns the last row, full width, and keeps the session
        // marker and prefix hint when the logical name cannot fit.
        let status = spans.last().expect("a status span");
        assert_eq!(status.at, Point::new(0, 5));
        assert_eq!(status.cells.len(), 20);
        let status_text = text_of(&status.cells);
        assert!(status_text.contains('s'), "session marker: {status_text:?}");
        assert!(status_text.contains("C-b"), "prefix hint: {status_text:?}");
    }

    #[test]
    fn nested_frames_cover_every_edge_without_overlapping() {
        let left = filled_grid(Size::new(8, 6), 'l');
        let upper = filled_grid(Size::new(8, 2), 'u');
        let lower = filled_grid(Size::new(8, 2), 'd');
        let panes = [
            FramePane::new(
                PaneArea::new(PaneId::new(1), 1, 2, left.size()),
                &left,
                PaneChrome::new(1, "left").focused(true),
            ),
            FramePane::new(
                PaneArea::new(PaneId::new(2), 11, 2, upper.size()),
                &upper,
                PaneChrome::new(2, "upper"),
            ),
            FramePane::new(
                PaneArea::new(PaneId::new(3), 11, 6, lower.size()),
                &lower,
                PaneChrome::new(3, "lower").attention(Attention::Ready),
            ),
        ];
        let tabs = one_tab();
        let queue = crate::chrome::AttentionQueue::new();
        let hint = crate::chrome::PrefixHint::default();
        let spans = compose_test_frame(
            Size::new(20, 10),
            TabBar::new(&tabs),
            &panes,
            &queue,
            &hint,
            ChromeOptions::default(),
        );

        let mut occupied = std::collections::HashSet::new();
        for span in &spans {
            for offset in 0..span.cells.len() {
                let col = span
                    .at
                    .col
                    .saturating_add(u16::try_from(offset).expect("small frame"));
                assert!(
                    occupied.insert((col, span.at.row)),
                    "two spans overlap at ({col}, {})",
                    span.at.row
                );
            }
        }
        assert_eq!(
            occupied.len(),
            20 * 10,
            "the nested frame leaves no stale background cells"
        );
    }

    #[test]
    fn an_unfocused_pane_body_is_dimmed_and_a_focused_one_is_not() {
        let grid = filled_grid(Size::new(4, 1), 'x');
        let before = grid.clone();
        let make = |focused: bool| {
            let panes = [FramePane::new(
                PaneArea::new(PaneId::new(1), 1, 1, Size::new(4, 1)),
                &grid,
                PaneChrome::new(1, "p").focused(focused),
            )];
            compose_test_frame(
                Size::new(6, 4),
                TabBar::default(),
                &panes,
                &crate::chrome::AttentionQueue::new(),
                &crate::chrome::PrefixHint::default(),
                ChromeOptions::default(),
            )
        };
        let focused = make(true);
        let unfocused = make(false);
        let themed = grid
            .row(0)
            .expect("row exists")
            .iter()
            .copied()
            .map(|cell| crate::theme::Theme::storm().map_child_cell(cell))
            .collect::<Vec<_>>();
        let focused_body = focused
            .iter()
            .find(|span| span.at == Point::new(1, 1) && span.cells.len() == 4)
            .expect("focused body");
        let unfocused_body = unfocused
            .iter()
            .find(|span| span.at == Point::new(1, 1) && span.cells.len() == 4)
            .expect("unfocused body");
        assert_eq!(focused_body.cells, themed, "a focused body is not dimmed");
        assert_ne!(unfocused_body.cells, themed, "an unfocused body recedes");
        assert_eq!(
            text_of(&unfocused_body.cells),
            "xxxx",
            "dimming changes colour, never the text"
        );
        assert_eq!(grid, before, "composition must not alter the cached grid");
    }

    #[test]
    fn a_headerless_pane_composes_no_header_row() {
        let grid = filled_grid(Size::new(4, 2), 'q');
        let panes = [FramePane::new(
            PaneArea::new(PaneId::new(1), 0, 0, Size::new(4, 2)).headerless(),
            &grid,
            PaneChrome::new(1, "p"),
        )];
        let spans = compose_test_frame(
            Size::new(4, 3),
            TabBar::default(),
            &panes,
            &crate::chrome::AttentionQueue::new(),
            &crate::chrome::PrefixHint::default(),
            ChromeOptions::default(),
        );
        // Two body rows and the status row; no header was drawn.
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].at, Point::new(0, 0));
        assert_eq!(spans[1].at, Point::new(0, 1));
    }

    #[test]
    fn a_composed_frame_renders_the_grid_at_its_rect_origin() {
        let size = Size::new(6, 3);
        let grid = filled_grid(Size::new(3, 1), 'z');
        let panes = [FramePane::new(
            PaneArea::new(PaneId::new(1), 2, 1, Size::new(3, 1)),
            &grid,
            PaneChrome::new(1, "p").focused(true),
        )];
        let spans = compose_test_frame(
            size,
            TabBar::default(),
            &panes,
            &crate::chrome::AttentionQueue::new(),
            &crate::chrome::PrefixHint::default(),
            ChromeOptions::default(),
        );
        let mut renderer = Renderer::new(TermCaps::default());
        let frame = String::from_utf8(renderer.render_spans(&spans, None).to_vec())
            .expect("output is valid utf-8");
        // The grid's row lands at column 2, row 1 (CUP is one-based): row 2,
        // column 3.
        let (_, body) = frame
            .split_once("\x1b[2;3H")
            .expect("the grid row moves to its rect origin");
        assert!(
            body.split_once('m')
                .is_some_and(|(_, text)| text.starts_with("zzz")),
            "the themed grid did not land at its rect origin: {frame:?}"
        );
    }

    #[test]
    fn a_zero_sized_frame_composes_nothing() {
        let grid = Grid::new(Size::new(0, 0));
        let panes = [FramePane::new(
            PaneArea::new(PaneId::new(1), 0, 0, Size::new(0, 0)),
            &grid,
            PaneChrome::new(1, "p"),
        )];
        assert!(
            compose_test_frame(
                Size::new(0, 0),
                TabBar::new(&one_tab()),
                &panes,
                &crate::chrome::AttentionQueue::new(),
                &crate::chrome::PrefixHint::default(),
                ChromeOptions::default(),
            )
            .is_empty()
        );
    }
}
