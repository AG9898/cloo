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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use cloo_core::StatusMode;
use cloo_proto::{Cell, CellAttrs, Color, Direction, Point, Size, TabSummary};

use crate::motion::{Motion, MotionKind, MotionSettings, Phase};
use crate::renderer::Span;
use crate::status::RepositoryStatus;
use crate::theme::{Theme, ThemeToken};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// The space between panes.
pub const FRAME: Color = Color::Rgb(0x0f, 0x0f, 0x16);
/// The chrome and pane base surface.
pub const SURFACE: Color = Color::Rgb(0x1a, 0x1b, 0x26);
/// The raised surface behind active tabs and overlays.
pub const RAISED_SURFACE: Color = Color::Rgb(0x24, 0x28, 0x3b);
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
        self.color_in(Theme::storm())
    }

    /// The semantic colour supplementing this state in one client theme.
    #[must_use]
    pub const fn color_in(self, theme: Theme) -> Color {
        match self {
            Self::Unknown | Self::Quiet => theme.color(ThemeToken::Muted),
            Self::Working => theme.color(ThemeToken::Info),
            Self::NeedsInput => theme.color(ThemeToken::Warning),
            Self::Ready => theme.color(ThemeToken::Success),
            Self::Failed => theme.color(ThemeToken::Error),
        }
    }

    /// Whether this state is something a human is being asked to act on, and so
    /// belongs in the attention queue.
    ///
    /// Only `needs_input`, `ready`, and `failed` qualify. Progress
    /// ([`Working`](Self::Working)) and the absence of news
    /// ([`Unknown`](Self::Unknown), [`Quiet`](Self::Quiet)) are not events a
    /// person has to navigate to, so they never enter the queue or raise a
    /// toast.
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        matches!(self, Self::NeedsInput | Self::Ready | Self::Failed)
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
    /// The client-local theme for this chrome pass.
    pub theme: Theme,
}

impl Default for ChromeOptions {
    fn default() -> Self {
        Self {
            dim_unfocused: true,
            theme: Theme::storm(),
        }
    }
}

impl ChromeOptions {
    /// The no-dim accessibility fallback.
    #[must_use]
    pub const fn no_dim() -> Self {
        Self {
            dim_unfocused: false,
            theme: Theme::storm(),
        }
    }

    /// Applies one client-local theme while preserving the chosen dimming mode.
    #[must_use]
    pub const fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
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
// Tab row
// ---------------------------------------------------------------------------

/// The one character a session badge reduces to before it disappears.
///
/// The same marker the narrowest status row keeps for the session field, so the
/// two rows never disagree about which glyph means "session".
const SESSION_GLYPH: char = 's';

/// Everything the top row describes about one workspace.
///
/// The tabs are the layout's own ordering. `session` and `clients` come from the
/// daemon's [`WorkspaceStatus`](cloo_proto::WorkspaceStatus) projection and
/// `panes` from the layout the client is already drawing, so every field on the
/// row has a named source. Each optional field is `None` when nothing
/// authoritative has been received; the row then omits it rather than showing a
/// placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabBar<'a> {
    tabs: &'a [TabSummary],
    session: Option<&'a str>,
    clients: Option<u16>,
    panes: Option<usize>,
}

impl<'a> TabBar<'a> {
    /// A bar over one tab ordering, with no workspace metadata yet.
    #[must_use]
    pub const fn new(tabs: &'a [TabSummary]) -> Self {
        Self {
            tabs,
            session: None,
            clients: None,
            panes: None,
        }
    }

    /// Names the session the badge draws — the daemon's display name, verbatim.
    #[must_use]
    pub const fn session(mut self, name: &'a str) -> Self {
        self.session = Some(name);
        self
    }

    /// Sets the attached-client count the right-side metadata may show.
    #[must_use]
    pub const fn clients(mut self, clients: u16) -> Self {
        self.clients = Some(clients);
        self
    }

    /// Sets the visible pane count the right-side metadata may show.
    #[must_use]
    pub const fn panes(mut self, panes: usize) -> Self {
        self.panes = Some(panes);
        self
    }

    /// The tab ordering this bar was built over.
    #[must_use]
    pub const fn tabs(&self) -> &'a [TabSummary] {
        self.tabs
    }

    /// The badge forms, widest first, always ending in "no badge at all".
    ///
    /// An absent or empty name yields only the empty form: a session whose name
    /// the client has not been told is not given an invented one.
    fn badge_forms(self) -> Vec<Vec<Cell>> {
        let Some(name) = self.session.filter(|name| !name.is_empty()) else {
            return vec![Vec::new()];
        };
        vec![
            badge_cells(name),
            badge_cells(&SESSION_GLYPH.to_string()),
            Vec::new(),
        ]
    }

    /// The right-side metadata forms, widest first, always ending in nothing.
    fn metadata_forms(self) -> Vec<Vec<Cell>> {
        let mut full = Vec::new();
        let mut short = Vec::new();
        if let Some(panes) = self.panes {
            push_str(&mut full, &plural(panes, "pane"), MUTED, CellAttrs::NONE);
            push_str(&mut short, &format!("{panes}p"), MUTED, CellAttrs::NONE);
        }
        if let Some(clients) = self.clients {
            if !full.is_empty() {
                push_str(&mut full, "  ", MUTED, CellAttrs::NONE);
                push_str(&mut short, " ", MUTED, CellAttrs::NONE);
            }
            push_str(
                &mut full,
                &plural(usize::from(clients), "client"),
                MUTED,
                CellAttrs::NONE,
            );
            push_str(&mut short, &format!("{clients}c"), MUTED, CellAttrs::NONE);
        }
        if full.is_empty() {
            vec![Vec::new()]
        } else {
            vec![full, short, Vec::new()]
        }
    }
}

/// Builds the session-aware top tab row, exactly `width` cells wide.
///
/// The row is a session badge, the ordered tab chips, and right-aligned
/// workspace metadata. Each tab is a one-based bar position and title, never a
/// stable id. The active chip is raised, accented, bold, and underlined — the
/// one-row terminal reading of the handoff's lower-edge treatment — and keeps
/// its `>` marker, so a 16-colour or monochrome terminal still has an
/// unambiguous answer.
///
/// Width is spent in one fixed order, which is what makes a narrowing terminal
/// deterministic:
///
/// 1. right-side metadata drops to its compact form and then disappears;
/// 2. inactive tabs yield from the far right and then the far left, keeping a
///    contiguous window around the active tab;
/// 3. the session badge reduces to its glyph and then disappears;
/// 4. the active chip's title truncates, and below that the marker and index are
///    what remain.
#[must_use]
pub fn tab_row_cells(bar: TabBar<'_>, width: u16, options: ChromeOptions) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let tabs = bar.tabs;
    let active = tabs.iter().position(|tab| tab.active).unwrap_or(0);
    let badges = bar.badge_forms();
    let widest_badge = badges.first().map_or(&[][..], Vec::as_slice);
    let nothing: &[Cell] = &[];

    // 1. Metadata is the cheapest thing on the row, so it goes first while every
    //    tab and the full badge are still at their widest.
    for metadata in bar.metadata_forms() {
        if let Some(cells) = fit_tab_row(widest_badge, tabs, 0, tabs.len(), &metadata, width) {
            return options.theme.map_storm_cells(cells);
        }
    }

    // 2. Then inactive tabs yield around the active one.
    let mut window = (0usize, tabs.len());
    loop {
        if let Some(cells) = fit_tab_row(widest_badge, tabs, window.0, window.1, nothing, width) {
            return options.theme.map_storm_cells(cells);
        }
        match narrower_window(window, active) {
            Some(next) => window = next,
            None => break,
        }
    }

    // 3. Only then does the badge reduce, and then disappear.
    for badge in badges.iter().skip(1) {
        if let Some(cells) = fit_tab_row(badge, tabs, window.0, window.1, nothing, width) {
            return options.theme.map_storm_cells(cells);
        }
    }

    // 4. The active chip alone, with its title truncated into whatever is left.
    let mut cells = Vec::with_capacity(width);
    if let Some(tab) = tabs.get(active) {
        let budget = width.saturating_sub(len(&chip_prefix(tab, active + 1)));
        cells.extend(chip_cells(tab, active + 1, Some(budget)));
        cells.truncate(width);
    }
    pad_status_row(&mut cells, width);
    options.theme.map_storm_cells(cells)
}

/// Positions a tab row for [`Renderer::render_spans`](crate::renderer::Renderer::render_spans).
#[must_use]
pub fn tab_row_span(at: Point, bar: TabBar<'_>, width: u16, options: ChromeOptions) -> Span {
    Span::new(at, tab_row_cells(bar, width, options))
}

/// One candidate row, or `None` when that combination does not fit `width`.
fn fit_tab_row(
    badge: &[Cell],
    tabs: &[TabSummary],
    first: usize,
    last: usize,
    metadata: &[Cell],
    width: usize,
) -> Option<Vec<Cell>> {
    let mut cells = badge.to_vec();
    for (offset, tab) in tabs.get(first..last)?.iter().enumerate() {
        if offset > 0 {
            push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
        }
        cells.extend(chip_cells(tab, first + offset + 1, None));
    }
    // One space keeps the metadata from touching the last chip.
    let gap = usize::from(!metadata.is_empty());
    if cells.len() + gap + metadata.len() > width {
        return None;
    }
    while cells.len() + metadata.len() < width {
        push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    }
    cells.extend_from_slice(metadata);
    Some(cells)
}

/// The next narrower contiguous window around `active`, or `None` at one tab.
///
/// Inactive tabs are given up from the far right first, then the far left, so a
/// narrowing bar never reorders itself or hides the tab the user is on.
fn narrower_window(window: (usize, usize), active: usize) -> Option<(usize, usize)> {
    let (first, last) = window;
    if last.saturating_sub(first) <= 1 {
        return None;
    }
    if last - 1 != active {
        Some((first, last - 1))
    } else if first != active {
        Some((first + 1, last))
    } else {
        None
    }
}

/// One tab chip's leading marker and one-based bar position.
fn chip_prefix(tab: &TabSummary, index: usize) -> String {
    let marker = if tab.active { '>' } else { ' ' };
    format!("{marker}{index} ")
}

/// One tab chip, optionally with its title truncated into `budget` cells.
fn chip_cells(tab: &TabSummary, index: usize, budget: Option<usize>) -> Vec<Cell> {
    let prefix = chip_prefix(tab, index);
    let title = budget.map_or(tab.title.as_str(), |budget| truncate(&tab.title, budget));
    let (fg, bg, attrs) = if tab.active {
        (
            ACCENT,
            RAISED_SURFACE,
            CellAttrs::BOLD.union(CellAttrs::UNDERLINE),
        )
    } else {
        (MUTED, SURFACE, CellAttrs::NONE)
    };
    let mut cells = Vec::with_capacity(len(&prefix) + len(title));
    push_styled(&mut cells, &prefix, fg, bg, attrs);
    push_styled(&mut cells, title, fg, bg, attrs);
    cells
}

/// The session badge: `text` on the accent, padded on both sides.
fn badge_cells(text: &str) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(len(text) + 2);
    push_styled(&mut cells, " ", SURFACE, ACCENT, CellAttrs::BOLD);
    push_styled(&mut cells, text, SURFACE, ACCENT, CellAttrs::BOLD);
    push_styled(&mut cells, " ", SURFACE, ACCENT, CellAttrs::BOLD);
    cells
}

/// `1 pane` / `2 panes`, so a count never reads as a broken template.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
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
fn toward_frame(color: Color, frame: Color) -> Option<Color> {
    let Color::Rgb(r, g, b) = color else {
        return None;
    };
    let Color::Rgb(fr, fg, fb) = frame else {
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
    dim_cell_with_theme(cell, Theme::storm())
}

/// Dims one cell toward the frame colour of `theme`.
///
/// A terminal-palette-inheriting theme has no RGB frame to blend toward, so it
/// deliberately takes the terminal's `DIM` attribute path instead of guessing
/// what the user's default background looks like.
#[must_use]
pub fn dim_cell_with_theme(cell: Cell, theme: Theme) -> Cell {
    let mut dimmed = cell;
    match toward_frame(cell.fg, theme.color(ThemeToken::Frame)) {
        Some(fg) => dimmed.fg = fg,
        None => dimmed.attrs = dimmed.attrs.union(CellAttrs::DIM),
    }
    if let Some(bg) = toward_frame(cell.bg, theme.color(ThemeToken::Frame)) {
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
    cells
        .iter()
        .copied()
        .map(|cell| dim_cell_with_theme(cell, options.theme))
        .collect()
}

/// Positions one row of a pane's body as a span, dimmed when the pane is
/// unfocused.
///
/// A pane's contents are the server's, arriving as whole grid rows; this is the
/// one place they are turned into a positioned run so a multi-pane frame can
/// drop each pane's grid into its own rect. Child default colours are resolved
/// on this copied row before dimming, so the cache stays application-owned and
/// the dimming policy sees the pane's actual rendered colours. Dimming stays
/// the single-place [`dim_cells`] policy rather than being re-decided here.
#[must_use]
pub fn body_span(at: Point, cells: &[Cell], focused: bool, options: ChromeOptions) -> Span {
    let themed = cells
        .iter()
        .copied()
        .map(|cell| options.theme.map_child_cell(cell))
        .collect::<Vec<_>>();
    Span::new(at, dim_cells(&themed, focused, options))
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

    // Chrome is authored against the reference Storm tokens above. Translate
    // those roles before applying dimming, so a non-Storm frame is also the
    // colour an unfocused pane recedes toward.
    let mut cells = options.theme.map_storm_cells(cells);

    if !chrome.focused && options.dim_unfocused {
        for cell in &mut cells {
            *cell = dim_cell_with_theme(*cell, options.theme);
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

/// Builds the pane's complete top frame edge.
///
/// The header occupies the edge between the two corners, so this adds no
/// second title row. The returned width is `body_width + 2`.
#[must_use]
pub fn top_frame_cells(chrome: &PaneChrome, body_width: u16, options: ChromeOptions) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(usize::from(body_width).saturating_add(2));
    cells.push(frame_cell('┌', chrome.focused, options));
    cells.extend(header_cells(chrome, body_width, options));
    cells.push(frame_cell('┐', chrome.focused, options));
    cells
}

/// Builds one vertical pane-frame edge cell.
#[must_use]
pub fn side_frame_cell(focused: bool, options: ChromeOptions) -> Cell {
    frame_cell('│', focused, options)
}

/// Builds the pane's complete bottom frame edge.
#[must_use]
pub fn bottom_frame_cells(focused: bool, body_width: u16, options: ChromeOptions) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(usize::from(body_width).saturating_add(2));
    cells.push(frame_cell('└', focused, options));
    cells.extend(std::iter::repeat_n(
        frame_cell('─', focused, options),
        usize::from(body_width),
    ));
    cells.push(frame_cell('┘', focused, options));
    cells
}

/// One cell of a pane's visible side or corner treatment.
fn frame_cell(ch: char, focused: bool, options: ChromeOptions) -> Cell {
    let mut cell = Cell {
        ch,
        fg: if focused { ACCENT } else { BORDER },
        bg: FRAME,
        attrs: CellAttrs::NONE,
    };
    cell = options.theme.map_storm_cell(cell);
    if !focused && options.dim_unfocused {
        cell = dim_cell_with_theme(cell, options.theme);
    }
    cell
}

// ---------------------------------------------------------------------------
// Active resize
// ---------------------------------------------------------------------------

/// Draws a lit divider and its resulting visible ratio.
///
/// `points` come from the same `ScreenLayout::divider_points` geometry used by
/// mouse hit-testing. Each point is painted independently because nested layouts
/// can split a divider into disjoint segments. The final span is the compact
/// right-aligned status label from card 08.
#[must_use]
pub fn resize_affordance_spans(
    points: &[Point],
    dir: Direction,
    ratio: f32,
    outer: Size,
    theme: Theme,
) -> Vec<Span> {
    if points.is_empty() || outer.cols == 0 || outer.rows == 0 {
        return Vec::new();
    }
    let glyph = match dir {
        Direction::Horizontal => '│',
        Direction::Vertical => '─',
    };
    let divider = Cell {
        ch: glyph,
        fg: theme.color(ThemeToken::Accent),
        bg: theme.color(ThemeToken::Frame),
        attrs: CellAttrs::BOLD,
    };
    let mut spans = points
        .iter()
        .copied()
        .map(|point| Span::new(point, vec![divider]))
        .collect::<Vec<_>>();

    let label = format!("resize · ratio {:.2}", ratio.clamp(0.0, 1.0));
    let width = usize::from(outer.cols);
    let shown = if label.chars().count() > width {
        label
            .chars()
            .skip(label.chars().count() - width)
            .collect::<String>()
    } else {
        label
    };
    let mut cells = Vec::with_capacity(shown.chars().count());
    let ratio_at = shown.find("0.").unwrap_or(shown.len());
    for (byte, ch) in shown.char_indices() {
        cells.push(Cell {
            ch,
            fg: if byte >= ratio_at {
                theme.color(ThemeToken::Accent)
            } else {
                theme.color(ThemeToken::Muted)
            },
            bg: theme.color(ThemeToken::Surface),
            attrs: if byte >= ratio_at {
                CellAttrs::BOLD
            } else {
                CellAttrs::NONE
            },
        });
    }
    let label_width = u16::try_from(cells.len()).unwrap_or(outer.cols);
    spans.push(Span::new(
        Point::new(
            outer.cols.saturating_sub(label_width),
            outer.rows.saturating_sub(1),
        ),
        cells,
    ));
    spans
}

/// Appends `text` as styled cells over the chrome surface.
fn push_str(cells: &mut Vec<Cell>, text: &str, fg: Color, attrs: CellAttrs) {
    push_styled(cells, text, fg, SURFACE, attrs);
}

/// Appends `text` as styled cells over an explicit background.
///
/// Only chrome that is deliberately raised off the base surface — an active tab
/// chip, the session badge — needs this; everything else uses [`push_str`] so a
/// chrome row has one background by default.
fn push_styled(cells: &mut Vec<Cell>, text: &str, fg: Color, bg: Color, attrs: CellAttrs) {
    for ch in text.chars() {
        cells.push(Cell { ch, fg, bg, attrs });
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

// ---------------------------------------------------------------------------
// Attention summary, queue, and toasts
// ---------------------------------------------------------------------------

/// The actionable states, most urgent first.
///
/// The status-bar summary and the queue both walk this fixed order, which is
/// what makes their layout deterministic rather than dependent on the order
/// events happened to arrive.
const ACTIONABLE: [Attention; 3] = [Attention::NeedsInput, Attention::Failed, Attention::Ready];

/// One pane's place in the attention queue.
///
/// Assembled from the pane's identity and its reported attention; never
/// authoritative and never inferred from the grid. A pane appears at most once,
/// carrying only its newest unacknowledged actionable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// The pane's index in the tab, as the user refers to it and as a focus
    /// action targets it.
    pub index: u16,
    /// The pane's name, for the queue row.
    pub title: String,
    /// The state that put the pane in the queue.
    pub attention: Attention,
}

/// The attention queue: the newest unacknowledged actionable event per pane.
///
/// A navigation surface, not a notification log, so its behaviour is defined by
/// three rules that keep it from becoming a firehose:
///
/// - **One entry per pane.** A pane is listed once; a fresh event for a pane
///   already present updates that entry in place rather than adding a second.
/// - **Newest first, deterministically.** A new or changed event moves its pane
///   to the front. A plain repeat of the same live state coalesces and leaves
///   the order untouched, so a harness re-announcing `needs_input` cannot churn
///   the list.
/// - **An acknowledged state does not come back.** Acknowledging a pane records
///   the state the user dismissed; re-reporting that same state is ignored,
///   exactly as [`cloo_core::pane::Attention::set`] clears acknowledgment only
///   when the state actually changes. A pane that returns to a non-actionable
///   state clears that memory, so its next real event alerts again.
#[derive(Debug, Clone, Default)]
pub struct AttentionQueue {
    /// Entries, front = most recent.
    entries: Vec<QueueEntry>,
    /// Per pane, the state the user last acknowledged.
    acked: HashMap<u16, Attention>,
    /// The keyboard cursor into `entries`.
    selected: usize,
}

impl AttentionQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a pane's current attention, applying the coalescing rules.
    pub fn record(&mut self, index: u16, title: impl Into<String>, attention: Attention) {
        if !attention.is_actionable() {
            // The pane is no longer asking for anything: drop it, and forget the
            // acknowledgment so its next real event is heard fresh.
            self.remove_pane(index);
            self.acked.remove(&index);
            return;
        }
        if self.acked.get(&index) == Some(&attention) {
            // The user dismissed exactly this; a re-report must not refill it.
            return;
        }
        // A state distinct from any acknowledged one is live again.
        self.acked.remove(&index);
        match self.position(index) {
            Some(pos) if self.entries[pos].attention == attention => {
                // A plain repeat of the same live state: coalesce, keep order.
            }
            Some(pos) => {
                let mut entry = self.entries.remove(pos);
                entry.attention = attention;
                entry.title = title.into();
                self.entries.insert(0, entry);
            }
            None => {
                self.entries.insert(
                    0,
                    QueueEntry {
                        index,
                        title: title.into(),
                        attention,
                    },
                );
            }
        }
        self.clamp_selection();
    }

    /// Acknowledges a pane, removing it and remembering what was dismissed.
    ///
    /// Returns the pane index when an entry was present, so a caller can pair
    /// acknowledgment with any follow-up it wants.
    pub fn acknowledge(&mut self, index: u16) -> Option<u16> {
        let pos = self.position(index)?;
        let entry = self.entries.remove(pos);
        self.acked.insert(index, entry.attention);
        self.clamp_selection();
        Some(index)
    }

    /// Acknowledges the currently selected entry.
    pub fn acknowledge_selected(&mut self) -> Option<u16> {
        let index = self.entries.get(self.selected)?.index;
        self.acknowledge(index)
    }

    /// Moves the keyboard cursor one entry toward the older end.
    pub fn select_next(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Moves the keyboard cursor one entry toward the newer end.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// The currently selected entry, if any.
    #[must_use]
    pub fn selected(&self) -> Option<&QueueEntry> {
        self.entries.get(self.selected)
    }

    /// The keyboard cursor's position.
    #[must_use]
    pub fn selection(&self) -> usize {
        self.selected
    }

    /// The pane a focus action would jump to: the selected entry's pane.
    #[must_use]
    pub fn focus_target(&self) -> Option<u16> {
        self.entries.get(self.selected).map(|entry| entry.index)
    }

    /// The entries, newest first.
    #[must_use]
    pub fn entries(&self) -> &[QueueEntry] {
        &self.entries
    }

    /// How many panes are waiting on the user. This is the status bar's count.
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries sit in each actionable state, in urgency order.
    #[must_use]
    pub fn tally(&self) -> [(Attention, usize); 3] {
        ACTIONABLE.map(|state| {
            let count = self
                .entries
                .iter()
                .filter(|entry| entry.attention == state)
                .count();
            (state, count)
        })
    }

    fn position(&self, index: u16) -> Option<usize> {
        self.entries.iter().position(|entry| entry.index == index)
    }

    fn remove_pane(&mut self, index: u16) {
        if let Some(pos) = self.position(index) {
            self.entries.remove(pos);
            self.clamp_selection();
        }
    }

    fn clamp_selection(&mut self) {
        let last = self.entries.len().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
        }
    }
}

/// A compact attention tally for the always-on status bar.
///
/// Renders `<count><glyph>` for each actionable state that has any waiting
/// panes, in the fixed [`ACTIONABLE`] order and coloured by state, so the count
/// is never carried by colour alone. An empty queue renders nothing.
#[must_use]
pub fn summary_cells(queue: &AttentionQueue) -> Vec<Cell> {
    let mut cells = Vec::new();
    for (state, count) in queue.tally() {
        if count == 0 {
            continue;
        }
        if !cells.is_empty() {
            push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
        }
        push_str(
            &mut cells,
            &count.to_string(),
            state.color(),
            CellAttrs::BOLD,
        );
        push_str(
            &mut cells,
            &state.glyph().to_string(),
            state.color(),
            CellAttrs::NONE,
        );
    }
    cells
}

/// The summary as a positioned span.
#[must_use]
pub fn summary_span(at: Point, queue: &AttentionQueue) -> Span {
    Span::new(at, summary_cells(queue))
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

/// The settled hint for cloo's default prefix,
/// [`cloo_core::keymap::DEFAULT_PREFIX`].
///
/// The default spelling, kept as a constant because it is what an unconfigured
/// client shows and what the style guide documents. The row itself renders
/// whatever [`PrefixHint`] it is handed, so a rebound prefix appears verbatim.
pub const DEFAULT_PREFIX_HINT: &str = "C-b";

/// The clue keys the first-attach guide offers, in the order they are spent.
///
/// Each is `(word, key)`, and the row yields them *from the end* — help first,
/// then stack, then split — which is the same "trailing detail goes first"
/// ladder the rest of the status row follows.
const PREFIX_CLUES: [(&str, char); 3] = [("split", '%'), ("stack", '"'), ("help", '?')];

/// The status row's prefix field: what the prefix is called, whether one is
/// pending, and whether the first-attach clues are being offered.
///
/// The prefix is a chrome concern, not session state: the keymap is the
/// client's, so two clients attached to one session may legitimately show
/// different hints. The spelling arrives already rendered — `Key::to_string`
/// on [`cloo_core::keymap::Keymap::prefix`] — because `cloo-client` must show
/// the chord a user actually configured, never a hard-coded `C-b`.
///
/// Two flags widen the field beyond that spelling:
///
/// - **Guided.** While the workspace still has one pane, the row spends its
///   trailing width explaining how to get a second one. More panes means the
///   user has already done it, so the ordinary session, tab, and attention
///   fields win that space back.
/// - **Pending.** A prefix that has been pressed and is waiting for the next
///   chord is bracketed as well as accented, so the state is legible without
///   colour, and it turns the clues on regardless of pane count — the moment
///   the next key matters is exactly the moment to say what it can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixHint {
    prefix: String,
    guide: bool,
    pending: bool,
}

impl PrefixHint {
    /// A settled, unguided hint over a prefix spelling.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            guide: false,
            pending: false,
        }
    }

    /// The hint a workspace with `panes` visible panes shows.
    ///
    /// One pane — the shape a freshly created default workspace has — is what
    /// turns the first-attach clues on.
    #[must_use]
    pub fn for_panes(prefix: impl Into<String>, panes: usize) -> Self {
        Self::new(prefix).guided(panes <= 1)
    }

    /// Offers, or withdraws, the first-attach clues.
    #[must_use]
    pub fn guided(mut self, guide: bool) -> Self {
        self.guide = guide;
        self
    }

    /// Marks the prefix as pressed and awaiting its next chord.
    #[must_use]
    pub fn pending(mut self, pending: bool) -> Self {
        self.pending = pending;
        self
    }

    /// The configured chord's spelling, exactly as it will be drawn.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Whether a prefix has been pressed and cloo owns the next chord.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Whether the first-attach clues are being offered.
    #[must_use]
    pub const fn is_guided(&self) -> bool {
        self.guide || self.pending
    }

    /// The prefix chord alone, bracketed and accented while pending.
    fn prefix_cells(&self) -> Vec<Cell> {
        if self.pending {
            text_cells(&format!("[{}]", self.prefix), ACCENT, CellAttrs::BOLD)
        } else {
            text_cells(&self.prefix, PRIMARY, CellAttrs::NONE)
        }
    }

    /// The guide forms, widest first, or nothing when the clues are withheld.
    fn guide_forms(&self) -> Vec<Vec<Cell>> {
        if !self.is_guided() {
            return Vec::new();
        }
        (0..PREFIX_CLUES.len())
            .rev()
            .map(|last| {
                let mut cells = self.prefix_cells();
                for (word, key) in &PREFIX_CLUES[..=last] {
                    push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
                    push_str(&mut cells, word, MUTED, CellAttrs::NONE);
                    push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
                    push_str(&mut cells, &key.to_string(), ACCENT, CellAttrs::BOLD);
                }
                cells
            })
            .collect()
    }

    /// The one character that survives on a four-cell row.
    ///
    /// Derived from the configured spelling rather than fixed at `b`, so a
    /// rebound prefix stays honest even where nothing else fits.
    fn mark(&self) -> char {
        self.prefix.chars().next_back().unwrap_or('b')
    }
}

impl Default for PrefixHint {
    /// cloo's default prefix, settled and unguided — exactly
    /// [`DEFAULT_PREFIX_HINT`].
    fn default() -> Self {
        Self::new(cloo_core::keymap::DEFAULT_PREFIX.to_string())
    }
}

/// The truthful values available to either status-row composition.
///
/// Server projections (`session`, `tabs`, `clients`, and `queue`) stay distinct
/// from client-local values (`repository`, `clock`, and `hint`) until the final
/// composition. Optional values are absent rather than represented by
/// placeholders.
#[derive(Debug, Clone, Copy)]
pub struct StatusBar<'a> {
    tabs: &'a [TabSummary],
    queue: &'a AttentionQueue,
    hint: &'a PrefixHint,
    session: Option<&'a str>,
    clients: Option<u16>,
    effective_size: Option<Size>,
    repository: Option<&'a RepositoryStatus>,
    clock: Option<&'a str>,
    mode: StatusMode,
    powerline_separators: bool,
}

impl<'a> StatusBar<'a> {
    /// Starts a bar with its required tab, attention, and prefix projections.
    #[must_use]
    pub const fn new(
        tabs: &'a [TabSummary],
        queue: &'a AttentionQueue,
        hint: &'a PrefixHint,
    ) -> Self {
        Self {
            tabs,
            queue,
            hint,
            session: None,
            clients: None,
            effective_size: None,
            repository: None,
            clock: None,
            mode: StatusMode::Minimal,
            powerline_separators: false,
        }
    }

    /// Supplies the daemon-projected logical session name.
    #[must_use]
    pub const fn session(mut self, session: &'a str) -> Self {
        self.session = Some(session);
        self
    }

    /// Supplies the daemon-projected attached-client count.
    #[must_use]
    pub const fn clients(mut self, clients: u16) -> Self {
        self.clients = Some(clients);
        self
    }

    /// Supplies the daemon-projected effective minimum terminal size.
    #[must_use]
    pub const fn effective_size(mut self, size: Size) -> Self {
        self.effective_size = Some(size);
        self
    }

    /// Supplies the focused pane's bounded client-local repository answer.
    #[must_use]
    pub const fn repository(mut self, repository: &'a RepositoryStatus) -> Self {
        self.repository = Some(repository);
        self
    }

    /// Supplies the client-local wall-clock text.
    #[must_use]
    pub const fn clock(mut self, clock: &'a str) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Selects the client-local status composition.
    ///
    /// Powerline is an explicit opt-in, so selecting it also opts into its font
    /// separator. A caller that knows the glyph is unavailable can retain the
    /// composition and request flat boundaries with
    /// [`Self::powerline_separators`].
    #[must_use]
    pub const fn mode(mut self, mode: StatusMode) -> Self {
        self.mode = mode;
        self.powerline_separators = matches!(mode, StatusMode::Powerline);
        self
    }

    /// Enables or disables the optional powerline separator glyph.
    ///
    /// Disabling it changes only the boundary cells. Fields, their order, and
    /// their semantic backgrounds remain the same.
    #[must_use]
    pub const fn powerline_separators(mut self, supported: bool) -> Self {
        self.powerline_separators = supported;
        self
    }
}

/// Builds the configured always-on status composition, exactly `width` cells
/// wide.
///
/// The reference form is a flat sequence of styled segments: logical session,
/// ordered tabs, attention, then right-aligned repository/client detail,
/// prefix, and clock. Width first spends first-attach guidance, then removes
/// clock, client and repository detail, inactive tabs, and tab/session titles.
/// In minimal mode, required session, active-tab, attention, and prefix markers
/// remain as the compact ASCII `s>!b` form at the physical limit. Powerline's
/// corresponding rules are documented by [`StatusBar::mode`].
#[must_use]
pub fn status_bar_cells(bar: StatusBar<'_>, width: u16, options: ChromeOptions) -> Vec<Cell> {
    match bar.mode {
        StatusMode::Minimal => minimal_status_bar_cells(bar, width, options),
        StatusMode::Powerline => powerline_status_bar_cells(bar, width, options),
    }
}

fn minimal_status_bar_cells(bar: StatusBar<'_>, width: u16, options: ChromeOptions) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let session_full = bar.session.filter(|name| !name.is_empty()).map_or_else(
        || status_segment("s", PRIMARY, SURFACE, CellAttrs::BOLD),
        |name| status_segment(&format!("s {name}"), SURFACE, ACCENT, CellAttrs::BOLD),
    );
    let session_mark = text_cells("s", PRIMARY, CellAttrs::BOLD);

    let active = bar.tabs.iter().position(|tab| tab.active).unwrap_or(0);
    let tab_short = status_tab_cells(bar.tabs.get(active), active + 1, false);
    let tab_mark = text_cells(">", ACCENT, CellAttrs::BOLD);

    let attention_full = status_segment_cells(status_attention_cells(bar.queue));
    let attention_count = text_cells(
        &format!("{}!", bar.queue.count()),
        if bar.queue.is_empty() { MUTED } else { WARNING },
        CellAttrs::BOLD,
    );
    let attention_mark = text_cells("!", WARNING, CellAttrs::BOLD);

    let prefix_full = status_segment_cells(bar.hint.prefix_cells());
    let prefix_short = bar.hint.prefix_cells();
    let repository = bar.repository.map(repository_cells);
    let clients = bar.clients.map(client_cells);
    let clock = bar.clock.filter(|clock| !clock.is_empty()).map(clock_cells);

    let mut window = (0usize, bar.tabs.len());
    let widest_tabs = status_tabs_cells(bar.tabs, window, true);

    // Guidance owns the optional right side while it is useful, and yields from
    // the end before any core field is shortened.
    for guide in bar.hint.guide_forms() {
        let guide = status_segment_cells(guide);
        let left = [
            session_full.as_slice(),
            widest_tabs.as_slice(),
            attention_full.as_slice(),
        ];
        if let Some(cells) = fit_status_row(&left, &[guide.as_slice()], width) {
            return options.theme.map_storm_cells(cells);
        }
    }

    let optional_forms = [
        (repository.as_deref(), clients.as_deref(), clock.as_deref()),
        (repository.as_deref(), clients.as_deref(), None),
        (repository.as_deref(), None, None),
        (None, None, None),
    ];
    for (repository, clients, clock) in optional_forms {
        let left = [
            session_full.as_slice(),
            widest_tabs.as_slice(),
            attention_full.as_slice(),
        ];
        let right = [
            repository.unwrap_or_default(),
            clients.unwrap_or_default(),
            prefix_full.as_slice(),
            clock.unwrap_or_default(),
        ];
        if let Some(cells) = fit_status_row(&left, &right, width) {
            return options.theme.map_storm_cells(cells);
        }
    }

    // Inactive tabs yield around the active one only after optional local and
    // client detail has gone.
    loop {
        let tabs = status_tabs_cells(bar.tabs, window, true);
        let left = [
            session_full.as_slice(),
            tabs.as_slice(),
            attention_full.as_slice(),
        ];
        if let Some(cells) = fit_status_row(&left, &[prefix_full.as_slice()], width) {
            return options.theme.map_storm_cells(cells);
        }
        match narrower_window(window, active) {
            Some(next) => window = next,
            None => break,
        }
    }

    for (session, tab, attention, prefix) in [
        (&session_full, &tab_short, &attention_full, &prefix_full),
        (&session_mark, &tab_short, &attention_full, &prefix_full),
        (&session_mark, &tab_short, &attention_count, &prefix_full),
        (&session_mark, &tab_short, &attention_count, &prefix_short),
        (&session_mark, &tab_mark, &attention_mark, &prefix_short),
    ] {
        let left = [session.as_slice(), tab.as_slice(), attention.as_slice()];
        if let Some(cells) = fit_status_row(&left, &[prefix.as_slice()], width) {
            return options.theme.map_storm_cells(cells);
        }
    }

    // Four ASCII markers retain every required field down to four cells. The
    // final marker is the last character of the configured prefix's spelling.
    let mut cells = Vec::with_capacity(width);
    cells.extend_from_slice(&session_mark);
    cells.extend_from_slice(&tab_mark);
    cells.extend_from_slice(&attention_mark);
    push_str(
        &mut cells,
        &bar.hint.mark().to_string(),
        PRIMARY,
        CellAttrs::NONE,
    );
    cells.truncate(width);
    pad_status_row(&mut cells, width);
    options.theme.map_storm_cells(cells)
}

/// Builds the opt-in powerline composition from the same [`StatusBar`] values.
///
/// The wide form is mode, logical session, active tab, repository (or attention
/// when no repository answer exists), client/effective-size detail, and clock.
/// It spends width in that reverse order before reducing the data-bearing left
/// fields. At the physical limit one ASCII marker per left field remains.
fn powerline_status_bar_cells(bar: StatusBar<'_>, width: u16, options: ChromeOptions) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let mode_full = status_segment("NORMAL", SURFACE, ACCENT, CellAttrs::BOLD);
    let mode_mark = powerline_segment(text_cells("N", SURFACE, CellAttrs::BOLD), ACCENT);
    let session_full = bar.session.filter(|name| !name.is_empty()).map_or_else(
        || status_segment("s", PRIMARY, BORDER, CellAttrs::BOLD),
        |name| status_segment(&format!("s {name}"), PRIMARY, BORDER, CellAttrs::BOLD),
    );
    let session_mark = powerline_segment(text_cells("s", PRIMARY, CellAttrs::BOLD), BORDER);

    let active = bar.tabs.iter().position(|tab| tab.active).unwrap_or(0);
    let tab_full = powerline_segment(
        status_tab_cells(bar.tabs.get(active), active + 1, true),
        RAISED_SURFACE,
    );
    let tab_short = powerline_segment(
        status_tab_cells(bar.tabs.get(active), active + 1, false),
        RAISED_SURFACE,
    );
    let tab_mark = powerline_segment(text_cells(">", ACCENT, CellAttrs::BOLD), RAISED_SURFACE);

    let (detail_full, detail_short, detail_mark) = if let Some(repository) = bar.repository {
        let mut short = Vec::new();
        push_str(&mut short, "git", SUCCESS, CellAttrs::BOLD);
        if repository.changes > 0 {
            push_str(
                &mut short,
                &format!(" +{}", repository.changes),
                WARNING,
                CellAttrs::NONE,
            );
        }
        (
            powerline_segment(repository_cells(repository), SURFACE),
            powerline_segment(short, SURFACE),
            powerline_segment(text_cells("g", SUCCESS, CellAttrs::BOLD), SURFACE),
        )
    } else {
        (
            powerline_segment(
                status_segment_cells(status_attention_cells(bar.queue)),
                SURFACE,
            ),
            powerline_segment(
                text_cells(
                    &format!("{}!", bar.queue.count()),
                    if bar.queue.is_empty() { MUTED } else { WARNING },
                    CellAttrs::BOLD,
                ),
                SURFACE,
            ),
            powerline_segment(text_cells("!", WARNING, CellAttrs::BOLD), SURFACE),
        )
    };

    let client_full = powerline_client_cells(bar.clients, bar.effective_size, false);
    let client_short = powerline_client_cells(bar.clients, bar.effective_size, true);
    let clock = bar
        .clock
        .filter(|clock| !clock.is_empty())
        .map(|clock| status_segment(clock, SURFACE, ACCENT, CellAttrs::BOLD));

    let forms = [
        (
            &mode_full,
            &session_full,
            &tab_full,
            &detail_full,
            client_full.as_deref(),
            clock.as_deref(),
        ),
        (
            &mode_full,
            &session_full,
            &tab_full,
            &detail_full,
            client_full.as_deref(),
            None,
        ),
        (
            &mode_full,
            &session_full,
            &tab_full,
            &detail_full,
            client_short.as_deref(),
            None,
        ),
        (
            &mode_full,
            &session_full,
            &tab_full,
            &detail_full,
            None,
            None,
        ),
        (
            &mode_full,
            &session_full,
            &tab_full,
            &detail_short,
            None,
            None,
        ),
        (
            &mode_full,
            &session_full,
            &tab_short,
            &detail_short,
            None,
            None,
        ),
        (
            &mode_full,
            &session_mark,
            &tab_short,
            &detail_short,
            None,
            None,
        ),
        (
            &mode_full,
            &session_mark,
            &tab_mark,
            &detail_short,
            None,
            None,
        ),
        (
            &mode_mark,
            &session_mark,
            &tab_mark,
            &detail_mark,
            None,
            None,
        ),
    ];
    for (mode, session, tab, detail, clients, clock) in forms {
        let left = [
            mode.as_slice(),
            session.as_slice(),
            tab.as_slice(),
            detail.as_slice(),
        ];
        let right = [clients.unwrap_or_default(), clock.unwrap_or_default()];
        if let Some(cells) = fit_powerline_row(&left, &right, width, bar.powerline_separators) {
            return options.theme.map_storm_cells(cells);
        }
    }

    let mut cells = Vec::with_capacity(width);
    push_str(&mut cells, "N", ACCENT, CellAttrs::BOLD);
    push_str(&mut cells, "s", PRIMARY, CellAttrs::BOLD);
    push_str(&mut cells, ">", ACCENT, CellAttrs::BOLD);
    if bar.repository.is_some() {
        push_str(&mut cells, "g", SUCCESS, CellAttrs::BOLD);
    } else {
        push_str(&mut cells, "!", WARNING, CellAttrs::BOLD);
    }
    cells.truncate(width);
    pad_status_row(&mut cells, width);
    options.theme.map_storm_cells(cells)
}

fn powerline_client_cells(
    clients: Option<u16>,
    effective_size: Option<Size>,
    compact: bool,
) -> Option<Vec<Cell>> {
    if clients.is_none() && effective_size.is_none() {
        return None;
    }
    let mut text = String::new();
    if let Some(clients) = clients {
        if compact {
            text.push_str(&format!("{clients}c"));
        } else {
            text.push_str(&plural(usize::from(clients), "client"));
        }
    }
    if let Some(size) = effective_size {
        if !text.is_empty() {
            text.push_str(if compact { " " } else { " · " });
        }
        if !compact {
            text.push_str("min ");
        }
        text.push_str(&format!("{}x{}", size.cols, size.rows));
    }
    Some(status_segment(
        &text,
        MUTED,
        RAISED_SURFACE,
        CellAttrs::NONE,
    ))
}

/// Applies one powerline segment background while preserving semantic text.
fn powerline_segment(mut cells: Vec<Cell>, background: Color) -> Vec<Cell> {
    if cells.is_empty() {
        return cells;
    }
    for cell in &mut cells {
        cell.bg = background;
    }
    if cells.first().is_some_and(|cell| cell.ch != ' ') {
        cells.insert(
            0,
            Cell {
                ch: ' ',
                fg: Color::Default,
                bg: background,
                attrs: CellAttrs::NONE,
            },
        );
    }
    if cells.last().is_some_and(|cell| cell.ch != ' ') {
        cells.push(Cell {
            ch: ' ',
            fg: Color::Default,
            bg: background,
            attrs: CellAttrs::NONE,
        });
    }
    cells
}

/// Joins powerline fields and right-aligns the optional group.
fn fit_powerline_row(
    left: &[&[Cell]],
    right: &[&[Cell]],
    width: usize,
    separators: bool,
) -> Option<Vec<Cell>> {
    let left = left
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let right = right
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let group_len = |parts: &[&[Cell]]| {
        parts.iter().map(|part| part.len()).sum::<usize>()
            + usize::from(separators) * parts.len().saturating_sub(1)
    };
    let left_len = group_len(&left);
    let right_len = group_len(&right);
    if left_len + right_len > width {
        return None;
    }

    let mut cells = Vec::with_capacity(width);
    push_powerline_group(&mut cells, &left, separators);
    while cells.len() + right_len < width {
        push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    }
    push_powerline_group(&mut cells, &right, separators);
    Some(cells)
}

fn push_powerline_group(cells: &mut Vec<Cell>, parts: &[&[Cell]], separators: bool) {
    for (index, part) in parts.iter().enumerate() {
        if separators && index > 0 {
            let previous = cells.last().map_or(SURFACE, |cell| cell.bg);
            let next = part.first().map_or(SURFACE, |cell| cell.bg);
            cells.push(Cell {
                ch: '\u{e0b0}',
                fg: previous,
                bg: next,
                attrs: CellAttrs::NONE,
            });
        }
        cells.extend_from_slice(part);
    }
}

/// Positions the always-on status row for the chrome renderer.
#[must_use]
pub fn status_bar_span(at: Point, bar: StatusBar<'_>, width: u16, options: ChromeOptions) -> Span {
    Span::new(at, status_bar_cells(bar, width, options))
}

fn status_segment(text: &str, fg: Color, bg: Color, attrs: CellAttrs) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(len(text) + 2);
    push_styled(&mut cells, " ", fg, bg, attrs);
    push_styled(&mut cells, text, fg, bg, attrs);
    push_styled(&mut cells, " ", fg, bg, attrs);
    cells
}

fn status_segment_cells(mut cells: Vec<Cell>) -> Vec<Cell> {
    cells.insert(
        0,
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: SURFACE,
            attrs: CellAttrs::NONE,
        },
    );
    cells.push(Cell {
        ch: ' ',
        fg: Color::Default,
        bg: SURFACE,
        attrs: CellAttrs::NONE,
    });
    cells
}

fn status_tab_cells(tab: Option<&TabSummary>, index: usize, title: bool) -> Vec<Cell> {
    let Some(tab) = tab else {
        return text_cells(">", ACCENT, CellAttrs::BOLD);
    };
    let marker = if tab.active { '>' } else { ' ' };
    let bg = if tab.active { RAISED_SURFACE } else { SURFACE };
    let attrs = if tab.active {
        CellAttrs::BOLD.union(CellAttrs::UNDERLINE)
    } else {
        CellAttrs::NONE
    };
    let mut cells = Vec::new();
    push_styled(&mut cells, " ", MUTED, bg, attrs);
    push_styled(
        &mut cells,
        &marker.to_string(),
        if tab.active { ACCENT } else { MUTED },
        bg,
        attrs,
    );
    push_styled(
        &mut cells,
        &index.to_string(),
        if tab.active { INFO } else { MUTED },
        bg,
        attrs,
    );
    if title && !tab.title.is_empty() {
        push_styled(&mut cells, " ", MUTED, bg, attrs);
        push_styled(
            &mut cells,
            &tab.title,
            if tab.active { PRIMARY } else { MUTED },
            bg,
            attrs,
        );
    }
    push_styled(&mut cells, " ", MUTED, bg, attrs);
    cells
}

fn status_tabs_cells(tabs: &[TabSummary], window: (usize, usize), titles: bool) -> Vec<Cell> {
    let mut cells = Vec::new();
    for (index, tab) in tabs.iter().enumerate().take(window.1).skip(window.0) {
        cells.extend(status_tab_cells(Some(tab), index + 1, titles));
    }
    cells
}

fn repository_cells(repository: &RepositoryStatus) -> Vec<Cell> {
    let mut cells = Vec::new();
    push_str(&mut cells, "git", SUCCESS, CellAttrs::BOLD);
    if let Some(branch) = repository
        .branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
    {
        push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
        push_str(&mut cells, branch, PRIMARY, CellAttrs::NONE);
    }
    if repository.changes > 0 {
        push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
        push_str(
            &mut cells,
            &format!("+{}", repository.changes),
            WARNING,
            CellAttrs::NONE,
        );
    }
    status_segment_cells(cells)
}

fn client_cells(clients: u16) -> Vec<Cell> {
    status_segment(
        &plural(usize::from(clients), "client"),
        MUTED,
        SURFACE,
        CellAttrs::NONE,
    )
}

fn clock_cells(clock: &str) -> Vec<Cell> {
    status_segment(clock, PRIMARY, SURFACE, CellAttrs::BOLD)
}

/// Turns text into cells for one flat status-bar field.
fn text_cells(text: &str, fg: Color, attrs: CellAttrs) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(len(text));
    push_str(&mut cells, text, fg, attrs);
    cells
}

/// The detailed attention field for a status row.
///
/// `summary_cells` intentionally answers with nothing when no pane needs
/// attention. The always-on row still needs to say that its count is zero, so
/// it supplies that one explicit, text-and-glyph fallback.
fn status_attention_cells(queue: &AttentionQueue) -> Vec<Cell> {
    let summary = summary_cells(queue);
    if summary.is_empty() {
        text_cells("0!", MUTED, CellAttrs::BOLD)
    } else {
        summary
    }
}

/// Joins fitted left and right fields into one padded status row.
fn fit_status_row(left: &[&[Cell]], right: &[&[Cell]], width: usize) -> Option<Vec<Cell>> {
    let left_len = left.iter().map(|part| part.len()).sum::<usize>();
    let right_len = right.iter().map(|part| part.len()).sum::<usize>();
    if left_len + right_len > width {
        return None;
    }
    let mut cells = Vec::with_capacity(width);
    for part in left {
        cells.extend_from_slice(part);
    }
    while cells.len() + right_len < width {
        push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    }
    for part in right {
        cells.extend_from_slice(part);
    }
    Some(cells)
}

/// Pads a status row with chrome-surface cells.
fn pad_status_row(cells: &mut Vec<Cell>, width: usize) {
    while cells.len() < width {
        push_str(cells, " ", Color::Default, CellAttrs::NONE);
    }
}

/// One row of the attention queue overlay, exactly `width` cells wide.
///
/// A queue row is the pane header's layout applied to an entry: the same fixed
/// width-degradation order, the same glyph-is-last rule, and the same accent
/// treatment for the row the keyboard cursor is on — `selected` maps to a
/// header's focus. Dimming is off, because an overlay row is never a background
/// pane. Reusing [`header_cells`] is what keeps a queue row and a pane header
/// visually identical and keeps the exact-width guarantee in one place.
#[must_use]
pub fn queue_row_cells(entry: &QueueEntry, selected: bool, width: u16) -> Vec<Cell> {
    let chrome = PaneChrome::new(entry.index, entry.title.clone())
        .attention(entry.attention)
        .focused(selected);
    header_cells(&chrome, width, ChromeOptions::no_dim())
}

/// One queue row as a positioned span.
#[must_use]
pub fn queue_row_span(at: Point, entry: &QueueEntry, selected: bool, width: u16) -> Span {
    Span::new(at, queue_row_cells(entry, selected, width))
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

/// How many toasts the live stack shows at once.
///
/// Three is the style guide's "bounded" made concrete: enough that two panes
/// finishing together are both seen, few enough that the stack cannot walk down
/// a pane the user is working in.
pub const TOAST_CAPACITY: usize = 3;

/// How long a toast stays up before it dismisses itself.
///
/// The same linger the status row's transient notice takes, and for the same
/// reason: long enough to read, and never covering a harness the user is typing
/// into indefinitely.
pub const TOAST_LIFETIME: Duration = Duration::from_secs(4);

/// The columns the stack keeps clear of the frame's right edge.
pub const TOAST_MARGIN: u16 = 1;

/// The most width one toast takes, however wide the terminal is.
///
/// A toast is a concise notice, not a panel: past this it would start reading as
/// a column of the workspace rather than as something passing through.
pub const TOAST_MAX_WIDTH: u16 = 36;

/// A transient notice that a pane raised an actionable event.
///
/// Carries its own clock: when it stops showing, and the frame-budgeted entrance
/// it is still part-way through. Both are driven by
/// [`ToastDeck::tick`](ToastDeck::tick) from the client's render clock — never
/// by a pane's output — so a busy child can raise no frames here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// The pane the notice is about.
    pub index: u16,
    /// The pane's name.
    pub title: String,
    /// The state that raised it.
    pub attention: Attention,
    /// How many times this pane's event has coalesced into this notice.
    pub repeats: u32,
    /// When this notice stops showing.
    until: Instant,
    /// The entrance, in flight until it settles.
    motion: Motion,
    /// The step of that entrance the deck last handed out, or `None` once it has
    /// settled into the chrome's own colours.
    phase: Option<Phase>,
}

impl Toast {
    /// A first notice for a pane, raised at `now` with the default lifetime and
    /// an animated entrance.
    ///
    /// [`ToastDeck`] applies its own lifetime and motion settings when it raises
    /// one, so this is the shape a caller rendering a toast on its own needs.
    #[must_use]
    pub fn new(index: u16, title: impl Into<String>, attention: Attention, now: Instant) -> Self {
        let mut toast = Self {
            index,
            title: title.into(),
            attention,
            repeats: 1,
            until: now,
            motion: Motion::new(MotionSettings::animated()),
            phase: None,
        };
        toast.enter(now, TOAST_LIFETIME, MotionSettings::animated());
        toast
    }

    /// The step of the entrance to draw, or `None` for the settled cells.
    #[must_use]
    pub const fn phase(&self) -> Option<Phase> {
        self.phase
    }

    /// When this notice stops showing.
    #[must_use]
    pub const fn expires_at(&self) -> Instant {
        self.until
    }

    /// Restarts the entrance and the lifetime, which is what raising or
    /// refreshing a notice means.
    ///
    /// The entrance is [`MotionKind::Overlay`]: a toast is a client-owned
    /// surface appearing, quantized into the render loop's frame budget like
    /// every other transition. Under reduce-motion it settles on the frame it
    /// started, so the deck asks for no extra frames at all.
    fn enter(&mut self, now: Instant, lifetime: Duration, settings: MotionSettings) {
        self.until = now + lifetime;
        self.motion = Motion::new(settings);
        let phase = self.motion.start(MotionKind::Overlay, now);
        self.phase = (!phase.is_settled()).then_some(phase);
    }

    /// Advances the entrance, reporting whether the cells it draws changed.
    fn tick(&mut self, now: Instant) -> bool {
        let Some(next) = self.motion.tick(now) else {
            return false;
        };
        self.phase = (!next.is_settled()).then_some(next);
        true
    }
}

/// A bounded, coalescing stack of toasts.
///
/// Three rules from the style guide are the whole point: the stack is *bounded*,
/// so a burst can never grow it without limit; repeated events from the same
/// pane *coalesce* into one notice with a repeat count rather than stacking
/// copies; and a notice dismisses itself, so it can never sit indefinitely over
/// a harness the user is typing into. When a new pane's toast would exceed
/// capacity, the oldest is dropped.
///
/// Time is passed in rather than read, exactly as [`crate::motion`] does it: a
/// whole lifetime is testable frame by frame without sleeping.
#[derive(Debug, Clone)]
pub struct ToastDeck {
    /// Front = oldest.
    toasts: VecDeque<Toast>,
    capacity: usize,
    lifetime: Duration,
    settings: MotionSettings,
}

impl ToastDeck {
    /// A deck holding at most `capacity` toasts (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            toasts: VecDeque::new(),
            capacity: capacity.max(1),
            lifetime: TOAST_LIFETIME,
            settings: MotionSettings::animated(),
        }
    }

    /// Sets how long a toast raised from now on stays up.
    #[must_use]
    pub const fn lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// Sets the entrance's accessibility settings.
    #[must_use]
    pub const fn motion(mut self, settings: MotionSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Replaces the entrance's accessibility settings without losing the stack.
    ///
    /// A live client resolves these from a reloaded configuration, and a notice
    /// already showing must not disappear because the user changed a preference.
    /// Reduce-motion settles every entrance in flight on this frame.
    pub fn set_motion(&mut self, settings: MotionSettings) {
        self.settings = settings;
        if settings.reduce_motion {
            for toast in &mut self.toasts {
                toast.phase = None;
            }
        }
    }

    /// How long a toast raised now would stay up.
    #[must_use]
    pub const fn lifetime_of(&self) -> Duration {
        self.lifetime
    }

    /// Raises or coalesces a toast for a pane.
    ///
    /// A pane already showing coalesces: its state and title refresh, its repeat
    /// count grows, its lifetime and entrance restart, and it moves to the newest
    /// position. A new pane pushes onto the back, evicting the oldest toast if
    /// the deck is full.
    pub fn push(
        &mut self,
        index: u16,
        title: impl Into<String>,
        attention: Attention,
        now: Instant,
    ) {
        if let Some(pos) = self.toasts.iter().position(|toast| toast.index == index) {
            let mut toast = self.toasts.remove(pos).expect("position just found");
            toast.title = title.into();
            toast.attention = attention;
            toast.repeats = toast.repeats.saturating_add(1);
            toast.enter(now, self.lifetime, self.settings);
            self.toasts.push_back(toast);
            return;
        }
        if self.toasts.len() == self.capacity {
            self.toasts.pop_front();
        }
        let mut toast = Toast::new(index, title, attention, now);
        toast.enter(now, self.lifetime, self.settings);
        self.toasts.push_back(toast);
    }

    /// Advances every entrance and drops every expired notice.
    ///
    /// Reports whether the frame changed, which is what decides whether the
    /// client redraws. Called from the render clock only: a toast is raised by an
    /// explicit attention projection and animated by the frame budget, so a large
    /// `cat` never becomes an animation source.
    pub fn tick(&mut self, now: Instant) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|toast| now < toast.until);
        let expired = self.toasts.len() != before;
        let mut advanced = false;
        for toast in &mut self.toasts {
            advanced |= toast.tick(now);
        }
        expired || advanced
    }

    /// Removes a pane's toast, if it has one.
    pub fn dismiss(&mut self, index: u16) {
        self.toasts.retain(|toast| toast.index != index);
    }

    /// Drops notices for pane positions the workspace no longer has.
    ///
    /// A toast names the pane by the position the user refers to it by, and a
    /// closing pane renumbers its neighbours. Dropping the positions that no
    /// longer exist is what keeps a notice from outliving its pane.
    pub fn retain_within(&mut self, panes: u16) {
        self.toasts.retain(|toast| toast.index <= panes);
    }

    /// The toasts, oldest first.
    pub fn toasts(&self) -> impl Iterator<Item = &Toast> {
        self.toasts.iter()
    }

    /// How many toasts are showing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    /// Whether the deck is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    /// The most a deck will hold at once.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// A concise toast line, truncated to `width`, in the default palette.
#[must_use]
pub fn toast_cells(toast: &Toast, width: u16) -> Vec<Cell> {
    toast_cells_in(toast, width, Theme::storm())
}

/// A concise toast line, truncated to `width`, in one client theme.
///
/// Renders `<title> <glyph> <label>` with the state coloured and, when the pane
/// has coalesced more than once, a muted `(xN)` repeat count. Unlike a header it
/// is not padded to width: a toast floats over the layout rather than owning a
/// row. Every part is text and shape as well as colour, so the 16-colour and
/// terminal-palette themes say the same thing the truecolour one does.
#[must_use]
pub fn toast_cells_in(toast: &Toast, width: u16, theme: Theme) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let mut cells = Vec::new();
    push_str(
        &mut cells,
        &toast.title,
        theme.color(ThemeToken::Primary),
        CellAttrs::NONE,
    );
    push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    push_str(
        &mut cells,
        &format!("{} {}", toast.attention.glyph(), toast.attention.label()),
        toast.attention.color_in(theme),
        CellAttrs::NONE,
    );
    if toast.repeats > 1 {
        push_str(
            &mut cells,
            &format!(" (x{})", toast.repeats),
            theme.color(ThemeToken::Muted),
            CellAttrs::NONE,
        );
    }
    cells.truncate(width);
    // The text above is pushed over the reference surface, like every other
    // chrome helper; translating those roles here is what keeps a notice on the
    // client's own ground at 16 colours instead of leaving one RGB rectangle
    // floating over an otherwise indexed frame.
    theme.map_storm_cells(cells)
}

/// A toast as a positioned span.
#[must_use]
pub fn toast_span(at: Point, toast: &Toast, width: u16) -> Span {
    Span::new(at, toast_cells(toast, width))
}

/// The rows a bounded toast stack draws on, oldest first.
///
/// The upper-right safe area is the rows *between* the client's two fixed chrome
/// rows: a toast never covers the tab row or the status row, because both are
/// always-on surfaces a user reads while the notice is up. `avoid_row` is the
/// focused pane's cursor row, and it is skipped rather than drawn over — a
/// notice may pass in front of a harness, never in front of the line it is being
/// typed into. Fewer rows than `count` means the frame is too short to hold the
/// whole stack, and the oldest notices are the ones that fit.
#[must_use]
pub fn toast_rows(outer: Size, count: usize, avoid_row: Option<u16>) -> Vec<u16> {
    let last = outer.rows.saturating_sub(2);
    let mut rows = Vec::new();
    let mut row = 1u16;
    while rows.len() < count && row <= last && row != u16::MAX {
        if Some(row) != avoid_row {
            rows.push(row);
        }
        row += 1;
    }
    rows
}

/// One toast, right-aligned in the outer frame's upper-right safe area.
///
/// The width is the toast's own — a notice floats, so it claims no more of a row
/// than its text needs — bounded by [`TOAST_MAX_WIDTH`] and by the frame itself.
#[must_use]
pub fn toast_stack_span(outer: Size, row: u16, toast: &Toast, theme: Theme) -> Span {
    let width = outer
        .cols
        .saturating_sub(TOAST_MARGIN.saturating_mul(2))
        .min(TOAST_MAX_WIDTH);
    let cells = toast_cells_in(toast, width, theme);
    let len = u16::try_from(cells.len()).unwrap_or(u16::MAX);
    let col = outer.cols.saturating_sub(len.saturating_add(TOAST_MARGIN));
    Span::new(Point::new(col, row), cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::motion::{FRAME_BUDGET, MOTION_STEPS};

    /// The header's text, with styling discarded.
    fn text_of(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    /// The first cell holding `ch`.
    fn cell_of(cells: &[Cell], ch: char) -> Cell {
        cells
            .iter()
            .copied()
            .find(|cell| cell.ch == ch)
            .expect("the glyph is present")
    }

    /// The foreground of the cell holding `ch`.
    fn fg_of(cells: &[Cell], ch: char) -> Color {
        cell_of(cells, ch).fg
    }

    /// The background of the cell holding `ch`.
    fn bg_of(cells: &[Cell], ch: char) -> Color {
        cell_of(cells, ch).bg
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
    fn terminal_palette_theme_keeps_focus_and_attention_distinct_without_truecolor() {
        let pane = PaneChrome::new(1, "agent")
            .attention(Attention::NeedsInput)
            .focused(true);
        let options = ChromeOptions::no_dim().with_theme(Theme::terminal());
        let row = header_cells(&pane, wide(), options);

        // Both meanings remain readable even when a terminal owns the actual
        // palette: ASCII carries the state, and their ANSI semantic colours do
        // not collapse into one another.
        assert_eq!(fg_of(&row, '>'), Color::Indexed(13));
        assert_eq!(fg_of(&row, '!'), Color::Indexed(11));
        assert!(text_of(&row).contains("! needs input"));

        let span = Span::new(Point::new(0, 0), row);
        let mut renderer = crate::renderer::Renderer::new(cloo_proto::TermCaps::default());
        let bytes = renderer.render_spans(&[span], None);
        assert!(!bytes.windows(3).any(|window| window == b";2;"));
        assert!(bytes.windows(4).any(|window| window == b";95m"));
        assert!(bytes.windows(4).any(|window| window == b";93m"));
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
    fn a_body_span_themes_defaults_without_rewriting_explicit_colours() {
        let cells = [
            Cell::default(),
            Cell {
                ch: 'x',
                fg: Color::Rgb(1, 2, 3),
                bg: Color::Indexed(4),
                attrs: CellAttrs::BOLD,
            },
        ];
        let original = cells;
        let theme = Theme::named(
            cloo_core::ThemeName::Nord,
            cloo_proto::TermCaps {
                truecolor: true,
                ..cloo_proto::TermCaps::default()
            },
        );
        let span = body_span(
            Point::new(2, 3),
            &cells,
            true,
            ChromeOptions::no_dim().with_theme(theme),
        );

        assert_eq!(span.at, Point::new(2, 3));
        assert_eq!(span.cells[0].fg, theme.color(ThemeToken::DefaultText));
        assert_eq!(span.cells[0].bg, theme.color(ThemeToken::Surface));
        assert_eq!(span.cells[1], cells[1]);
        assert_eq!(cells, original, "render-time mapping must not alter input");
    }

    #[test]
    fn active_resize_lights_only_its_divider_and_labels_the_ratio() {
        let points = [Point::new(10, 2), Point::new(10, 3)];
        let spans = resize_affordance_spans(
            &points,
            Direction::Horizontal,
            0.625,
            Size::new(40, 8),
            Theme::storm(),
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].at, points[0]);
        assert_eq!(spans[1].at, points[1]);
        for span in &spans[..2] {
            assert_eq!(text_of(&span.cells), "│");
            assert_eq!(span.cells[0].fg, ACCENT);
            assert!(span.cells[0].attrs.contains(CellAttrs::BOLD));
        }
        assert_eq!(spans[2].at.row, 7);
        assert_eq!(text_of(&spans[2].cells), "resize · ratio 0.62");
    }

    #[test]
    fn active_resize_uses_the_sixteen_colour_theme_without_losing_text() {
        let theme = Theme::named(cloo_core::ThemeName::Storm, cloo_proto::TermCaps::default());
        let spans = resize_affordance_spans(
            &[Point::new(3, 2)],
            Direction::Vertical,
            0.4,
            Size::new(24, 6),
            theme,
        );
        assert_eq!(spans[0].cells[0].ch, '─');
        assert_eq!(spans[0].cells[0].fg, Color::Indexed(13));
        assert_eq!(
            text_of(&spans.last().expect("a ratio label").cells),
            "resize · ratio 0.40"
        );
    }

    #[test]
    fn terminal_inheritance_keeps_body_defaults() {
        let cells = [Cell::default()];
        let span = body_span(
            Point::new(0, 0),
            &cells,
            true,
            ChromeOptions::no_dim().with_theme(Theme::terminal()),
        );
        assert_eq!(span.cells, cells);
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

    /// Three tabs with the middle one active — the shape every yield rule needs.
    fn bar_tabs() -> Vec<TabSummary> {
        vec![
            TabSummary {
                tab: cloo_proto::TabId::new(3),
                title: "edit".into(),
                active: false,
            },
            TabSummary {
                tab: cloo_proto::TabId::new(8),
                title: "build".into(),
                active: true,
            },
            TabSummary {
                tab: cloo_proto::TabId::new(9),
                title: "logs".into(),
                active: false,
            },
        ]
    }

    fn tab_row_text(bar: TabBar<'_>, width: u16) -> String {
        text_of(&tab_row_cells(bar, width, ChromeOptions::default()))
    }

    #[test]
    fn tab_row_marks_the_active_tab_without_reordering_the_bar() {
        let tabs = bar_tabs();
        let row = tab_row_cells(
            TabBar::new(&tabs).session("dev"),
            32,
            ChromeOptions::default(),
        );

        assert_eq!(text_of(&row), " dev  1 edit >2 build  3 logs   ");
        assert_eq!(row.len(), 32);
        assert_eq!(fg_of(&row, '>'), ACCENT);
        assert_eq!(fg_of(&row, 'd'), SURFACE, "the badge sits on the accent");
        assert_eq!(bg_of(&row, 'd'), ACCENT);
    }

    #[test]
    fn the_active_tab_chip_is_raised_and_underlined_as_well_as_marked() {
        let tabs = bar_tabs();
        let row = tab_row_cells(TabBar::new(&tabs), 32, ChromeOptions::default());

        let active = cell_of(&row, 'b');
        assert_eq!(active.bg, RAISED_SURFACE);
        assert!(active.attrs.contains(CellAttrs::UNDERLINE));
        assert!(active.attrs.contains(CellAttrs::BOLD));

        let inactive = cell_of(&row, 'e');
        assert_eq!(inactive.bg, SURFACE);
        assert_eq!(inactive.attrs, CellAttrs::NONE);
        assert_eq!(inactive.fg, MUTED);
    }

    #[test]
    fn the_active_tab_row_marker_survives_without_colour() {
        let tabs = bar_tabs();
        let bar = TabBar::new(&tabs).session("dev").panes(2).clients(1);
        let reference = tab_row_text(bar, 60);

        for theme in [
            Theme::storm(),
            Theme::named(cloo_core::ThemeName::Storm, cloo_proto::TermCaps::default()),
            Theme::terminal(),
        ] {
            let options = ChromeOptions::default().with_theme(theme);
            let row = tab_row_cells(bar, 60, options);
            assert_eq!(
                text_of(&row),
                reference,
                "no theme may change which characters the row draws"
            );
            let active = cell_of(&row, 'b');
            assert!(
                active.attrs.contains(CellAttrs::UNDERLINE),
                "the lower-edge treatment must not depend on colour"
            );
            assert!(reference.contains(">2 build"));
        }
    }

    #[test]
    fn tab_row_metadata_is_right_aligned_and_yields_before_any_tab() {
        let tabs = bar_tabs();
        let bar = TabBar::new(&tabs).session("dev").panes(2).clients(1);

        // Widest: badge, every chip, and the spelled-out metadata.
        assert_eq!(
            tab_row_text(bar, 60),
            " dev  1 edit >2 build  3 logs              2 panes  1 client"
        );
        // Metadata compacts before a tab is given up.
        assert_eq!(
            tab_row_text(bar, 40),
            " dev  1 edit >2 build  3 logs      2p 1c"
        );
        // Then disappears, still before a tab is given up.
        assert_eq!(tab_row_text(bar, 32), " dev  1 edit >2 build  3 logs   ");
    }

    #[test]
    fn a_narrow_tab_row_yields_tabs_then_the_badge_then_the_title() {
        let tabs = bar_tabs();
        let bar = TabBar::new(&tabs).session("dev").panes(2).clients(1);

        // The far-right inactive tab goes first, then the far-left one.
        assert_eq!(tab_row_text(bar, 24), " dev  1 edit >2 build   ");
        assert_eq!(tab_row_text(bar, 16), " dev >2 build   ");
        // Only then does the badge reduce to its glyph, and then disappear.
        assert_eq!(tab_row_text(bar, 12), " s >2 build ");
        assert_eq!(tab_row_text(bar, 8), ">2 build");
        // Below that the title truncates, and the marker is the last thing left.
        assert_eq!(tab_row_text(bar, 5), ">2 bu");
        assert_eq!(tab_row_text(bar, 3), ">2 ");
        assert_eq!(tab_row_text(bar, 1), ">");
    }

    #[test]
    fn a_tab_row_omits_metadata_the_daemon_has_not_published() {
        let tabs = bar_tabs();
        // No session name and no client count: nothing is invented for either.
        let row = tab_row_text(TabBar::new(&tabs).panes(1), 40);
        assert_eq!(row, " 1 edit >2 build  3 logs          1 pane");
        assert!(!row.contains("client"));

        // An empty projected name is absent, not a zero-width badge.
        assert_eq!(
            tab_row_text(TabBar::new(&tabs).session(""), 30),
            " 1 edit >2 build  3 logs      "
        );
    }

    #[test]
    fn a_tab_row_span_keeps_the_caller_position() {
        let span = tab_row_span(
            Point::new(2, 0),
            TabBar::default(),
            10,
            ChromeOptions::default(),
        );
        assert_eq!(span.at, Point::new(2, 0));
        assert_eq!(span.cells.len(), 10);
    }

    // -- Attention queue --------------------------------------------------

    /// The pane indices in the queue, newest first.
    fn order(queue: &AttentionQueue) -> Vec<u16> {
        queue.entries().iter().map(|entry| entry.index).collect()
    }

    #[test]
    fn only_actionable_states_enter_the_queue() {
        let mut queue = AttentionQueue::new();
        for state in [Attention::Unknown, Attention::Working, Attention::Quiet] {
            queue.record(1, "sh", state);
        }
        assert!(queue.is_empty(), "progress and no-news are not queued");
        for state in [Attention::NeedsInput, Attention::Ready, Attention::Failed] {
            assert!(state.is_actionable(), "{state:?} must be queued");
        }
    }

    #[test]
    fn the_queue_lists_newest_first() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        queue.record(3, "c", Attention::Failed);
        assert_eq!(order(&queue), vec![3, 2, 1]);
        assert_eq!(queue.count(), 3);
    }

    #[test]
    fn a_repeat_of_the_same_state_coalesces_without_reordering() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        // Pane 1 re-announces the same state every tick.
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(1, "a", Attention::NeedsInput);
        assert_eq!(
            order(&queue),
            vec![2, 1],
            "a repeat must not churn the list"
        );
        assert_eq!(queue.count(), 2);
    }

    #[test]
    fn a_changed_state_moves_the_pane_to_the_front() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        queue.record(1, "a", Attention::Failed);
        assert_eq!(order(&queue), vec![1, 2], "a new event is the newest");
        assert_eq!(queue.entries()[0].attention, Attention::Failed);
    }

    #[test]
    fn acknowledging_removes_a_pane_and_blocks_the_same_state_returning() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        assert_eq!(queue.acknowledge(1), Some(1));
        assert!(queue.is_empty());
        // The harness keeps announcing needs_input; the user already cleared it.
        queue.record(1, "a", Attention::NeedsInput);
        assert!(
            queue.is_empty(),
            "an acknowledged state must not refill the queue"
        );
    }

    #[test]
    fn a_different_state_after_acknowledgment_alerts_again() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.acknowledge(1);
        queue.record(1, "a", Attention::Failed);
        assert_eq!(order(&queue), vec![1], "a genuinely new event is heard");
    }

    #[test]
    fn returning_to_a_quiet_state_forgets_the_acknowledgment() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.acknowledge(1);
        // The pane finishes and later needs input again: a fresh event.
        queue.record(1, "a", Attention::Quiet);
        queue.record(1, "a", Attention::NeedsInput);
        assert_eq!(
            order(&queue),
            vec![1],
            "a lull resets the slate for the next real event"
        );
    }

    #[test]
    fn a_pane_leaving_the_queue_drops_its_entry() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        queue.record(1, "a", Attention::Working);
        assert_eq!(order(&queue), vec![2], "working is not an ask");
    }

    #[test]
    fn navigation_and_focus_track_the_selected_entry() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        queue.record(3, "c", Attention::Failed);
        // Order is [3, 2, 1]; the cursor starts at the newest.
        assert_eq!(queue.focus_target(), Some(3));
        queue.select_next();
        assert_eq!(queue.focus_target(), Some(2));
        queue.select_next();
        queue.select_next();
        assert_eq!(queue.focus_target(), Some(1), "selection clamps at the end");
        queue.select_prev();
        assert_eq!(queue.focus_target(), Some(2));
    }

    #[test]
    fn acknowledge_selected_clears_the_cursor_entry() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::Ready);
        // Order [2, 1]; cursor on 2.
        assert_eq!(queue.acknowledge_selected(), Some(2));
        assert_eq!(order(&queue), vec![1]);
    }

    // -- Summary rendering ------------------------------------------------

    #[test]
    fn the_summary_tallies_each_state_with_a_glyph_and_colour() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::NeedsInput);
        queue.record(2, "b", Attention::NeedsInput);
        queue.record(3, "c", Attention::Failed);
        let cells = summary_cells(&queue);
        // Fixed urgency order: needs_input, then failed.
        assert_eq!(text_of(&cells), "2! 1x");
        assert_eq!(fg_of(&cells, '!'), Attention::NeedsInput.color());
        assert_eq!(fg_of(&cells, 'x'), Attention::Failed.color());
    }

    #[test]
    fn an_empty_queue_summarises_to_nothing() {
        assert!(summary_cells(&AttentionQueue::new()).is_empty());
    }

    #[test]
    fn a_summary_span_sits_where_it_is_placed() {
        let mut queue = AttentionQueue::new();
        queue.record(1, "a", Attention::Ready);
        let span = summary_span(Point::new(3, 0), &queue);
        assert_eq!(span.at, Point::new(3, 0));
        assert!(!span.cells.is_empty());
    }

    // -- Status bar ------------------------------------------------------

    fn status_tabs() -> Vec<TabSummary> {
        vec![
            TabSummary {
                tab: cloo_proto::TabId::new(3),
                title: "shell".into(),
                active: false,
            },
            TabSummary {
                tab: cloo_proto::TabId::new(8),
                title: "build".into(),
                active: true,
            },
        ]
    }

    fn status_queue() -> AttentionQueue {
        let mut queue = AttentionQueue::new();
        queue.record(1, "lint", Attention::NeedsInput);
        queue.record(2, "test", Attention::NeedsInput);
        queue.record(3, "build", Attention::Failed);
        queue
    }

    fn status_text(
        tabs: &[TabSummary],
        queue: &AttentionQueue,
        hint: &PrefixHint,
        width: u16,
    ) -> String {
        text_of(&status_bar_cells(
            StatusBar::new(tabs, queue, hint).session("main"),
            width,
            ChromeOptions::default(),
        ))
    }

    #[test]
    fn status_bar_reference_width_has_truthful_segmented_fields() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let repository = RepositoryStatus {
            branch: Some("feature/status".to_owned()),
            changes: 2,
        };
        let row = status_bar_cells(
            StatusBar::new(&tabs, &queue, &hint)
                .session("main")
                .clients(2)
                .repository(&repository)
                .clock("14:38"),
            96,
            ChromeOptions::default(),
        );
        let text = text_of(&row);
        for field in [
            "s main",
            " 1 shell ",
            ">2 build",
            "2! 1x",
            "git feature/status +2",
            "2 clients",
            "C-b",
            "14:38",
        ] {
            assert!(text.contains(field), "missing {field:?} in {text:?}");
        }
        assert_eq!(
            bg_of(&row, 's'),
            ACCENT,
            "the session is the accent segment"
        );
        assert_eq!(fg_of(&row, '>'), ACCENT, "the active tab stays visible");
        assert_eq!(fg_of(&row, '!'), Attention::NeedsInput.color());
    }

    #[test]
    fn status_bar_yields_optional_detail_before_required_markers() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let repository = RepositoryStatus {
            branch: Some("main".to_owned()),
            changes: 2,
        };
        let bar = StatusBar::new(&tabs, &queue, &hint)
            .session("main")
            .clients(2)
            .repository(&repository)
            .clock("14:38");
        let wide = text_of(&status_bar_cells(bar, 80, ChromeOptions::default()));
        let narrow = text_of(&status_bar_cells(bar, 30, ChromeOptions::default()));
        assert!(wide.contains("14:38") && wide.contains("git main +2"));
        assert!(!narrow.contains("14:38") && !narrow.contains("git"));
        for marker in ['s', '>', '!', 'b'] {
            assert!(
                narrow.contains(marker),
                "required marker {marker:?}: {narrow:?}"
            );
        }
        assert_eq!(
            text_of(&status_bar_cells(bar, 4, ChromeOptions::default())),
            "s>!b",
            "four ASCII markers keep every field at the narrowest useful size"
        );
    }

    #[test]
    fn a_status_bar_uses_ascii_tokens_and_a_zero_attention_count() {
        let tabs = status_tabs();
        let queue = AttentionQueue::new();
        let hint = PrefixHint::default();
        let row = status_bar_cells(
            StatusBar::new(&tabs, &queue, &hint).session("main"),
            40,
            ChromeOptions::default(),
        );
        let text = text_of(&row);
        assert!(row.iter().all(|cell| cell.ch.is_ascii()));
        assert!(text.contains("0!"), "zero is an explicit attention count");
        assert!(text.contains(DEFAULT_PREFIX_HINT));
    }

    #[test]
    fn a_status_bar_span_keeps_its_origin_and_width() {
        let tabs = status_tabs();
        let queue = AttentionQueue::new();
        let hint = PrefixHint::default();
        let span = status_bar_span(
            Point::new(4, 23),
            StatusBar::new(&tabs, &queue, &hint).session("main"),
            20,
            ChromeOptions::default(),
        );
        assert_eq!(span.at, Point::new(4, 23));
        assert_eq!(span.cells.len(), 20);
    }

    #[test]
    fn powerline_status_reference_width_renders_every_available_field() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let repository = RepositoryStatus {
            branch: Some("feature/status".to_owned()),
            changes: 2,
        };
        let row = status_bar_cells(
            StatusBar::new(&tabs, &queue, &hint)
                .mode(StatusMode::Powerline)
                .session("main")
                .clients(2)
                .effective_size(Size::new(132, 38))
                .repository(&repository)
                .clock("14:38"),
            96,
            ChromeOptions::default(),
        );
        let text = text_of(&row);
        for field in [
            "NORMAL",
            "s main",
            ">2 build",
            "git feature/status +2",
            "2 clients · min 132x38",
            "14:38",
        ] {
            assert!(text.contains(field), "missing {field:?} in {text:?}");
        }
        assert_eq!(
            text.matches('\u{e0b0}').count(),
            4,
            "one glyph per boundary"
        );
        assert_eq!(bg_of(&row, 'N'), ACCENT);
        assert_eq!(fg_of(&row, '>'), ACCENT);
        let separator = cell_of(&row, '\u{e0b0}');
        assert_eq!(separator.fg, ACCENT);
        assert_eq!(separator.bg, BORDER);
    }

    #[test]
    fn powerline_status_uses_attention_when_repository_data_is_unavailable() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let text = text_of(&status_bar_cells(
            StatusBar::new(&tabs, &queue, &hint)
                .mode(StatusMode::Powerline)
                .session("main"),
            50,
            ChromeOptions::default(),
        ));
        assert!(
            text.contains("2! 1x"),
            "attention is the truthful fallback: {text:?}"
        );
        assert!(
            !text.contains("git"),
            "no repository answer was supplied: {text:?}"
        );
    }

    #[test]
    fn powerline_status_flat_fallback_keeps_field_truth_and_order() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let repository = RepositoryStatus {
            branch: Some("main".to_owned()),
            changes: 2,
        };
        let base = StatusBar::new(&tabs, &queue, &hint)
            .mode(StatusMode::Powerline)
            .session("main")
            .repository(&repository);
        let glyph = text_of(&status_bar_cells(base, 50, ChromeOptions::default()));
        let flat = text_of(&status_bar_cells(
            base.powerline_separators(false),
            50,
            ChromeOptions::default(),
        ));

        assert!(glyph.contains('\u{e0b0}'));
        assert!(!flat.contains('\u{e0b0}'));
        let positions = ["NORMAL", "s main", ">2 build", "git main +2"].map(|field| {
            flat.find(field)
                .unwrap_or_else(|| panic!("missing {field:?} in {flat:?}"))
        });
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn powerline_status_has_exact_narrow_goldens_and_ascii_floor() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let repository = RepositoryStatus {
            branch: Some("main".to_owned()),
            changes: 2,
        };
        let bar = StatusBar::new(&tabs, &queue, &hint)
            .mode(StatusMode::Powerline)
            .session("main")
            .repository(&repository);
        for (width, expected) in [
            (
                42,
                " NORMAL \u{e0b0} s main \u{e0b0} >2 build \u{e0b0} git main +2 ",
            ),
            (
                37,
                " NORMAL \u{e0b0} s main \u{e0b0} >2 build \u{e0b0} git +2 ",
            ),
            (31, " NORMAL \u{e0b0} s main \u{e0b0} >2 \u{e0b0} git +2 "),
            (26, " NORMAL \u{e0b0} s \u{e0b0} >2 \u{e0b0} git +2 "),
            (25, " NORMAL \u{e0b0} s \u{e0b0} > \u{e0b0} git +2 "),
            (15, " N \u{e0b0} s \u{e0b0} > \u{e0b0} g "),
            (4, "Ns>g"),
        ] {
            assert_eq!(
                text_of(&status_bar_cells(bar, width, ChromeOptions::default())),
                expected,
                "width {width}"
            );
        }
        let floor = status_bar_cells(bar, 4, ChromeOptions::default());
        assert_eq!(floor[0].fg, ACCENT, "the mode marker remains visible");
    }

    #[test]
    fn powerline_status_sixteen_color_golden_keeps_text_and_uses_indexed_roles() {
        let tabs = status_tabs();
        let queue = status_queue();
        let hint = PrefixHint::default();
        let bar = StatusBar::new(&tabs, &queue, &hint)
            .mode(StatusMode::Powerline)
            .session("main");
        let reference = status_bar_cells(bar, 40, ChromeOptions::default());
        let ansi_theme = Theme::named(cloo_core::ThemeName::Storm, cloo_proto::TermCaps::default());
        let ansi = status_bar_cells(bar, 40, ChromeOptions::default().with_theme(ansi_theme));
        assert_eq!(text_of(&ansi), text_of(&reference));
        assert!(ansi.iter().all(|cell| !matches!(cell.fg, Color::Rgb(..))));
        assert!(ansi.iter().all(|cell| !matches!(cell.bg, Color::Rgb(..))));
        assert_eq!(bg_of(&ansi, 'N'), Color::Indexed(13));
    }

    // -- First-attach shortcut hints ---------------------------------------

    /// The row a workspace with `panes` panes draws at `width`.
    fn hinted_row(hint: &PrefixHint, width: u16) -> String {
        let tabs = status_tabs();
        let queue = AttentionQueue::new();
        status_text(&tabs, &queue, hint, width)
    }

    #[test]
    fn one_pane_spends_trailing_width_on_the_split_stack_and_help_clues() {
        let hint = PrefixHint::for_panes("C-b", 1);
        assert!(hint.is_guided(), "one pane is the first-attach shape");
        let row = hinted_row(&hint, 60);
        assert!(
            row.contains("s main") && row.contains("C-b split % stack \" help ?"),
            "the widest form names the workspace and all three clues: {row:?}"
        );
    }

    #[test]
    fn a_second_pane_wins_the_clue_width_back() {
        let hint = PrefixHint::for_panes("C-b", 2);
        assert!(!hint.is_guided());
        let row = hinted_row(&hint, 48);
        assert!(row.contains("s main") && row.contains("C-b"), "{row:?}");
        assert!(
            !row.contains("split %"),
            "past the first pane guidance is gone: {row:?}"
        );
    }

    #[test]
    fn the_clues_yield_from_the_end_before_any_core_field_does() {
        let hint = PrefixHint::for_panes("C-b", 1);
        // Every rung below still carries the session, active tab, and attention
        // tab, and attention fields are untouched while the clues are spent.
        for (width, expected) in [
            (60, "C-b split % stack \" help ?"),
            (53, "C-b split % stack \""),
            (45, "C-b split %"),
        ] {
            let row = hinted_row(&hint, width);
            for core in ["s main", "1 shell", ">2 build", "0!"] {
                assert!(
                    row.contains(core),
                    "core field {core:?} yielded before the clues at width {width}: {row:?}"
                );
            }
            assert!(
                row.trim_end().ends_with(expected),
                "width {width} expected the hint {expected:?}, got {row:?}"
            );
        }
    }

    #[test]
    fn a_configured_prefix_is_drawn_verbatim_down_to_its_last_marker() {
        let hint = PrefixHint::for_panes("M-Space", 1);
        assert_eq!(hint.prefix(), "M-Space");
        assert!(
            hinted_row(&hint, 50).contains("M-Space split %"),
            "the configured chord, not a hard-coded C-b"
        );
        assert_eq!(
            hinted_row(&hint, 4),
            "s>!e",
            "the marker is the configured spelling's own last character"
        );
    }

    #[test]
    fn a_pending_prefix_is_textually_distinct_and_offers_the_clues() {
        let settled = PrefixHint::for_panes("C-b", 4);
        let pending = settled.clone().pending(true);
        assert!(!settled.is_pending() && pending.is_pending());

        let settled_row = hinted_row(&settled, 62);
        let pending_row = hinted_row(&pending, 62);
        assert!(
            !settled_row.contains("[C-b]"),
            "a settled prefix is never bracketed: {settled_row:?}"
        );
        assert!(
            pending_row.contains("[C-b] split % stack \" help ?"),
            "a pending prefix is bracketed and explains itself: {pending_row:?}"
        );
        assert_eq!(
            fg_of(
                &status_bar_cells(
                    StatusBar::new(&status_tabs(), &AttentionQueue::new(), &pending)
                        .session("main"),
                    62,
                    ChromeOptions::default(),
                ),
                '['
            ),
            ACCENT,
            "colour supplements the bracket rather than carrying it"
        );
    }

    #[test]
    fn every_hinted_row_is_exactly_its_width_and_ascii_at_every_width() {
        let hint = PrefixHint::for_panes("C-b", 1).pending(true);
        let tabs = status_tabs();
        let queue = status_queue();
        for width in 0..=60_u16 {
            let row = status_bar_cells(
                StatusBar::new(&tabs, &queue, &hint).session("main"),
                width,
                ChromeOptions::default(),
            );
            assert_eq!(row.len(), usize::from(width), "width {width}");
            assert!(
                row.iter().all(|cell| cell.ch.is_ascii()),
                "width {width} rendered a non-ASCII signal"
            );
        }
    }

    #[test]
    fn the_default_hint_is_the_documented_default_spelling() {
        let hint = PrefixHint::default();
        assert_eq!(hint.prefix(), "C-b");
        assert!(!hint.is_guided());
        assert!(hinted_row(&hint, 40).contains(DEFAULT_PREFIX_HINT));
    }

    // -- Queue row rendering ----------------------------------------------

    #[test]
    fn every_actionable_state_renders_text_glyph_and_colour_in_a_row() {
        for state in [Attention::NeedsInput, Attention::Ready, Attention::Failed] {
            // A title free of any state glyph, so the glyph lookup is unambiguous.
            let entry = QueueEntry {
                index: 2,
                title: "agent".into(),
                attention: state,
            };
            let row = queue_row_cells(&entry, false, wide());
            let text = text_of(&row);
            assert!(
                text.contains(state.glyph()) && text.contains(state.label()),
                "{state:?} rendered as {text:?}"
            );
            assert_eq!(
                fg_of(&row, state.glyph()),
                state.color(),
                "{state:?} keeps its semantic colour"
            );
        }
    }

    #[test]
    fn a_selected_row_wears_the_cursor_marker_and_an_unselected_one_does_not() {
        let entry = QueueEntry {
            index: 3,
            title: "claude".into(),
            attention: Attention::NeedsInput,
        };
        let selected = queue_row_cells(&entry, true, wide());
        let plain = queue_row_cells(&entry, false, wide());
        assert!(text_of(&selected).starts_with('>'), "the cursor is visible");
        assert!(!text_of(&plain).starts_with('>'));
        assert_eq!(fg_of(&selected, 'c'), ACCENT, "the selected title accents");
    }

    #[test]
    fn a_queue_row_is_exactly_the_width_at_every_size() {
        let entry = QueueEntry {
            index: 12,
            title: "claude-code".into(),
            attention: Attention::Failed,
        };
        for width in 0_u16..=40 {
            assert_eq!(
                queue_row_cells(&entry, true, width).len(),
                usize::from(width)
            );
        }
    }

    #[test]
    fn a_queue_row_span_carries_its_origin() {
        let entry = QueueEntry {
            index: 1,
            title: "sh".into(),
            attention: Attention::Ready,
        };
        let span = queue_row_span(Point::new(5, 7), &entry, false, 20);
        assert_eq!(span.at, Point::new(5, 7));
        assert_eq!(span.cells.len(), 20);
    }

    // -- Toasts -----------------------------------------------------------

    /// A toast with a repeat count, without going through a deck.
    fn repeated_toast(index: u16, title: &str, attention: Attention, repeats: u32) -> Toast {
        let mut toast = Toast::new(index, title, attention, Instant::now());
        toast.repeats = repeats;
        toast
    }

    #[test]
    fn a_toast_deck_is_bounded_and_evicts_the_oldest() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(2);
        deck.push(1, "a", Attention::NeedsInput, now);
        deck.push(2, "b", Attention::Ready, now);
        deck.push(3, "c", Attention::Failed, now);
        assert_eq!(deck.len(), 2, "capacity is never exceeded");
        let indices: Vec<u16> = deck.toasts().map(|toast| toast.index).collect();
        assert_eq!(indices, vec![2, 3], "the oldest was dropped");
    }

    #[test]
    fn a_repeat_toast_coalesces_and_moves_to_the_newest() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3);
        deck.push(1, "a", Attention::NeedsInput, now);
        deck.push(2, "b", Attention::Ready, now);
        deck.push(1, "a", Attention::Failed, now);
        assert_eq!(deck.len(), 1 + 1, "a repeat is one notice, not two");
        let toasts: Vec<&Toast> = deck.toasts().collect();
        assert_eq!(toasts[0].index, 2, "the untouched toast is now oldest");
        assert_eq!(toasts[1].index, 1);
        assert_eq!(toasts[1].repeats, 2);
        assert_eq!(toasts[1].attention, Attention::Failed, "state refreshes");
    }

    #[test]
    fn a_zero_capacity_deck_still_holds_one() {
        let mut deck = ToastDeck::new(0);
        deck.push(1, "a", Attention::NeedsInput, Instant::now());
        assert_eq!(deck.len(), 1);
    }

    #[test]
    fn dismissing_removes_a_panes_toast() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3);
        deck.push(1, "a", Attention::NeedsInput, now);
        deck.push(2, "b", Attention::Ready, now);
        deck.dismiss(1);
        let indices: Vec<u16> = deck.toasts().map(|toast| toast.index).collect();
        assert_eq!(indices, vec![2]);
    }

    #[test]
    fn a_closed_panes_position_takes_its_toast_with_it() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3);
        deck.push(1, "a", Attention::NeedsInput, now);
        deck.push(3, "c", Attention::Failed, now);
        deck.retain_within(2);
        let indices: Vec<u16> = deck.toasts().map(|toast| toast.index).collect();
        assert_eq!(indices, vec![1], "a notice never outlives its pane");
    }

    // -- Toast lifetime and entrance --------------------------------------

    #[test]
    fn a_toast_dismisses_itself_on_its_own_deadline() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3).lifetime(Duration::from_secs(1));
        deck.push(1, "claude", Attention::NeedsInput, now);
        assert_eq!(
            deck.toasts().next().map(Toast::expires_at),
            Some(now + Duration::from_secs(1))
        );

        assert!(!deck.is_empty());
        let _ = deck.tick(now + Duration::from_millis(999));
        assert_eq!(deck.len(), 1, "a notice inside its lifetime stays up");
        assert!(
            deck.tick(now + Duration::from_secs(1)),
            "the deadline passing changes the frame"
        );
        assert!(deck.is_empty(), "a toast never covers a pane indefinitely");
        assert!(
            !deck.tick(now + Duration::from_secs(30)),
            "an empty deck asks for no further frames"
        );
    }

    #[test]
    fn a_refreshed_toast_restarts_its_deadline() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3).lifetime(Duration::from_secs(1));
        deck.push(1, "claude", Attention::NeedsInput, now);
        deck.push(
            1,
            "claude",
            Attention::Failed,
            now + Duration::from_millis(900),
        );
        let _ = deck.tick(now + Duration::from_secs(1));
        assert_eq!(
            deck.len(),
            1,
            "the refresh, not the first raise, is the clock"
        );
        let _ = deck.tick(now + Duration::from_millis(1900));
        assert!(deck.is_empty());
    }

    #[test]
    fn an_entrance_is_frame_budgeted_and_settles_into_the_chromes_own_cells() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3);
        deck.push(1, "claude", Attention::NeedsInput, now);
        let entering = deck.toasts().next().expect("one toast").phase();
        assert_eq!(entering.map(Phase::step), Some(0), "it enters over frames");

        // Sampled far faster than the frame budget — a burst of PTY reads —
        // the entrance still advances at most once per budget.
        let mut frames = 0;
        for n in 0..1000 {
            if deck.tick(now + Duration::from_micros(n * 200)) {
                frames += 1;
            }
        }
        assert!(
            frames <= usize::from(MOTION_STEPS),
            "{frames} frames for one entrance"
        );
        assert_eq!(
            deck.toasts().next().expect("one toast").phase(),
            None,
            "a settled toast draws the chrome's own cells"
        );
    }

    #[test]
    fn reduce_motion_gives_a_toast_no_entrance_at_all() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3).motion(MotionSettings::reduced());
        deck.push(1, "claude", Attention::NeedsInput, now);
        assert_eq!(deck.toasts().next().expect("one toast").phase(), None);
        assert!(
            !deck.tick(now + FRAME_BUDGET),
            "reduce-motion asks for no entrance frames"
        );
    }

    #[test]
    fn changing_the_motion_preference_keeps_the_stack() {
        let now = Instant::now();
        let mut deck = ToastDeck::new(3);
        deck.push(1, "claude", Attention::NeedsInput, now);
        deck.set_motion(MotionSettings::reduced());
        assert_eq!(deck.len(), 1, "a preference change is not a dismissal");
        assert_eq!(deck.toasts().next().expect("one toast").phase(), None);
    }

    // -- Toast rendering and placement ------------------------------------

    #[test]
    fn a_toast_line_carries_text_glyph_colour_and_a_repeat_count() {
        let toast = repeated_toast(2, "codex", Attention::NeedsInput, 3);
        let cells = toast_cells(&toast, 40);
        let text = text_of(&cells);
        assert_eq!(text, "codex ! needs input (x3)");
        assert_eq!(fg_of(&cells, '!'), Attention::NeedsInput.color());
    }

    #[test]
    fn a_single_toast_omits_the_repeat_count() {
        let toast = repeated_toast(1, "sh", Attention::Ready, 1);
        assert_eq!(text_of(&toast_cells(&toast, 40)), "sh + ready");
    }

    #[test]
    fn a_toast_is_truncated_to_width_rather_than_padded() {
        let toast = repeated_toast(1, "sh", Attention::Ready, 1);
        assert_eq!(toast_cells(&toast, 4).len(), 4);
        assert!(toast_cells(&toast, 0).is_empty());
    }

    #[test]
    fn a_sixteen_colour_toast_says_the_same_thing_the_truecolour_one_does() {
        let toast = repeated_toast(1, "build", Attention::Failed, 2);
        let ansi = Theme::named(cloo_core::ThemeName::Storm, cloo_proto::TermCaps::default());
        assert_eq!(
            text_of(&toast_cells_in(&toast, 40, ansi)),
            text_of(&toast_cells(&toast, 40)),
            "glyph, label, and repeat count never depend on the palette"
        );
        assert_eq!(
            fg_of(&toast_cells_in(&toast, 40, ansi), 'x'),
            Attention::Failed.color_in(ansi),
            "the state still resolves through the client theme"
        );
        let cells = toast_cells_in(&toast, 40, ansi);
        assert!(
            cells.iter().all(
                |cell| !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..))
            ),
            "a notice must not leave one RGB rectangle over an indexed frame"
        );
        assert_eq!(cells[0].bg, ansi.color(ThemeToken::Surface));
    }

    #[test]
    fn the_stack_sits_between_the_tab_and_status_rows() {
        let outer = Size::new(80, 24);
        assert_eq!(
            toast_rows(outer, 3, None),
            vec![1, 2, 3],
            "the tab row is row zero and is never covered"
        );
        assert!(
            toast_rows(outer, 40, None).iter().all(|row| *row < 23),
            "the status row is never covered either"
        );
        assert!(
            toast_rows(Size::new(80, 2), 3, None).is_empty(),
            "a frame with no room between its chrome rows shows none"
        );
    }

    #[test]
    fn the_focused_cursors_row_is_skipped_rather_than_drawn_over() {
        let rows = toast_rows(Size::new(80, 24), 2, Some(2));
        assert_eq!(
            rows,
            vec![1, 3],
            "a notice never covers the line being typed"
        );
    }

    #[test]
    fn a_stacked_toast_is_right_aligned_inside_the_frame() {
        let outer = Size::new(80, 24);
        let toast = repeated_toast(1, "claude", Attention::NeedsInput, 1);
        let span = toast_stack_span(outer, 1, &toast, Theme::storm());
        let len = u16::try_from(span.cells.len()).expect("a short notice");
        assert_eq!(span.at.row, 1);
        assert_eq!(
            span.at.col + len + TOAST_MARGIN,
            outer.cols,
            "the stack floats against the right edge, inside the margin"
        );
        assert_eq!(text_of(&span.cells), "claude ! needs input");
    }

    #[test]
    fn a_narrow_frame_truncates_a_toast_rather_than_overflowing_it() {
        let outer = Size::new(12, 24);
        let toast = repeated_toast(1, "claude", Attention::NeedsInput, 1);
        let span = toast_stack_span(outer, 1, &toast, Theme::storm());
        let len = u16::try_from(span.cells.len()).expect("a short notice");
        assert!(
            len <= outer.cols - TOAST_MARGIN * 2,
            "{len} cells in {outer:?}"
        );
        assert!(span.at.col + len <= outer.cols);
    }
}
