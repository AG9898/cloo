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

use cloo_proto::{Cell, CellAttrs, Color, Point, SessionId, TabSummary};

use crate::renderer::Span;
use crate::theme::{Theme, ThemeToken};

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

/// Builds the compact top tab row, exactly `width` cells wide.
///
/// Each tab is shown as a one-based bar position and title. The active tab is
/// marked with `>` as well as the accent treatment, so a 16-colour or
/// monochrome terminal still has an unambiguous answer. When the bar is too
/// narrow for every tab, it yields inactive tabs from the far right and then
/// the far left, keeping a contiguous window around the active tab. If even
/// that does not fit, the active title truncates before its marker or index.
#[must_use]
pub fn tab_row_cells(tabs: &[TabSummary], width: u16) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let active = tabs.iter().position(|tab| tab.active).unwrap_or(0);
    let mut first = 0;
    let mut last = tabs.len();
    while first < last && tab_row_len(&tabs[first..last], first) > width {
        if last.saturating_sub(1) != active {
            last -= 1;
        } else if first != active {
            first += 1;
        } else {
            break;
        }
    }

    let visible = &tabs[first..last];
    let mut cells = Vec::with_capacity(width);
    for (offset, tab) in visible.iter().enumerate() {
        if offset > 0 {
            push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
        }
        let index = first + offset + 1;
        let marker = if tab.active { ">" } else { " " };
        let prefix = format!("{marker}{index} ");
        let remaining = width.saturating_sub(cells.len());
        if remaining == 0 {
            break;
        }
        let title_budget = remaining.saturating_sub(len(&prefix));
        let title = truncate(&tab.title, title_budget);
        let (fg, attrs) = if tab.active {
            (ACCENT, CellAttrs::BOLD)
        } else {
            (MUTED, CellAttrs::NONE)
        };
        push_str(&mut cells, &prefix, fg, attrs);
        push_str(&mut cells, title, fg, attrs);
    }
    cells.truncate(width);
    while cells.len() < width {
        push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    }
    cells
}

/// Positions a compact tab row for [`Renderer::render_spans`](crate::renderer::Renderer::render_spans).
#[must_use]
pub fn tab_row_span(at: Point, tabs: &[TabSummary], width: u16) -> Span {
    Span::new(at, tab_row_cells(tabs, width))
}

fn tab_row_len(tabs: &[TabSummary], start: usize) -> usize {
    tabs.iter()
        .enumerate()
        .map(|(offset, tab)| {
            len(&format!(
                "{}{} {}",
                if tab.active { ">" } else { " " },
                start + offset + 1,
                tab.title
            ))
        })
        .sum::<usize>()
        + tabs.len().saturating_sub(1)
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

/// The hint for cloo's default prefix, [`cloo_core::keymap::DEFAULT_PREFIX`].
///
/// The prefix is a chrome concern, not session state: the keymap is the
/// client's, so two clients attached to one session may legitimately show
/// different hints. M4-02 makes the chord configurable; rendering a *rebound*
/// prefix in this row is chrome work still to land, so the row states the
/// default rather than a chord it has not been told about.
pub const DEFAULT_PREFIX_HINT: &str = "C-b ?";

/// Builds the always-on minimal status row, exactly `width` cells wide.
///
/// The flat row carries the session, active tab, attention summary, and prefix
/// hint without depending on colour or non-ASCII glyphs. Its fixed degradation
/// ladder first drops the active tab title, then shortens the session and
/// attention summary, then drops the help suffix from the prefix. At very
/// narrow widths it condenses to `s>!b`: session, tab, attention, and the
/// `C-b` prefix, in that order. A terminal narrower than four cells is the
/// unavoidable physical limit and receives the leading part of that form.
#[must_use]
pub fn status_bar_cells(
    session: SessionId,
    tabs: &[TabSummary],
    queue: &AttentionQueue,
    width: u16,
) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let session_full = text_cells(
        &format!("session:{}", session.get()),
        PRIMARY,
        CellAttrs::BOLD,
    );
    let session_short = text_cells(&format!("s{}", session.get()), PRIMARY, CellAttrs::BOLD);
    let session_mark = text_cells("s", PRIMARY, CellAttrs::BOLD);

    let (tab_index, tab_title) = tabs
        .iter()
        .enumerate()
        .find(|(_, tab)| tab.active)
        .map(|(index, tab)| (index + 1, tab.title.as_str()))
        .unwrap_or((0, ""));
    let tab_short_text = if tab_index == 0 {
        ">?".to_owned()
    } else {
        format!(">{tab_index}")
    };
    let tab_full_text = if tab_title.is_empty() {
        tab_short_text.clone()
    } else {
        format!("{tab_short_text} {tab_title}")
    };
    let tab_full = text_cells(&tab_full_text, ACCENT, CellAttrs::BOLD);
    let tab_short = text_cells(&tab_short_text, ACCENT, CellAttrs::BOLD);
    let tab_mark = text_cells(">", ACCENT, CellAttrs::BOLD);

    let attention_full = status_attention_cells(queue);
    let attention_count = text_cells(
        &format!("{}!", queue.count()),
        if queue.is_empty() { MUTED } else { WARNING },
        CellAttrs::BOLD,
    );
    let attention_mark = text_cells("!", WARNING, CellAttrs::BOLD);

    let prefix_full = text_cells(DEFAULT_PREFIX_HINT, PRIMARY, CellAttrs::NONE);
    let prefix_short = text_cells("C-b", PRIMARY, CellAttrs::NONE);

    // The order here is the documented yield order. Keeping the complete
    // candidate rows explicit makes a narrow status bar deterministic and
    // byte-for-byte testable, like pane headers and tab rows.
    for parts in [
        [
            session_full.as_slice(),
            tab_full.as_slice(),
            attention_full.as_slice(),
            prefix_full.as_slice(),
        ],
        [
            session_full.as_slice(),
            tab_short.as_slice(),
            attention_full.as_slice(),
            prefix_full.as_slice(),
        ],
        [
            session_short.as_slice(),
            tab_short.as_slice(),
            attention_full.as_slice(),
            prefix_full.as_slice(),
        ],
        [
            session_short.as_slice(),
            tab_short.as_slice(),
            attention_count.as_slice(),
            prefix_full.as_slice(),
        ],
        [
            session_short.as_slice(),
            tab_short.as_slice(),
            attention_count.as_slice(),
            prefix_short.as_slice(),
        ],
        [
            session_short.as_slice(),
            tab_mark.as_slice(),
            attention_count.as_slice(),
            prefix_short.as_slice(),
        ],
        [
            session_mark.as_slice(),
            tab_mark.as_slice(),
            attention_mark.as_slice(),
            prefix_short.as_slice(),
        ],
    ] {
        if status_row_len(&parts) <= width {
            return status_row(&parts, width);
        }
    }

    // Four ASCII markers retain every required field down to four cells. The
    // final `b` is the compact spelling of the configured `C-b` prefix.
    let mut cells = Vec::with_capacity(width);
    cells.extend_from_slice(&session_mark);
    cells.extend_from_slice(&tab_mark);
    cells.extend_from_slice(&attention_mark);
    push_str(&mut cells, "b", PRIMARY, CellAttrs::NONE);
    cells.truncate(width);
    pad_status_row(&mut cells, width);
    cells
}

/// Positions the always-on status row for the chrome renderer.
#[must_use]
pub fn status_bar_span(
    at: Point,
    session: SessionId,
    tabs: &[TabSummary],
    queue: &AttentionQueue,
    width: u16,
) -> Span {
    Span::new(at, status_bar_cells(session, tabs, queue, width))
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

/// The number of cells in a status row, including field gaps.
fn status_row_len(parts: &[&[Cell]; 4]) -> usize {
    parts.iter().map(|part| part.len()).sum::<usize>() + parts.len().saturating_sub(1)
}

/// Joins already-fitted fields into one padded status row.
fn status_row(parts: &[&[Cell]; 4], width: usize) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(width);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            push_str(&mut cells, " ", MUTED, CellAttrs::NONE);
        }
        cells.extend_from_slice(part);
    }
    pad_status_row(&mut cells, width);
    cells
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

/// A transient notice that a pane raised an actionable event.
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
}

/// A bounded, coalescing stack of toasts.
///
/// Two rules from the style guide are the whole point: the stack is *bounded*,
/// so a burst can never grow it without limit, and repeated events from the
/// same pane *coalesce* into one notice with a repeat count rather than stacking
/// copies. When a new pane's toast would exceed capacity, the oldest is dropped.
#[derive(Debug, Clone)]
pub struct ToastDeck {
    /// Front = oldest.
    toasts: VecDeque<Toast>,
    capacity: usize,
}

impl ToastDeck {
    /// A deck holding at most `capacity` toasts (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            toasts: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Raises or coalesces a toast for a pane.
    ///
    /// A pane already showing coalesces: its state and title refresh, its repeat
    /// count grows, and it moves to the newest position. A new pane pushes onto
    /// the back, evicting the oldest toast if the deck is full.
    pub fn push(&mut self, index: u16, title: impl Into<String>, attention: Attention) {
        if let Some(pos) = self.toasts.iter().position(|toast| toast.index == index) {
            let mut toast = self.toasts.remove(pos).expect("position just found");
            toast.title = title.into();
            toast.attention = attention;
            toast.repeats = toast.repeats.saturating_add(1);
            self.toasts.push_back(toast);
            return;
        }
        if self.toasts.len() == self.capacity {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            index,
            title: title.into(),
            attention,
            repeats: 1,
        });
    }

    /// Removes a pane's toast, if it has one.
    pub fn dismiss(&mut self, index: u16) {
        self.toasts.retain(|toast| toast.index != index);
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

/// A concise toast line, truncated to `width`.
///
/// Renders `<title> <glyph> <label>` with the state coloured and, when the pane
/// has coalesced more than once, a muted `(xN)` repeat count. Unlike a header it
/// is not padded to width: a toast floats over the layout rather than owning a
/// row.
#[must_use]
pub fn toast_cells(toast: &Toast, width: u16) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let mut cells = Vec::new();
    push_str(&mut cells, &toast.title, PRIMARY, CellAttrs::NONE);
    push_str(&mut cells, " ", Color::Default, CellAttrs::NONE);
    push_str(
        &mut cells,
        &format!("{} {}", toast.attention.glyph(), toast.attention.label()),
        toast.attention.color(),
        CellAttrs::NONE,
    );
    if toast.repeats > 1 {
        push_str(
            &mut cells,
            &format!(" (x{})", toast.repeats),
            MUTED,
            CellAttrs::NONE,
        );
    }
    cells.truncate(width);
    cells
}

/// A toast as a positioned span.
#[must_use]
pub fn toast_span(at: Point, toast: &Toast, width: u16) -> Span {
    Span::new(at, toast_cells(toast, width))
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

    #[test]
    fn tab_row_marks_the_active_tab_without_reordering_the_bar() {
        let tabs = vec![
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
        ];

        let row = tab_row_cells(&tabs, 20);
        assert_eq!(text_of(&row), " 1 shell >2 build   ");
        assert_eq!(row.len(), 20);
        assert_eq!(fg_of(&row, '>'), ACCENT);
    }

    #[test]
    fn a_narrow_tab_row_keeps_the_active_marker_and_index() {
        let tabs = vec![
            TabSummary {
                tab: cloo_proto::TabId::new(0),
                title: "shell".into(),
                active: false,
            },
            TabSummary {
                tab: cloo_proto::TabId::new(1),
                title: "build".into(),
                active: true,
            },
        ];

        assert_eq!(text_of(&tab_row_cells(&tabs, 8)), ">2 build");
        assert_eq!(text_of(&tab_row_cells(&tabs, 3)), ">2 ");
    }

    #[test]
    fn a_tab_row_span_keeps_the_caller_position() {
        let span = tab_row_span(Point::new(2, 0), &[], 10);
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

    #[test]
    fn a_wide_status_bar_has_every_required_field() {
        let queue = status_queue();
        let row = status_bar_cells(SessionId::new(7), &status_tabs(), &queue, 30);
        assert_eq!(text_of(&row), "session:7 >2 build 2! 1x C-b ?");
        assert_eq!(fg_of(&row, '>'), ACCENT, "the active tab stays visible");
        assert_eq!(fg_of(&row, '!'), Attention::NeedsInput.color());
    }

    #[test]
    fn a_narrow_status_bar_yields_detail_before_required_fields() {
        let queue = status_queue();
        assert_eq!(
            text_of(&status_bar_cells(
                SessionId::new(7),
                &status_tabs(),
                &queue,
                12
            )),
            "s7 >2 3! C-b",
            "the tab title, state split, and help suffix yield first"
        );
        assert_eq!(
            text_of(&status_bar_cells(
                SessionId::new(7),
                &status_tabs(),
                &queue,
                6
            )),
            "s>!b  ",
            "four ASCII markers keep every field at the narrowest useful size"
        );
    }

    #[test]
    fn a_status_bar_uses_ascii_tokens_and_a_zero_attention_count() {
        let queue = AttentionQueue::new();
        let row = status_bar_cells(SessionId::new(1), &status_tabs(), &queue, 40);
        let text = text_of(&row);
        assert!(row.iter().all(|cell| cell.ch.is_ascii()));
        assert!(text.contains("0!"), "zero is an explicit attention count");
        assert!(text.contains(DEFAULT_PREFIX_HINT));
    }

    #[test]
    fn a_status_bar_span_keeps_its_origin_and_width() {
        let queue = AttentionQueue::new();
        let span = status_bar_span(
            Point::new(4, 23),
            SessionId::new(1),
            &status_tabs(),
            &queue,
            20,
        );
        assert_eq!(span.at, Point::new(4, 23));
        assert_eq!(span.cells.len(), 20);
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

    #[test]
    fn a_toast_deck_is_bounded_and_evicts_the_oldest() {
        let mut deck = ToastDeck::new(2);
        deck.push(1, "a", Attention::NeedsInput);
        deck.push(2, "b", Attention::Ready);
        deck.push(3, "c", Attention::Failed);
        assert_eq!(deck.len(), 2, "capacity is never exceeded");
        let indices: Vec<u16> = deck.toasts().map(|toast| toast.index).collect();
        assert_eq!(indices, vec![2, 3], "the oldest was dropped");
    }

    #[test]
    fn a_repeat_toast_coalesces_and_moves_to_the_newest() {
        let mut deck = ToastDeck::new(3);
        deck.push(1, "a", Attention::NeedsInput);
        deck.push(2, "b", Attention::Ready);
        deck.push(1, "a", Attention::Failed);
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
        deck.push(1, "a", Attention::NeedsInput);
        assert_eq!(deck.len(), 1);
    }

    #[test]
    fn dismissing_removes_a_panes_toast() {
        let mut deck = ToastDeck::new(3);
        deck.push(1, "a", Attention::NeedsInput);
        deck.push(2, "b", Attention::Ready);
        deck.dismiss(1);
        let indices: Vec<u16> = deck.toasts().map(|toast| toast.index).collect();
        assert_eq!(indices, vec![2]);
    }

    #[test]
    fn a_toast_line_carries_text_glyph_colour_and_a_repeat_count() {
        let toast = Toast {
            index: 2,
            title: "codex".into(),
            attention: Attention::NeedsInput,
            repeats: 3,
        };
        let cells = toast_cells(&toast, 40);
        let text = text_of(&cells);
        assert_eq!(text, "codex ! needs input (x3)");
        assert_eq!(fg_of(&cells, '!'), Attention::NeedsInput.color());
    }

    #[test]
    fn a_single_toast_omits_the_repeat_count() {
        let toast = Toast {
            index: 1,
            title: "sh".into(),
            attention: Attention::Ready,
            repeats: 1,
        };
        assert_eq!(text_of(&toast_cells(&toast, 40)), "sh + ready");
    }

    #[test]
    fn a_toast_is_truncated_to_width_rather_than_padded() {
        let toast = Toast {
            index: 1,
            title: "sh".into(),
            attention: Attention::Ready,
            repeats: 1,
        };
        assert_eq!(toast_cells(&toast, 4).len(), 4);
        assert!(toast_cells(&toast, 0).is_empty());
    }
}
