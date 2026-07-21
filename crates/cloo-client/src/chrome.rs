//! Pane chrome: headers, the focus treatment, and dimming.
//!
//! Chrome is rendered entirely client-side. The server sends contents and
//! geometry; what a pane header says, which colour a focused border wears, and
//! whether a neighbour is dimmed are decided here, which is why theming never
//! touches session state.
//!
//! Everything in this module is a pure function from a description into
//! [`Cell`]s. Nothing writes to a descriptor and nothing emits an escape
//! sequence — [`crate::renderer`] remains the only place bytes are produced —
//! so a header is testable against an exact expected row.
//!
//! Three rules from `docs/STYLEGUIDE.md` are load-bearing here:
//!
//! - **Colour is never the only signal.** Every attention state carries a glyph
//!   and, whenever width allows, its text label. A monochrome terminal loses
//!   nothing but emphasis.
//! - **Focus is not an attention state.** Focus changes the marker and the
//!   accent; it never changes the state glyph. A focused quiet pane and an
//!   unfocused pane needing input are distinct in both axes at once.
//! - **Dimming is a contrast reduction toward the frame background, not
//!   alpha,** and it must be switchable off for accessibility. A dimmed pane
//!   keeps readable text and keeps the *hue* of its state colour.
//!
//! ```
//! use cloo_client::chrome::{Attention, ChromeOptions, PaneChrome, header_cells};
//!
//! let pane = PaneChrome::new(1, "claude").attention(Attention::NeedsInput);
//! let row = header_cells(&pane, 24, ChromeOptions::default());
//! let text: String = row.iter().map(|c| c.ch).collect();
//! assert_eq!(text, "  1 claude ! needs input");
//! ```

use cloo_proto::{Cell, CellAttrs, Color, Point};

use crate::renderer::Span;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// The space between panes.
pub const FRAME: Color = Color::Rgb(0x0f, 0x0f, 0x16);
/// The chrome and pane base surface.
pub const SURFACE: Color = Color::Rgb(0x1a, 0x1b, 0x26);
/// The border of an unfocused pane.
pub const BORDER: Color = Color::Rgb(0x2a, 0x2e, 0x42);
/// Focus, selection, and active controls.
pub const ACCENT: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
/// Labels and important text.
pub const PRIMARY: Color = Color::Rgb(0xc0, 0xca, 0xf5);
/// Secondary text.
pub const MUTED: Color = Color::Rgb(0x56, 0x5f, 0x89);
/// Success and ready state.
pub const SUCCESS: Color = Color::Rgb(0x9e, 0xce, 0x6a);
/// Caution and pending state.
pub const WARNING: Color = Color::Rgb(0xe0, 0xaf, 0x68);
/// Failure and bell state.
pub const ERROR: Color = Color::Rgb(0xf7, 0x76, 0x8e);
/// Paths and informational state.
pub const INFO: Color = Color::Rgb(0x7d, 0xcf, 0xff);

/// How far a dimmed cell is pulled toward the frame background.
///
/// Chosen to read as clearly recessed while leaving text legible; the style
/// guide requires both. Applied as an exact blend rather than as an alpha
/// composite, because a terminal cell has no alpha.
const DIM_BLEND: u16 = 45;

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

/// A pane's workspace state, as the chrome presents it.
///
/// Never inferred from a pane's output: harness state is explicit, set by an
/// opt-in adapter or by the user. [`Unknown`](Self::Unknown) is the honest
/// answer when nothing has reported, and is distinct from
/// [`Quiet`](Self::Quiet), which means an adapter said there is nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Attention {
    /// No reliable activity signal.
    #[default]
    Unknown,
    /// Set by an opt-in adapter or the user.
    Working,
    /// Requires a decision or response.
    NeedsInput,
    /// Completed with an unread result.
    Ready,
    /// The child exited unsuccessfully, or an adapter reported failure.
    Failed,
    /// No active attention condition.
    Quiet,
}

impl Attention {
    /// The state's glyph. Deliberately ASCII: it is the last thing standing in
    /// a narrow pane, so it may never depend on a font.
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Unknown => '?',
            Self::Working => '*',
            Self::NeedsInput => '!',
            Self::Ready => '+',
            Self::Failed => 'x',
            Self::Quiet => '-',
        }
    }

    /// The state's text label, shown whenever the width allows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Working => "working",
            Self::NeedsInput => "needs input",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Quiet => "quiet",
        }
    }

    /// The semantic colour supplementing the glyph and label.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Unknown | Self::Quiet => MUTED,
            Self::Working => INFO,
            Self::NeedsInput => WARNING,
            Self::Ready => SUCCESS,
            Self::Failed => ERROR,
        }
    }
}

// ---------------------------------------------------------------------------
// Options and description
// ---------------------------------------------------------------------------

/// The accessibility choices that change how chrome is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeOptions {
    /// Whether unfocused panes are dimmed. The style guide requires a
    /// configuration that turns this off; with it off, focus is carried by the
    /// accent border and marker alone.
    pub dim_unfocused: bool,
}

impl Default for ChromeOptions {
    fn default() -> Self {
        Self {
            dim_unfocused: true,
        }
    }
}

impl ChromeOptions {
    /// The no-dim accessibility fallback.
    #[must_use]
    pub const fn no_dim() -> Self {
        Self {
            dim_unfocused: false,
        }
    }
}

/// Everything the chrome needs to know about one pane.
///
/// Client-side view state, assembled from the layout snapshot and whatever
/// pane metadata the session reports. It is never authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneChrome {
    /// The pane's position in the tab, as the user refers to it.
    pub index: u16,
    /// The pane's name — a profile, a command, or a user-chosen label.
    pub title: String,
    /// An optional task label. The first thing to go when width is scarce.
    pub task: Option<String>,
    /// The pane's workspace state.
    pub attention: Attention,
    /// Whether this pane has focus.
    pub focused: bool,
    /// Whether this pane is zoomed to fill its tab.
    pub zoomed: bool,
}

impl PaneChrome {
    /// Builds an unfocused, unzoomed pane header description.
    #[must_use]
    pub fn new(index: u16, title: impl Into<String>) -> Self {
        Self {
            index,
            title: title.into(),
            ..Self::default()
        }
    }

    /// Sets the task label.
    #[must_use]
    pub fn task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Sets the workspace state.
    #[must_use]
    pub const fn attention(mut self, attention: Attention) -> Self {
        self.attention = attention;
        self
    }

    /// Marks the pane focused.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Marks the pane zoomed.
    #[must_use]
    pub const fn zoomed(mut self, zoomed: bool) -> Self {
        self.zoomed = zoomed;
        self
    }
}

// ---------------------------------------------------------------------------
// Dimming
// ---------------------------------------------------------------------------

/// Reduces one colour's contrast toward the frame background.
///
/// Only a 24-bit colour can be blended exactly. A palette index or the
/// terminal's own default is left alone here and dimmed by the `DIM` attribute
/// instead — guessing at what index 4 looks like in the user's palette would
/// produce a worse answer than the terminal's own faint rendition.
fn toward_frame(color: Color) -> Option<Color> {
    let Color::Rgb(r, g, b) = color else {
        return None;
    };
    let Color::Rgb(fr, fg, fb) = FRAME else {
        return None;
    };
    let blend = |value: u8, frame: u8| -> u8 {
        let mixed = (u16::from(value) * (100 - DIM_BLEND) + u16::from(frame) * DIM_BLEND) / 100;
        u8::try_from(mixed).unwrap_or(value)
    };
    Some(Color::Rgb(blend(r, fr), blend(g, fg), blend(b, fb)))
}

/// Dims one cell.
///
/// A 24-bit foreground keeps its hue and loses contrast, which is what lets a
/// dimmed pane that needs input stay recognisably amber. Anything else falls
/// back to `DIM`, the terminal's own faint rendition.
#[must_use]
pub fn dim_cell(cell: Cell) -> Cell {
    let mut dimmed = cell;
    match toward_frame(cell.fg) {
        Some(fg) => dimmed.fg = fg,
        None => dimmed.attrs = dimmed.attrs.union(CellAttrs::DIM),
    }
    if let Some(bg) = toward_frame(cell.bg) {
        dimmed.bg = bg;
    }
    dimmed
}

/// Dims a whole row of an unfocused pane's body.
///
/// A no-op when `options` disables dimming or the pane is focused, so callers
/// can apply it unconditionally and let the policy live in one place.
#[must_use]
pub fn dim_cells(cells: &[Cell], focused: bool, options: ChromeOptions) -> Vec<Cell> {
    if focused || !options.dim_unfocused {
        return cells.to_vec();
    }
    cells.iter().copied().map(dim_cell).collect()
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// One styled run of header text, before it becomes cells.
struct Segment<'a> {
    text: &'a str,
    fg: Color,
    attrs: CellAttrs,
}

/// Builds the header row for one pane, exactly `width` cells wide.
///
/// The row is the pane's top border as well as its label: its foreground is the
/// accent when the pane is focused and the neutral border colour otherwise, so
/// focus is visible without reading a word.
///
/// Width is spent in a fixed order of preference. The marker, the pane index,
/// the zoom indicator, the title, and the state glyph are what a header is; the
/// task label goes first when space runs out, then the state's text label, and
/// only then is the title truncated. At a width too small even for that, the
/// glyph is the last thing standing.
#[must_use]
pub fn header_cells(chrome: &PaneChrome, width: u16, options: ChromeOptions) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let marker = if chrome.focused { "> " } else { "  " };
    let zoom = if chrome.zoomed { "Z " } else { "" };
    let index = format!("{} ", chrome.index);
    let state_full = format!("{} {}", chrome.attention.glyph(), chrome.attention.label());
    let state_compact = chrome.attention.glyph().to_string();
    let task = chrome
        .task
        .as_deref()
        .filter(|task| !task.is_empty())
        .map(|task| format!(" - {task}"))
        .unwrap_or_default();

    let prefix_len = len(marker) + len(zoom) + len(&index);
    let title_len = len(&chrome.title);

    // One space is the minimum gap between the label and the state.
    let fits = |left: usize, right: usize| left + 1 + right <= width;
    let (title_budget, keep_task, state) =
        if fits(prefix_len + title_len + len(&task), len(&state_full)) {
            (title_len, true, state_full.as_str())
        } else if fits(prefix_len + title_len, len(&state_full)) {
            (title_len, false, state_full.as_str())
        } else if fits(prefix_len + title_len, len(&state_compact)) {
            (title_len, false, state_compact.as_str())
        } else {
            // Truncate the title into whatever is left beside the glyph. A width
            // that cannot hold even one title character drops to the glyph alone,
            // below.
            let budget = width
                .saturating_sub(prefix_len + len(&state_compact) + 1)
                .min(title_len);
            (budget, false, state_compact.as_str())
        };

    let title = truncate(&chrome.title, title_budget);
    let title_fg = if chrome.focused { ACCENT } else { PRIMARY };
    let title_attrs = if chrome.focused {
        CellAttrs::BOLD
    } else {
        CellAttrs::NONE
    };

    let mut segments = Vec::with_capacity(6);
    if title_budget > 0 {
        segments.push(Segment {
            text: marker,
            fg: if chrome.focused { ACCENT } else { BORDER },
            attrs: CellAttrs::NONE,
        });
        if !zoom.is_empty() {
            segments.push(Segment {
                text: zoom,
                fg: WARNING,
                attrs: CellAttrs::BOLD,
            });
        }
        segments.push(Segment {
            text: &index,
            fg: MUTED,
            attrs: CellAttrs::NONE,
        });
        segments.push(Segment {
            text: title,
            fg: title_fg,
            attrs: title_attrs,
        });
        if keep_task {
            segments.push(Segment {
                text: &task,
                fg: MUTED,
                attrs: CellAttrs::NONE,
            });
        }
    }

    let used: usize = segments.iter().map(|s| len(s.text)).sum();
    let state = truncate(state, width.saturating_sub(used));
    let gap = width - used - len(state);

    let mut cells = Vec::with_capacity(width);
    for segment in &segments {
        push_str(&mut cells, segment.text, segment.fg, segment.attrs);
    }
    for _ in 0..gap {
        push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    }
    push_str(&mut cells, state, chrome.attention.color(), CellAttrs::NONE);

    if !chrome.focused && options.dim_unfocused {
        for cell in &mut cells {
            *cell = dim_cell(*cell);
        }
    }
    cells
}

/// Builds a header as a positioned span, ready for
/// [`Renderer::render_spans`](crate::renderer::Renderer::render_spans).
///
/// `at` is the header's own row, in outer-terminal coordinates — the pane's
/// body starts on the row below it.
#[must_use]
pub fn header_span(at: Point, chrome: &PaneChrome, width: u16, options: ChromeOptions) -> Span {
    Span::new(at, header_cells(chrome, width, options))
}

/// Appends `text` as styled cells over the chrome surface.
fn push_str(cells: &mut Vec<Cell>, text: &str, fg: Color, attrs: CellAttrs) {
    for ch in text.chars() {
        cells.push(Cell {
            ch,
            fg,
            bg: SURFACE,
            attrs,
        });
    }
}

/// How many cells a string occupies.
fn len(text: &str) -> usize {
    text.chars().count()
}

/// The longest prefix of `text` that fits in `budget` cells.
fn truncate(text: &str, budget: usize) -> &str {
    match text.char_indices().nth(budget) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header's text, with styling discarded.
    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    /// The foreground of the cell holding `ch`.
    fn fg_of(cells: &[Cell], ch: char) -> Color {
        cells
            .iter()
            .find(|cell| cell.ch == ch)
            .map(|cell| cell.fg)
            .expect("the glyph is present")
    }

    fn wide() -> u16 {
        40
    }

    // -- Attention --------------------------------------------------------

    #[test]
    fn every_state_has_a_distinct_ascii_glyph_and_a_label() {
        let all = [
            Attention::Unknown,
            Attention::Working,
            Attention::NeedsInput,
            Attention::Ready,
            Attention::Failed,
            Attention::Quiet,
        ];
        let mut glyphs: Vec<char> = all.iter().map(|a| a.glyph()).collect();
        glyphs.sort_unstable();
        let unique = glyphs.len();
        glyphs.dedup();
        assert_eq!(glyphs.len(), unique, "glyphs must distinguish every state");
        for state in all {
            assert!(state.glyph().is_ascii(), "{state:?} needs an ASCII glyph");
            assert!(!state.label().is_empty(), "{state:?} needs a label");
        }
    }

    #[test]
    fn state_text_and_glyph_carry_the_state_without_color() {
        // A monochrome terminal must still tell these apart.
        for state in [Attention::NeedsInput, Attention::Failed, Attention::Ready] {
            let pane = PaneChrome::new(1, "sh").attention(state);
            let row = header_cells(&pane, wide(), ChromeOptions::default());
            let text = text_of(&row);
            assert!(
                text.contains(state.glyph()) && text.contains(state.label()),
                "{state:?} rendered as {text:?}"
            );
        }
    }

    // -- Focus versus attention -------------------------------------------

    #[test]
    fn focus_and_attention_are_independent_signals() {
        let options = ChromeOptions::default();
        let focused_quiet = header_cells(
            &PaneChrome::new(1, "sh")
                .attention(Attention::Quiet)
                .focused(true),
            wide(),
            options,
        );
        let unfocused_needs_input = header_cells(
            &PaneChrome::new(1, "sh").attention(Attention::NeedsInput),
            wide(),
            options,
        );
        assert_ne!(focused_quiet, unfocused_needs_input);
        assert!(text_of(&focused_quiet).starts_with('>'), "focus marker");
        assert!(
            !text_of(&unfocused_needs_input).starts_with('>'),
            "an unfocused pane must not wear the focus marker"
        );
        assert!(text_of(&unfocused_needs_input).contains('!'));
        assert!(text_of(&focused_quiet).contains('-'));
    }

    #[test]
    fn focus_changes_the_accent_and_never_the_state_glyph() {
        let unfocused = header_cells(
            &PaneChrome::new(2, "claude").attention(Attention::Working),
            wide(),
            ChromeOptions::no_dim(),
        );
        let focused = header_cells(
            &PaneChrome::new(2, "claude")
                .attention(Attention::Working)
                .focused(true),
            wide(),
            ChromeOptions::no_dim(),
        );
        assert_eq!(
            fg_of(&focused, 'c'),
            ACCENT,
            "the focused title is accented"
        );
        assert_eq!(fg_of(&unfocused, 'c'), PRIMARY);
        assert_eq!(
            fg_of(&focused, '*'),
            fg_of(&unfocused, '*'),
            "focus must not restyle the attention glyph"
        );
    }

    #[test]
    fn a_dimmed_pane_keeps_its_state_apart_from_a_quiet_one() {
        let options = ChromeOptions::default();
        let needs_input = header_cells(
            &PaneChrome::new(1, "sh").attention(Attention::NeedsInput),
            wide(),
            options,
        );
        let quiet = header_cells(
            &PaneChrome::new(1, "sh").attention(Attention::Quiet),
            wide(),
            options,
        );
        assert_ne!(
            fg_of(&needs_input, '!'),
            fg_of(&quiet, '-'),
            "dimming must reduce contrast, never erase the semantic colour"
        );
    }

    // -- Geometry and truncation ------------------------------------------

    #[test]
    fn a_header_is_exactly_the_pane_width_at_every_size() {
        let pane = PaneChrome::new(12, "claude-code")
            .task("refactor the layout pass")
            .attention(Attention::NeedsInput)
            .focused(true)
            .zoomed(true);
        for width in 0_u16..=60 {
            let row = header_cells(&pane, width, ChromeOptions::default());
            assert_eq!(
                row.len(),
                usize::from(width),
                "width {width} produced {} cells",
                row.len()
            );
        }
    }

    #[test]
    fn a_wide_header_shows_index_title_task_and_state() {
        let pane = PaneChrome::new(3, "codex")
            .task("tests")
            .attention(Attention::Working);
        let row = header_cells(&pane, 30, ChromeOptions::default());
        assert_eq!(text_of(&row), "  3 codex - tests    * working");
    }

    #[test]
    fn the_task_label_is_the_first_thing_to_go() {
        let pane = PaneChrome::new(3, "codex")
            .task("tests")
            .attention(Attention::Working);
        let row = header_cells(&pane, 22, ChromeOptions::default());
        let text = text_of(&row);
        assert!(!text.contains("tests"), "got {text:?}");
        assert!(
            text.contains("codex") && text.contains("working"),
            "got {text:?}"
        );
    }

    #[test]
    fn the_state_label_goes_before_the_title_is_truncated() {
        let pane = PaneChrome::new(3, "codex")
            .task("tests")
            .attention(Attention::Working);
        let row = header_cells(&pane, 12, ChromeOptions::default());
        let text = text_of(&row);
        assert_eq!(text, "  3 codex  *");
    }

    #[test]
    fn a_narrow_pane_truncates_the_title_but_keeps_the_glyph() {
        let pane = PaneChrome::new(3, "codex").attention(Attention::Failed);
        let row = header_cells(&pane, 8, ChromeOptions::default());
        assert_eq!(text_of(&row), "  3 co x");
    }

    #[test]
    fn the_glyph_is_the_last_thing_standing() {
        let pane = PaneChrome::new(3, "codex").attention(Attention::Failed);
        assert_eq!(
            text_of(&header_cells(&pane, 1, ChromeOptions::default())),
            "x"
        );
        assert!(header_cells(&pane, 0, ChromeOptions::default()).is_empty());
    }

    #[test]
    fn a_zoomed_pane_says_so_in_its_header() {
        let pane = PaneChrome::new(1, "sh")
            .attention(Attention::Quiet)
            .focused(true)
            .zoomed(true);
        let row = header_cells(&pane, 20, ChromeOptions::no_dim());
        let text = text_of(&row);
        assert!(text.starts_with("> Z 1 sh"), "got {text:?}");
        assert_eq!(fg_of(&row, 'Z'), WARNING);
    }

    // -- Dimming ----------------------------------------------------------

    #[test]
    fn the_no_dim_fallback_leaves_an_unfocused_header_at_full_contrast() {
        let pane = PaneChrome::new(1, "sh").attention(Attention::Ready);
        let dimmed = header_cells(&pane, wide(), ChromeOptions::default());
        let plain = header_cells(&pane, wide(), ChromeOptions::no_dim());
        assert_ne!(dimmed, plain, "dimming must actually change the row");
        assert_eq!(
            fg_of(&plain, '+'),
            SUCCESS,
            "undimmed keeps the token exactly"
        );
        assert_eq!(text_of(&dimmed), text_of(&plain), "only colour changes");
    }

    #[test]
    fn a_focused_header_is_never_dimmed() {
        let pane = PaneChrome::new(1, "sh")
            .attention(Attention::Ready)
            .focused(true);
        assert_eq!(
            header_cells(&pane, wide(), ChromeOptions::default()),
            header_cells(&pane, wide(), ChromeOptions::no_dim()),
        );
    }

    #[test]
    fn dimming_a_body_row_is_policy_in_one_place() {
        let cells = vec![Cell {
            ch: 'a',
            fg: PRIMARY,
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        }];
        let options = ChromeOptions::default();
        assert_eq!(
            dim_cells(&cells, true, options),
            cells,
            "a focused pane is untouched"
        );
        assert_eq!(
            dim_cells(&cells, false, ChromeOptions::no_dim()),
            cells,
            "the no-dim fallback is untouched"
        );
        assert_ne!(dim_cells(&cells, false, options), cells);
    }

    #[test]
    fn a_palette_color_dims_with_the_attribute_rather_than_a_guess() {
        let indexed = Cell {
            ch: 'a',
            fg: Color::Indexed(4),
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        };
        let dimmed = dim_cell(indexed);
        assert_eq!(
            dimmed.fg,
            Color::Indexed(4),
            "the palette entry is the user's"
        );
        assert!(dimmed.attrs.contains(CellAttrs::DIM));
    }

    #[test]
    fn a_true_color_cell_dims_by_blending_and_stays_legible() {
        let cell = Cell {
            ch: 'a',
            fg: PRIMARY,
            bg: SURFACE,
            attrs: CellAttrs::NONE,
        };
        let dimmed = dim_cell(cell);
        assert!(
            !dimmed.attrs.contains(CellAttrs::DIM),
            "the blend is the reduction; stacking DIM on it would double-dim"
        );
        let (Color::Rgb(r, _, _), Color::Rgb(dr, _, _)) = (cell.fg, dimmed.fg) else {
            panic!("both are 24-bit");
        };
        assert!(dr < r, "contrast is reduced");
        assert!(dr > 0x50, "text stays readable: got {dr:#04x}");
        assert_ne!(dimmed.bg, cell.bg, "the surface recedes too");
    }

    // -- Spans ------------------------------------------------------------

    #[test]
    fn a_header_span_sits_where_the_pane_starts() {
        let pane = PaneChrome::new(1, "sh");
        let span = header_span(Point::new(10, 4), &pane, 12, ChromeOptions::default());
        assert_eq!(span.at, Point::new(10, 4));
        assert_eq!(span.cells.len(), 12);
    }
}
