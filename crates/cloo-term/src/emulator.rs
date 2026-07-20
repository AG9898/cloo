//! The emulation wrapper itself.
//!
//! Everything `alacritty_terminal` in the workspace is contained in this file.
//! Nothing in the public API below mentions one of its types — that boundary,
//! plus the exact version pin in the root manifest, is the whole mitigation for
//! upstream API churn. See `docs/DECISIONS.md` RESOLVED-02.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as VteColor, CursorShape as VteCursorShape, NamedColor, Processor,
};

use crate::cell::{Cell, CellAttrs, Color, CursorShape, CursorState, TermSize};

/// Default scrollback depth, in lines, for a newly created pane.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// A single pane's terminal emulator: bytes in, cells out.
///
/// One of these exists per pane, owned by the session task. It is deliberately
/// synchronous and does no I/O of its own; the PTY reactor reads bytes and
/// hands them to [`feed`](Self::feed).
pub struct Emulator {
    term: Term<VoidListener>,
    parser: Processor,
    size: TermSize,
}

/// Bridges a [`TermSize`] to the backend's dimensions trait.
///
/// `total_lines` reports only the screen lines: it describes the *viewport*
/// being requested, and the backend adds the configured scrollback itself.
struct Dims {
    cols: usize,
    rows: usize,
}

impl From<TermSize> for Dims {
    fn from(size: TermSize) -> Self {
        Self {
            cols: usize::from(size.cols()),
            rows: usize::from(size.rows()),
        }
    }
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

impl Emulator {
    /// Creates an emulator over a grid of `size` with `scrollback` lines of
    /// history retained above the viewport.
    #[must_use]
    pub fn new(size: TermSize, scrollback: usize) -> Self {
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &Dims::from(size), VoidListener),
            parser: Processor::new(),
            size,
        }
    }

    /// Creates an emulator with [`DEFAULT_SCROLLBACK_LINES`] of history.
    #[must_use]
    pub fn with_default_scrollback(size: TermSize) -> Self {
        Self::new(size, DEFAULT_SCROLLBACK_LINES)
    }

    /// Feeds PTY output into the parser, updating the grid.
    ///
    /// Byte boundaries do not matter: parser state is retained across calls, so
    /// an escape sequence or a multi-byte character split across two reads is
    /// handled correctly. This never fails — malformed input is the emulator's
    /// problem to absorb, not the caller's to handle.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// The current grid size.
    #[must_use]
    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Resizes the grid, reflowing content.
    ///
    /// This only touches emulation state. Telling the child process about the
    /// new size (`TIOCSWINSZ`) is the PTY layer's job, and both must happen —
    /// see the resize ordering note in `AGENTS.md`.
    pub fn resize(&mut self, size: TermSize) {
        self.term.resize(Dims::from(size));
        self.size = size;
    }

    /// Reads one row of the visible grid, top-down from the current scroll
    /// position. Returns `None` if `row` is past the bottom of the grid.
    ///
    /// The returned vector always has exactly [`TermSize::cols`] entries.
    #[must_use]
    pub fn row(&self, row: u16) -> Option<Vec<Cell>> {
        if row >= self.size.rows() {
            return None;
        }
        let grid = self.term.grid();
        let line = Line(i32::from(row) - offset_as_i32(grid.display_offset()));
        let source = &grid[line];
        Some(
            (0..usize::from(self.size.cols()))
                .map(|col| convert_cell(&source[Column(col)]))
                .collect(),
        )
    }

    /// Reads the whole visible grid, top row first.
    #[must_use]
    pub fn rows(&self) -> Vec<Vec<Cell>> {
        (0..self.size.rows())
            .filter_map(|row| self.row(row))
            .collect()
    }

    /// Reads one row as text, with trailing blanks trimmed.
    ///
    /// This exists for assertions and logging: comparing a single row's text is
    /// far more readable on failure than comparing a whole grid of cells.
    #[must_use]
    pub fn row_text(&self, row: u16) -> Option<String> {
        let cells = self.row(row)?;
        let text: String = cells.iter().map(|cell| cell.ch).collect();
        Some(text.trim_end().to_owned())
    }

    /// Where the cursor is and how to draw it.
    #[must_use]
    pub fn cursor(&self) -> CursorState {
        let grid = self.term.grid();
        let point = grid.cursor.point;
        let row = point.line.0 + offset_as_i32(grid.display_offset());
        let shape = convert_cursor_shape(self.term.cursor_style().shape);

        // Scrolling back pushes the cursor out of the viewport; so does DECTCEM
        // and an explicitly hidden cursor style.
        let in_view = row >= 0 && row < i32::from(self.size.rows());
        let visible = in_view
            && shape != CursorShape::Hidden
            && self.term.mode().contains(TermMode::SHOW_CURSOR);

        CursorState {
            col: u16::try_from(point.column.0).unwrap_or(u16::MAX),
            row: u16::try_from(row).unwrap_or(0),
            shape,
            visible,
        }
    }

    /// Whether the child has switched to the alternate screen.
    ///
    /// Full-screen programs — and every agent harness worth running — use it.
    /// The alternate screen has no scrollback by design.
    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// How many lines of history sit above the viewport.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.term.grid().history_size()
    }

    /// How far the viewport is scrolled back, in lines. Zero means the viewport
    /// is at the bottom, following live output.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scrolls the viewport by `delta` lines: positive scrolls back into
    /// history, negative scrolls forward. Clamped to the available scrollback.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Returns the viewport to the bottom, following live output again.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }
}

/// A display offset is a line count, always small enough to be a line index.
fn offset_as_i32(offset: usize) -> i32 {
    i32::try_from(offset).unwrap_or(i32::MAX)
}

fn convert_cell(cell: &alacritty_terminal::term::cell::Cell) -> Cell {
    Cell {
        ch: cell.c,
        fg: convert_color(cell.fg),
        bg: convert_color(cell.bg),
        attrs: convert_flags(cell.flags),
    }
}

fn convert_color(color: VteColor) -> Color {
    match color {
        VteColor::Named(named) => convert_named_color(named),
        VteColor::Indexed(index) => Color::Indexed(index),
        VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Maps a named colour onto the palette.
///
/// The 16 ANSI names collapse to their palette indices. Everything that names a
/// *role* rather than a colour — foreground, background, cursor — becomes
/// [`Color::Default`], because the role is resolved by the client's theme, not
/// here. Dim variants map to the base colour and rely on [`CellAttrs::DIM`]
/// riding along in the flags.
fn convert_named_color(named: NamedColor) -> Color {
    match named {
        NamedColor::Black => Color::Indexed(0),
        NamedColor::Red => Color::Indexed(1),
        NamedColor::Green => Color::Indexed(2),
        NamedColor::Yellow => Color::Indexed(3),
        NamedColor::Blue => Color::Indexed(4),
        NamedColor::Magenta => Color::Indexed(5),
        NamedColor::Cyan => Color::Indexed(6),
        NamedColor::White => Color::Indexed(7),
        NamedColor::BrightBlack => Color::Indexed(8),
        NamedColor::BrightRed => Color::Indexed(9),
        NamedColor::BrightGreen => Color::Indexed(10),
        NamedColor::BrightYellow => Color::Indexed(11),
        NamedColor::BrightBlue => Color::Indexed(12),
        NamedColor::BrightMagenta => Color::Indexed(13),
        NamedColor::BrightCyan => Color::Indexed(14),
        NamedColor::BrightWhite => Color::Indexed(15),
        NamedColor::DimBlack => Color::Indexed(0),
        NamedColor::DimRed => Color::Indexed(1),
        NamedColor::DimGreen => Color::Indexed(2),
        NamedColor::DimYellow => Color::Indexed(3),
        NamedColor::DimBlue => Color::Indexed(4),
        NamedColor::DimMagenta => Color::Indexed(5),
        NamedColor::DimCyan => Color::Indexed(6),
        NamedColor::DimWhite => Color::Indexed(7),
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => Color::Default,
    }
}

fn convert_flags(flags: Flags) -> CellAttrs {
    let mut attrs = CellAttrs::NONE;
    for (flag, attr) in [
        (Flags::BOLD, CellAttrs::BOLD),
        (Flags::DIM, CellAttrs::DIM),
        (Flags::ITALIC, CellAttrs::ITALIC),
        (Flags::UNDERLINE, CellAttrs::UNDERLINE),
        (Flags::INVERSE, CellAttrs::REVERSE),
        (Flags::HIDDEN, CellAttrs::HIDDEN),
        (Flags::STRIKEOUT, CellAttrs::STRIKETHROUGH),
    ] {
        if flags.contains(flag) {
            attrs = attrs.union(attr);
        }
    }
    attrs
}

fn convert_cursor_shape(shape: VteCursorShape) -> CursorShape {
    match shape {
        VteCursorShape::Block => CursorShape::Block,
        VteCursorShape::Underline => CursorShape::Underline,
        VteCursorShape::Beam => CursorShape::Beam,
        VteCursorShape::HollowBlock => CursorShape::HollowBlock,
        VteCursorShape::Hidden => CursorShape::Hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TermError;

    fn size(cols: u16, rows: u16) -> TermSize {
        TermSize::new(cols, rows).expect("test size must be non-zero")
    }

    fn emulator(cols: u16, rows: u16) -> Emulator {
        Emulator::with_default_scrollback(size(cols, rows))
    }

    // -- size validation ----------------------------------------------------

    #[test]
    fn zero_size_is_rejected_with_the_offending_dimensions() {
        assert_eq!(
            TermSize::new(0, 24),
            Err(TermError::ZeroSize { cols: 0, rows: 24 })
        );
        assert_eq!(
            TermSize::new(80, 0),
            Err(TermError::ZeroSize { cols: 80, rows: 0 })
        );
        assert!(TermSize::new(80, 24).is_ok());
    }

    // -- feeding ------------------------------------------------------------

    #[test]
    fn plain_text_lands_in_the_first_row() {
        let mut term = emulator(20, 5);
        term.feed(b"hello");
        assert_eq!(term.row_text(0).as_deref(), Some("hello"));
    }

    #[test]
    fn a_row_is_always_exactly_as_wide_as_the_grid() {
        let mut term = emulator(20, 5);
        term.feed(b"hi");
        assert_eq!(term.row(0).expect("row 0 exists").len(), 20);
        assert_eq!(term.rows().len(), 5);
        assert!(term.row(5).is_none());
    }

    #[test]
    fn newline_and_carriage_return_advance_to_the_next_row() {
        let mut term = emulator(20, 5);
        term.feed(b"first\r\nsecond");
        assert_eq!(term.row_text(0).as_deref(), Some("first"));
        assert_eq!(term.row_text(1).as_deref(), Some("second"));
    }

    #[test]
    fn a_sequence_split_across_feeds_is_still_parsed() {
        // The PTY reactor has no control over where a read boundary falls, so
        // parser state must survive one.
        let mut term = emulator(20, 5);
        term.feed(b"\x1b[1");
        term.feed(b"mbold");
        let cells = term.row(0).expect("row 0 exists");
        assert!(cells[0].attrs.contains(CellAttrs::BOLD));
        assert_eq!(term.row_text(0).as_deref(), Some("bold"));
    }

    #[test]
    fn a_utf8_character_split_across_feeds_is_still_decoded() {
        let mut term = emulator(20, 5);
        let bytes = "é".as_bytes();
        term.feed(&bytes[..1]);
        term.feed(&bytes[1..]);
        assert_eq!(term.row_text(0).as_deref(), Some("é"));
    }

    // -- SGR ----------------------------------------------------------------

    #[test]
    fn sgr_sets_every_rendition_flag() {
        let cases: [(&[u8], CellAttrs); 7] = [
            (b"\x1b[1mx", CellAttrs::BOLD),
            (b"\x1b[2mx", CellAttrs::DIM),
            (b"\x1b[3mx", CellAttrs::ITALIC),
            (b"\x1b[4mx", CellAttrs::UNDERLINE),
            (b"\x1b[7mx", CellAttrs::REVERSE),
            (b"\x1b[8mx", CellAttrs::HIDDEN),
            (b"\x1b[9mx", CellAttrs::STRIKETHROUGH),
        ];
        for (input, expected) in cases {
            let mut term = emulator(10, 2);
            term.feed(input);
            let cell = term.row(0).expect("row 0 exists")[0];
            assert!(
                cell.attrs.contains(expected),
                "input {input:?} did not set {expected:?}, got {:?}",
                cell.attrs
            );
        }
    }

    #[test]
    fn sgr_sets_named_indexed_and_rgb_colors() {
        let mut term = emulator(10, 2);
        term.feed(b"\x1b[31;42ma\x1b[38;5;200;48;5;17mb\x1b[38;2;10;20;30mc");
        let cells = term.row(0).expect("row 0 exists");

        assert_eq!(cells[0].fg, Color::Indexed(1));
        assert_eq!(cells[0].bg, Color::Indexed(2));
        assert_eq!(cells[1].fg, Color::Indexed(200));
        assert_eq!(cells[1].bg, Color::Indexed(17));
        assert_eq!(cells[2].fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_reset_returns_a_cell_to_the_default_rendition() {
        let mut term = emulator(10, 2);
        term.feed(b"\x1b[1;31ma\x1b[0mb");
        let cells = term.row(0).expect("row 0 exists");

        assert!(cells[0].attrs.contains(CellAttrs::BOLD));
        assert_eq!(
            cells[1],
            Cell {
                ch: 'b',
                ..Cell::default()
            }
        );
    }

    #[test]
    fn default_colors_stay_default_rather_than_becoming_a_palette_index() {
        // A role name resolves in the client's theme, not here.
        let mut term = emulator(10, 2);
        term.feed(b"\x1b[31ma\x1b[39;49mb");
        let cells = term.row(0).expect("row 0 exists");
        assert_eq!(cells[1].fg, Color::Default);
        assert_eq!(cells[1].bg, Color::Default);
    }

    // -- alternate screen ---------------------------------------------------

    #[test]
    fn entering_and_leaving_the_alternate_screen_restores_the_primary_grid() {
        let mut term = emulator(20, 5);
        term.feed(b"primary");
        assert!(!term.is_alt_screen());

        term.feed(b"\x1b[?1049h");
        assert!(term.is_alt_screen());
        assert_eq!(term.row_text(0).as_deref(), Some(""));

        // 1049 saves the cursor rather than homing it, so a full-screen
        // program positions itself explicitly. Do the same here.
        term.feed(b"\x1b[Halternate");
        assert_eq!(term.row_text(0).as_deref(), Some("alternate"));

        term.feed(b"\x1b[?1049l");
        assert!(!term.is_alt_screen());
        assert_eq!(term.row_text(0).as_deref(), Some("primary"));
    }

    #[test]
    fn the_alternate_screen_accumulates_no_scrollback() {
        let mut term = Emulator::new(size(20, 3), 100);
        term.feed(b"\x1b[?1049h");
        for i in 0..20 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(term.is_alt_screen());
        assert_eq!(term.scrollback_len(), 0);
    }

    // -- cursor -------------------------------------------------------------

    #[test]
    fn cursor_position_follows_output_and_absolute_positioning() {
        let mut term = emulator(20, 5);
        term.feed(b"abc");
        let cursor = term.cursor();
        assert_eq!((cursor.col, cursor.row), (3, 0));
        assert!(cursor.visible);

        // CUP is 1-based on the wire, 0-based in the grid.
        term.feed(b"\x1b[3;5H");
        let cursor = term.cursor();
        assert_eq!((cursor.col, cursor.row), (4, 2));
    }

    #[test]
    fn dectcem_hides_and_shows_the_cursor() {
        let mut term = emulator(20, 5);
        term.feed(b"\x1b[?25l");
        assert!(!term.cursor().visible);
        term.feed(b"\x1b[?25h");
        assert!(term.cursor().visible);
    }

    #[test]
    fn decscusr_selects_the_cursor_shape() {
        let mut term = emulator(20, 5);
        assert_eq!(term.cursor().shape, CursorShape::Block);
        term.feed(b"\x1b[3 q");
        assert_eq!(term.cursor().shape, CursorShape::Underline);
        term.feed(b"\x1b[5 q");
        assert_eq!(term.cursor().shape, CursorShape::Beam);
    }

    // -- resize -------------------------------------------------------------

    #[test]
    fn resize_reports_the_new_size_and_row_width() {
        let mut term = emulator(20, 5);
        term.feed(b"hello");

        term.resize(size(40, 10));
        assert_eq!(term.size(), size(40, 10));
        assert_eq!(term.row(0).expect("row 0 exists").len(), 40);
        assert_eq!(term.rows().len(), 10);
        assert!(term.row(10).is_none());
        assert_eq!(term.row_text(0).as_deref(), Some("hello"));
    }

    #[test]
    fn shrinking_then_growing_preserves_unwrapped_content() {
        let mut term = emulator(20, 5);
        term.feed(b"short\r\nlines");

        term.resize(size(10, 3));
        assert_eq!(term.size(), size(10, 3));
        term.resize(size(20, 5));

        assert_eq!(term.row_text(0).as_deref(), Some("short"));
        assert_eq!(term.row_text(1).as_deref(), Some("lines"));
    }

    #[test]
    fn a_one_by_one_grid_is_valid() {
        // The layout pass squeezes panes to a one-cell floor rather than
        // dropping them, so the emulator has to survive the result.
        let mut term = emulator(20, 5);
        term.resize(size(1, 1));
        term.feed(b"x");
        assert_eq!(term.row(0).expect("row 0 exists").len(), 1);
        assert_eq!(term.rows().len(), 1);
    }

    // -- scrollback ---------------------------------------------------------

    #[test]
    fn scrollback_grows_up_to_its_configured_limit() {
        let mut term = Emulator::new(size(20, 3), 5);
        for i in 0..20 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(term.scrollback_len(), 5);
    }

    #[test]
    fn a_zero_scrollback_grid_retains_no_history() {
        let mut term = Emulator::new(size(20, 3), 0);
        for i in 0..10 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(term.scrollback_len(), 0);
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn scrolling_back_moves_the_viewport_into_history() {
        let mut term = Emulator::new(size(20, 3), 100);
        for i in 0..10 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        // Viewport shows the last two lines plus the cursor's empty row.
        assert_eq!(term.scroll_offset(), 0);
        assert_eq!(term.row_text(0).as_deref(), Some("line 8"));

        term.scroll(3);
        assert_eq!(term.scroll_offset(), 3);
        assert_eq!(term.row_text(0).as_deref(), Some("line 5"));

        term.scroll_to_bottom();
        assert_eq!(term.scroll_offset(), 0);
        assert_eq!(term.row_text(0).as_deref(), Some("line 8"));
    }

    #[test]
    fn scrolling_is_clamped_to_the_available_history() {
        let mut term = Emulator::new(size(20, 3), 100);
        for i in 0..6 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        let history = term.scrollback_len();

        term.scroll(1_000);
        assert_eq!(term.scroll_offset(), history);

        term.scroll(-1_000);
        assert_eq!(term.scroll_offset(), 0);
    }

    #[test]
    fn the_cursor_leaves_the_viewport_when_scrolled_out_of_view() {
        let mut term = Emulator::new(size(20, 3), 100);
        for i in 0..10 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(term.cursor().visible);

        term.scroll(5);
        assert!(!term.cursor().visible);

        term.scroll_to_bottom();
        assert!(term.cursor().visible);
    }
}
