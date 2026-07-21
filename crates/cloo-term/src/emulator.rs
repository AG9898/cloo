//! The emulation wrapper itself.
//!
//! Everything `alacritty_terminal` in the workspace is contained in this file.
//! Nothing in the public API below mentions one of its types — that boundary,
//! plus the exact version pin in the root manifest, is the whole mitigation for
//! upstream API churn. See `docs/DECISIONS.md` RESOLVED-02.

use std::sync::mpsc::{self, Receiver, SyncSender};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as VteColor, CursorShape as VteCursorShape, NamedColor, Processor,
};

use crate::cell::{Cell, CellAttrs, Color, CursorShape, CursorState, TermSize};
use crate::effects::{ClipboardTarget, OuterTerminalEffect};
use crate::modes::{MouseTracking, PaneModes};

/// Default scrollback depth, in lines, for a newly created pane.
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Client-local effects are suppressible, so their queue never waits on a
/// session actor or a renderer.
const EFFECT_QUEUE_CAPACITY: usize = 64;

/// A single pane's terminal emulator: bytes in, cells out.
///
/// One of these exists per pane, owned by the session task. It is deliberately
/// synchronous and does no I/O of its own; the PTY reactor reads bytes and
/// hands them to [`feed`](Self::feed).
pub struct Emulator {
    term: Term<EffectListener>,
    parser: Processor,
    size: TermSize,
    effects: Receiver<OuterTerminalEffect>,
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

/// Collects only the terminal effects cloo has an explicit type for.
///
/// The bounded, non-blocking channel keeps the `Emulator` `Send`, which the
/// session's multi-PTY pump requires. Effects are always safe to suppress, so
/// a full queue drops only a client-local request. Backend events that request
/// a PTY reply or carry arbitrary terminal output are intentionally ignored.
#[derive(Clone)]
struct EffectListener {
    sender: SyncSender<OuterTerminalEffect>,
}

impl EventListener for EffectListener {
    fn send_event(&self, event: Event) {
        let effect = match event {
            // OSC 0/1/2 with an empty payload is the conventional title reset.
            // The backend reports it as `Title("")`, while its separate reset
            // event is used when configuration is reloaded.
            Event::Title(title) if title.is_empty() => Some(OuterTerminalEffect::ResetTitle),
            Event::Title(title) => Some(OuterTerminalEffect::SetTitle(title)),
            Event::ResetTitle => Some(OuterTerminalEffect::ResetTitle),
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                Some(OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::Clipboard,
                    text,
                })
            }
            Event::ClipboardStore(ClipboardType::Selection, text) => {
                Some(OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::PrimarySelection,
                    text,
                })
            }
            // In particular, `PtyWrite`, `ClipboardLoad`, and `ColorRequest`
            // can contain backend-produced control strings. They have no typed
            // cloo effect, so forwarding them is impossible at this boundary.
            _ => None,
        };

        if let Some(effect) = effect {
            // Suppression is a safe fallback for effects, unlike grid damage
            // or a lifecycle event, so a full bounded queue needs no await.
            let _ = self.sender.try_send(effect);
        }
    }
}

impl Emulator {
    /// Creates an emulator over a grid of `size` with `scrollback` lines of
    /// history retained above the viewport.
    #[must_use]
    pub fn new(size: TermSize, scrollback: usize) -> Self {
        let config = Config {
            scrolling_history: scrollback,
            // Off by default in the backend, and an agent harness is exactly the
            // kind of application that pushes a Kitty flag set. Left off, the
            // push is silently discarded and [`modes`](Self::modes) would report
            // legacy keys forever — a wrong answer rather than a missing one.
            kitty_keyboard: true,
            ..Config::default()
        };
        let (sender, effects) = mpsc::sync_channel(EFFECT_QUEUE_CAPACITY);
        let listener = EffectListener { sender };
        Self {
            term: Term::new(config, &Dims::from(size), listener),
            parser: Processor::new(),
            size,
            effects,
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

    /// Drains typed outer-terminal effects observed since the last call.
    ///
    /// The returned values are intent, not raw terminal bytes. The server and
    /// client policy layers decide whether a capable attached terminal may
    /// apply any of them; ignored backend events never appear here.
    #[must_use]
    pub fn drain_effects(&mut self) -> Vec<OuterTerminalEffect> {
        self.effects.try_iter().collect()
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

    /// The input modes the child application has asked for.
    ///
    /// This is what decides how an input event is *encoded* — whether a paste is
    /// bracketed, whether a click is reported at all — and therefore whether a
    /// mouse event belongs to the application or to cloo's chrome. Reading it
    /// from the emulator rather than tracking the sequences separately is the
    /// point: the private mode sets are already parsed here, and a second parser
    /// would be a second answer that could disagree.
    #[must_use]
    pub fn modes(&self) -> PaneModes {
        let mode = self.term.mode();
        let mouse = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseTracking::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseTracking::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseTracking::Click
        } else {
            MouseTracking::Off
        };
        PaneModes {
            mouse,
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            focus_events: mode.contains(TermMode::FOCUS_IN_OUT),
            // Any Kitty flag being live means the application is reading keys in
            // the extended encoding; which flags in particular is its business.
            extended_keys: mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL),
        }
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
    use crate::effects::{GraphicsEffect, ProgressState};
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

    // -- negotiated input modes ---------------------------------------------

    #[test]
    fn an_application_that_negotiates_nothing_has_every_mode_off() {
        let term = emulator(20, 3);
        assert_eq!(term.modes(), PaneModes::default());
    }

    /// One fixture per negotiated mode: the sequence that turns it on, the
    /// predicate that must then hold, and the sequence that turns it off again.
    /// A mode that is set but never cleared is the bug this table catches.
    #[test]
    fn every_negotiated_mode_is_read_back_and_cleared() {
        /// A named fixture: enable it, check it, disable it.
        type ModeCase = (
            &'static str,
            &'static [u8],
            fn(PaneModes) -> bool,
            &'static [u8],
        );
        let cases: [ModeCase; 4] = [
            (
                "bracketed paste",
                b"\x1b[?2004h",
                |m| m.bracketed_paste,
                b"\x1b[?2004l",
            ),
            (
                "focus events",
                b"\x1b[?1004h",
                |m| m.focus_events,
                b"\x1b[?1004l",
            ),
            ("SGR mouse", b"\x1b[?1006h", |m| m.sgr_mouse, b"\x1b[?1006l"),
            (
                "mouse click tracking",
                b"\x1b[?1000h",
                |m| m.mouse == MouseTracking::Click,
                b"\x1b[?1000l",
            ),
        ];

        for (name, enable, holds, disable) in cases {
            let mut term = emulator(20, 3);
            term.feed(enable);
            assert!(holds(term.modes()), "{name} was not seen as enabled");
            term.feed(disable);
            assert_eq!(
                term.modes(),
                PaneModes::default(),
                "{name} was not seen as disabled again"
            );
        }
    }

    #[test]
    fn mouse_tracking_reports_the_highest_level_the_application_asked_for() {
        let mut term = emulator(20, 3);
        term.feed(b"\x1b[?1000h");
        assert_eq!(term.modes().mouse, MouseTracking::Click);
        term.feed(b"\x1b[?1002h");
        assert_eq!(term.modes().mouse, MouseTracking::Drag);
        term.feed(b"\x1b[?1003h");
        assert_eq!(term.modes().mouse, MouseTracking::Motion);
    }

    #[test]
    fn a_kitty_keyboard_push_is_extended_keys() {
        let mut term = emulator(20, 3);
        assert!(!term.modes().extended_keys);
        term.feed(b"\x1b[>1u");
        assert!(
            term.modes().extended_keys,
            "a pushed Kitty flag set means the application reads extended keys"
        );
        term.feed(b"\x1b[<u");
        assert!(!term.modes().extended_keys, "popping must restore legacy");
    }

    #[test]
    fn the_modes_an_application_sets_are_independent() {
        let mut term = emulator(20, 3);
        term.feed(b"\x1b[?2004h");
        let modes = term.modes();
        assert!(modes.bracketed_paste);
        assert!(
            !modes.focus_events && !modes.sgr_mouse && modes.mouse == MouseTracking::Off,
            "one mode must not imply another, got {modes:?}"
        );
    }

    // -- outer-terminal effects --------------------------------------------

    #[test]
    fn allowlisted_outer_terminal_effects_are_typed_and_drained_once() {
        let mut term = emulator(20, 3);
        term.feed(b"\x1b]2;agent task\x07");
        term.feed(b"\x1b]52;c;Y2xpcGJvYXJk\x07");
        term.feed(b"\x1b]52;p;cHJpbWFyeQ==\x07");
        term.feed(b"\x1b]2;\x07");

        assert_eq!(
            term.drain_effects(),
            vec![
                OuterTerminalEffect::SetTitle("agent task".into()),
                OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::Clipboard,
                    text: "clipboard".into(),
                },
                OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::PrimarySelection,
                    text: "primary".into(),
                },
                OuterTerminalEffect::ResetTitle,
            ]
        );
        assert!(
            term.drain_effects().is_empty(),
            "effects must be drained once"
        );
    }

    #[test]
    fn backend_reply_events_cannot_become_raw_outer_terminal_effects() {
        let mut term = emulator(20, 3);

        // Device attributes asks the backend to write a reply to the PTY. It
        // must never turn into an effect for the user's terminal.
        term.feed(b"\x1b[c");
        assert!(term.drain_effects().is_empty());

        // Graphics are deliberately representable only as unavailable, never
        // as an opaque DCS/OSC payload that a renderer could forward.
        let unavailable = OuterTerminalEffect::Graphics(GraphicsEffect::Unavailable);
        assert!(matches!(
            unavailable,
            OuterTerminalEffect::Graphics(GraphicsEffect::Unavailable)
        ));
        let complete = OuterTerminalEffect::Progress(ProgressState::Value(100));
        assert!(matches!(
            complete,
            OuterTerminalEffect::Progress(ProgressState::Value(100))
        ));
    }

    #[test]
    fn a_full_effect_queue_suppresses_without_blocking_the_emulator() {
        let mut term = emulator(20, 3);
        for title in 0..=EFFECT_QUEUE_CAPACITY {
            term.feed(format!("\x1b]2;task {title}\x07").as_bytes());
        }

        let effects = term.drain_effects();
        assert_eq!(effects.len(), EFFECT_QUEUE_CAPACITY);
        assert_eq!(
            effects.first(),
            Some(&OuterTerminalEffect::SetTitle("task 0".into()))
        );
        assert_eq!(
            effects.last(),
            Some(&OuterTerminalEffect::SetTitle(format!(
                "task {}",
                EFFECT_QUEUE_CAPACITY - 1
            )))
        );
    }
}
