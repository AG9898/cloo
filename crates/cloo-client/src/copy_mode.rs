//! Client-side rendering of server-owned copy mode, and the explicit copy.
//!
//! Copy mode lives in the session actor, because scrollback does. Everything
//! here is the other half: a pure projection of the [`CopyModeState`] the
//! server sends into highlight [`Span`]s and one status row, plus the explicit,
//! policy-gated OSC 52 copy.
//!
//! Two rules shape the module.
//!
//! - **A selection is a rendition, never a cell.** Highlights are built as
//!   positioned spans that read the client's [`Grid`] and leave it exactly as it
//!   was. The grid is a cache of the server's authoritative cells, so a
//!   selection that wrote into it would make the next damage frame disagree with
//!   the server about what a pane says.
//! - **A copy is explicit, and the client is the last gate.** Selected text is
//!   server-owned history, so it crosses the wire only when the user asks. It
//!   comes back as a typed [`OuterTerminalEffect::ClipboardStore`], and a client
//!   whose policy or terminal cannot store a clipboard neither writes anything
//!   nor asks for the text in the first place.
//!
//! Positions from the server are absolute in retained scrollback, while a client
//! holds only the visible grid. [`CopyModeState::viewport_top`] is the one
//! number that joins them, so a highlight is placed against the viewport the
//! server described rather than one the client guessed.
//!
//! ```
//! use cloo_client::copy_mode::{Highlight, highlight_spans};
//! use cloo_client::renderer::Grid;
//! use cloo_client::theme::Theme;
//! use cloo_proto::{CopyModeState, CopySelection, PaneId, Point, ScrollPoint, Size};
//!
//! let grid = Grid::new(Size::new(8, 2));
//! let state = CopyModeState {
//!     pane: PaneId::new(1),
//!     viewport_top: 10,
//!     cursor: ScrollPoint::new(10, 2),
//!     selection: Some(CopySelection {
//!         anchor: ScrollPoint::new(10, 0),
//!         head: ScrollPoint::new(10, 2),
//!     }),
//!     query: None,
//!     matches: Vec::new(),
//! };
//! let spans = highlight_spans(Point::new(0, 0), &grid, &state, Theme::storm());
//! assert_eq!(spans.len(), 1);
//! assert_eq!(spans[0].cells.len(), 3);
//! ```

use std::io::{self, Write};

use cloo_proto::{
    Action, Cell, CellAttrs, ClientMessage, ClipboardTarget, Color, CopyModeState,
    OuterTerminalEffect, Point, ScrollPoint, TermCaps,
};

use crate::effects::{EffectPolicy, apply_effect};
use crate::renderer::{Grid, Span};
use crate::theme::{Theme, ThemeToken};

// ---------------------------------------------------------------------------
// Highlights
// ---------------------------------------------------------------------------

/// What a highlighted cell takes part in.
///
/// Ordered by precedence: one cell can be a search match inside a selection
/// under the copy cursor, and the strongest role is the one drawn. Each role
/// differs from the others in *attributes* as well as colour, so a terminal
/// without a usable palette still tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Highlight {
    /// A result of the active regex search.
    Match,
    /// Part of the live visual selection.
    Selection,
    /// The copy cursor itself.
    Cursor,
}

impl Highlight {
    /// Every role, weakest first.
    pub const ALL: [Self; 3] = [Self::Match, Self::Selection, Self::Cursor];

    /// The role's name, for a status row or a test.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Selection => "selection",
            Self::Cursor => "cursor",
        }
    }

    /// Applies the role's rendition to one cached cell.
    ///
    /// The character is kept exactly as the server sent it — a highlight is a
    /// rendition and never a substitution. The cursor deliberately reverses the
    /// selection's own colours, so it stays visible inside a selected run
    /// whether or not the terminal draws colour.
    #[must_use]
    pub fn apply(self, cell: Cell, theme: Theme) -> Cell {
        let frame = theme.color(ThemeToken::Frame);
        let (fg, bg, attrs) = match self {
            Self::Match => (frame, theme.color(ThemeToken::Info), CellAttrs::UNDERLINE),
            Self::Selection => (frame, theme.color(ThemeToken::Accent), CellAttrs::NONE),
            Self::Cursor => (
                frame,
                theme.color(ThemeToken::Accent),
                CellAttrs::REVERSE.union(CellAttrs::BOLD),
            ),
        };
        Cell {
            ch: cell.ch,
            fg,
            bg,
            attrs: cell.attrs.union(attrs),
        }
    }
}

/// Where a retained-scrollback line sits in the visible grid.
///
/// `None` means the line is scrolled out of the pane, which is the ordinary
/// case for a selection that reaches above the viewport. A client must never
/// clamp such a line onto the nearest visible row: that would highlight text
/// the user did not select.
#[must_use]
pub fn viewport_row(state: &CopyModeState, line: u32, rows: u16) -> Option<u16> {
    let offset = line.checked_sub(state.viewport_top)?;
    let row = u16::try_from(offset).ok()?;
    (row < rows).then_some(row)
}

/// Builds the highlight spans for one pane, reading its grid and never
/// changing it.
///
/// `at` is the pane body's first cell in outer-terminal coordinates, so a
/// caller that draws a header above the body passes the row below it. The
/// returned spans are ready for
/// [`Renderer::render_spans`](crate::renderer::Renderer::render_spans), and a
/// row with nothing highlighted produces no span at all.
#[must_use]
pub fn highlight_spans(at: Point, grid: &Grid, state: &CopyModeState, theme: Theme) -> Vec<Span> {
    let size = grid.size();
    if size.rows == 0 || size.cols == 0 {
        return Vec::new();
    }

    let mut spans = Vec::new();
    for row in 0..size.rows {
        let Some(cells) = grid.row(row) else {
            continue;
        };
        let roles = row_roles(state, row, size.cols);
        for (start, run) in runs(&roles) {
            let painted = run
                .iter()
                .zip(cells.iter().skip(usize::from(start)))
                .map(|(role, cell)| role.apply(*cell, theme))
                .collect();
            spans.push(Span::new(
                Point::new(at.col.saturating_add(start), at.row.saturating_add(row)),
                painted,
            ));
        }
    }
    spans
}

/// The strongest role claimed by each column of one visible row.
///
/// Every comparison is made in retained-scrollback coordinates, which is why a
/// selection whose ends are both off screen still highlights the rows between
/// them: the row's own line is inside the range even though neither end is.
fn row_roles(state: &CopyModeState, row: u16, cols: u16) -> Vec<Option<Highlight>> {
    let mut roles = vec![None; usize::from(cols)];
    let line = state.viewport_top.saturating_add(u32::from(row));
    let last_column = cols.saturating_sub(1);
    let mut claim = |from: u16, to: u16, role: Highlight| {
        for column in from..=to.min(last_column) {
            let slot = &mut roles[usize::from(column)];
            if slot.is_none_or(|current| current < role) {
                *slot = Some(role);
            }
        }
    };

    for matched in &state.matches {
        // A match never spans two terminal lines: the server searches line by
        // line, because a soft wrap is a rendering detail and not text.
        if matched.start.line != line {
            continue;
        }
        let Some(end) = matched.end.column.checked_sub(1) else {
            continue;
        };
        if matched.start.column <= end {
            claim(matched.start.column, end, Highlight::Match);
        }
    }

    if let Some(selection) = state.selection {
        let (start, end) = ordered(selection.anchor, selection.head);
        if (start.line..=end.line).contains(&line) {
            let from = if line == start.line { start.column } else { 0 };
            let to = if line == end.line {
                end.column
            } else {
                last_column
            };
            if from <= to {
                claim(from, to, Highlight::Selection);
            }
        }
    }

    if state.cursor.line == line && state.cursor.column < cols {
        claim(state.cursor.column, state.cursor.column, Highlight::Cursor);
    }

    roles
}

/// Splits a row's roles into contiguous runs, each with its starting column.
fn runs(roles: &[Option<Highlight>]) -> Vec<(u16, Vec<Highlight>)> {
    let mut runs = Vec::new();
    let mut current: Option<(u16, Vec<Highlight>)> = None;
    for (column, role) in roles.iter().enumerate() {
        let column = u16::try_from(column).unwrap_or(u16::MAX);
        match (role, current.as_mut()) {
            (Some(role), Some((_, run))) => run.push(*role),
            (Some(role), None) => current = Some((column, vec![*role])),
            (None, _) => {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
            }
        }
    }
    if let Some(run) = current.take() {
        runs.push(run);
    }
    runs
}

/// The two ends of a selection in retained-scrollback order.
fn ordered(anchor: ScrollPoint, head: ScrollPoint) -> (ScrollPoint, ScrollPoint) {
    if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    }
}

// ---------------------------------------------------------------------------
// Status row
// ---------------------------------------------------------------------------

/// Builds the copy-mode status row, exactly `width` cells wide.
///
/// Width is spent in one fixed order, exactly as a pane header spends it, so
/// two panes of the same width never disagree about what a copy-mode row says.
/// The mode label is what the row *is* and is the last thing standing; the
/// match count goes first when space runs out, then the query, then the
/// selection marker, and only then the cursor position.
#[must_use]
pub fn status_cells(state: &CopyModeState, width: u16, theme: Theme) -> Vec<Cell> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let label = "COPY";
    let position = format!(" {}:{}", state.cursor.line, state.cursor.column);
    let selection = if state.selection.is_some() {
        " SEL"
    } else {
        ""
    };
    let query = state
        .query
        .as_deref()
        .filter(|query| !query.is_empty())
        .map(|query| format!(" /{query}"))
        .unwrap_or_default();
    let count = if state.query.is_some() {
        format!(" {} matches", state.matches.len())
    } else {
        String::new()
    };

    let mut segments: Vec<(&str, Color, CellAttrs)> =
        vec![(label, theme.color(ThemeToken::Accent), CellAttrs::BOLD)];
    let mut used = len(label);
    let spend = |text: &str, used: &mut usize| {
        if !text.is_empty() && *used + len(text) <= width {
            *used += len(text);
            true
        } else {
            false
        }
    };
    if spend(&position, &mut used) {
        segments.push((
            position.as_str(),
            theme.color(ThemeToken::Muted),
            CellAttrs::NONE,
        ));
    }
    if spend(selection, &mut used) {
        segments.push((selection, theme.color(ThemeToken::Accent), CellAttrs::NONE));
    }
    if spend(&query, &mut used) {
        segments.push((
            query.as_str(),
            theme.color(ThemeToken::Info),
            CellAttrs::NONE,
        ));
    }
    if spend(&count, &mut used) {
        segments.push((
            count.as_str(),
            theme.color(ThemeToken::Muted),
            CellAttrs::NONE,
        ));
    }

    let surface = theme.color(ThemeToken::Surface);
    let mut cells = Vec::with_capacity(width);
    for (text, fg, attrs) in segments {
        // Only the mandatory label can exceed the row, and only on a pane too
        // narrow to hold four characters.
        for ch in truncate(text, width.saturating_sub(cells.len())).chars() {
            cells.push(Cell {
                ch,
                fg,
                bg: surface,
                attrs,
            });
        }
    }
    while cells.len() < width {
        cells.push(Cell {
            ch: ' ',
            fg: theme.color(ThemeToken::DefaultText),
            bg: surface,
            attrs: CellAttrs::NONE,
        });
    }
    cells
}

/// Builds the copy-mode status row as a positioned span.
#[must_use]
pub fn status_span(at: Point, state: &CopyModeState, width: u16, theme: Theme) -> Span {
    Span::new(at, status_cells(state, width, theme))
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
// The explicit copy
// ---------------------------------------------------------------------------

/// The request that asks the server for the current selection as a clipboard
/// effect.
///
/// `None` when this client could not act on the answer: with clipboard policy
/// denied, or a terminal that never reported OSC 52, a client does not make the
/// server put a user's scrollback on the wire for it to throw away.
#[must_use]
pub fn copy_request(
    caps: TermCaps,
    policy: EffectPolicy,
    target: ClipboardTarget,
) -> Option<ClientMessage> {
    policy
        .permits_clipboard(caps)
        .then_some(ClientMessage::Command(Action::CopySelection(target)))
}

/// Applies the clipboard effect returned for an explicit copy.
///
/// Returns `Ok(true)` when the store reached the terminal and `Ok(false)` when
/// it did not: local policy, capabilities, or an effect that is not a clipboard
/// store at all. The false case writes nothing, so a denied copy leaves both the
/// terminal and the rendered frame exactly as they were.
///
/// # Errors
///
/// Returns the output writer's error once a permitted store begins writing.
pub fn apply_copy<W: Write>(
    output: &mut W,
    caps: TermCaps,
    policy: EffectPolicy,
    effect: &OuterTerminalEffect,
) -> io::Result<bool> {
    // An explicit copy accepts only the answer it asked for. A title change
    // arriving on this path is a pane's request, not the user's copy, and it
    // goes through the ordinary effect route with its own policy bit.
    if !matches!(effect, OuterTerminalEffect::ClipboardStore { .. }) {
        return Ok(false);
    }
    apply_effect(output, caps, policy, effect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_proto::{CopySelection, PaneId, RowUpdate, SearchMatch, Size};

    fn grid_of(rows: &[&str]) -> Grid {
        let cols = u16::try_from(
            rows.iter()
                .map(|row| row.chars().count())
                .max()
                .unwrap_or(0),
        )
        .expect("test rows are small");
        let mut grid = Grid::new(Size::new(
            cols,
            u16::try_from(rows.len()).expect("test rows are small"),
        ));
        for (index, text) in rows.iter().enumerate() {
            let mut cells: Vec<Cell> = text
                .chars()
                .map(|ch| Cell {
                    ch,
                    ..Cell::default()
                })
                .collect();
            cells.resize(usize::from(cols), Cell::default());
            grid.apply(&RowUpdate {
                row: u16::try_from(index).expect("test rows are small"),
                cells,
            })
            .expect("the fixture row matches the grid");
        }
        grid
    }

    fn state() -> CopyModeState {
        CopyModeState {
            pane: PaneId::new(1),
            viewport_top: 100,
            cursor: ScrollPoint::new(100, 0),
            selection: None,
            query: None,
            matches: Vec::new(),
        }
    }

    /// The character and role of every highlighted cell, by row.
    fn painted(spans: &[Span]) -> Vec<(u16, u16, String)> {
        spans
            .iter()
            .map(|span| {
                (
                    span.at.row,
                    span.at.col,
                    span.cells.iter().map(|cell| cell.ch).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_selection_paints_spans_and_leaves_the_cached_grid_untouched() {
        let grid = grid_of(&["alpha", "beta ", "gamma"]);
        let before = grid.clone();
        let mut state = state();
        state.selection = Some(CopySelection {
            anchor: ScrollPoint::new(100, 2),
            head: ScrollPoint::new(102, 1),
        });
        state.cursor = ScrollPoint::new(102, 1);

        // The pane body starts at column 2 of terminal row 4.
        let spans = highlight_spans(Point::new(2, 4), &grid, &state, Theme::storm());

        // Three visible rows: the tail of the first, all of the second, and the
        // head of the third — carrying the grid's own characters.
        assert_eq!(
            painted(&spans),
            vec![
                (4, 4, "pha".to_owned()),
                (5, 2, "beta ".to_owned()),
                (6, 2, "ga".to_owned()),
            ]
        );
        assert_eq!(grid, before, "a selection must not mutate the grid cache");
    }

    #[test]
    fn roles_have_a_fixed_precedence_and_stay_distinct_without_colour() {
        let grid = grid_of(&["error here"]);
        let mut state = state();
        state.query = Some("error".into());
        state.matches = vec![SearchMatch {
            start: ScrollPoint::new(100, 0),
            end: ScrollPoint::new(100, 5),
        }];
        state.selection = Some(CopySelection {
            anchor: ScrollPoint::new(100, 2),
            head: ScrollPoint::new(100, 6),
        });
        state.cursor = ScrollPoint::new(100, 6);

        let spans = highlight_spans(Point::new(0, 0), &grid, &state, Theme::storm());
        let cells = &spans[0].cells;
        assert_eq!(spans.len(), 1);
        assert_eq!(cells.len(), 7);

        // Columns 0-1 are match only, 2-4 are selection over match, and 6 is
        // the cursor. Each role differs from its neighbour by an attribute as
        // well as a colour, so the three stay apart on a monochrome terminal.
        assert!(cells[0].attrs.contains(CellAttrs::UNDERLINE));
        assert!(!cells[0].attrs.contains(CellAttrs::REVERSE));
        assert!(!cells[2].attrs.contains(CellAttrs::UNDERLINE));
        assert!(!cells[2].attrs.contains(CellAttrs::REVERSE));
        assert_ne!(cells[0].bg, cells[2].bg);
        assert!(cells[6].attrs.contains(CellAttrs::REVERSE));
        assert_eq!(cells[6].bg, cells[2].bg, "the cursor inverts its selection");
        assert!(Highlight::Match < Highlight::Selection);
        assert!(Highlight::Selection < Highlight::Cursor);
    }

    #[test]
    fn positions_above_or_below_the_viewport_are_never_clamped_into_it() {
        let grid = grid_of(&["one", "two"]);
        let mut state = state();
        // A selection that began far above the viewport still highlights every
        // visible row, while a match on a scrolled-out line highlights nothing.
        state.selection = Some(CopySelection {
            anchor: ScrollPoint::new(3, 1),
            head: ScrollPoint::new(200, 2),
        });
        state.cursor = ScrollPoint::new(200, 2);
        state.matches = vec![SearchMatch {
            start: ScrollPoint::new(4, 0),
            end: ScrollPoint::new(4, 2),
        }];

        let spans = highlight_spans(Point::new(0, 0), &grid, &state, Theme::storm());
        assert_eq!(
            painted(&spans),
            vec![(0, 0, "one".to_owned()), (1, 0, "two".to_owned())]
        );
        assert_eq!(viewport_row(&state, 99, 2), None);
        assert_eq!(viewport_row(&state, 100, 2), Some(0));
        assert_eq!(viewport_row(&state, 102, 2), None);
    }

    #[test]
    fn the_status_row_is_exactly_its_width_at_every_width() {
        let mut state = state();
        state.cursor = ScrollPoint::new(1234, 7);
        state.selection = Some(CopySelection {
            anchor: ScrollPoint::new(1234, 0),
            head: ScrollPoint::new(1234, 7),
        });
        state.query = Some("retry".into());
        state.matches = vec![SearchMatch {
            start: ScrollPoint::new(1234, 0),
            end: ScrollPoint::new(1234, 5),
        }];

        for width in 0..60u16 {
            let row = status_cells(&state, width, Theme::storm());
            assert_eq!(
                row.len(),
                usize::from(width),
                "a copy-mode row must be exactly {width} cells"
            );
        }

        let text: String = status_cells(&state, 32, Theme::storm())
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(text, "COPY 1234:7 SEL /retry 1 matches");

        // Width is spent in one fixed order: the count goes before the query,
        // the query before the selection marker, and the label stands last.
        let narrow: String = status_cells(&state, 22, Theme::storm())
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(narrow, "COPY 1234:7 SEL /retry");
        let narrower: String = status_cells(&state, 15, Theme::storm())
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(narrower, "COPY 1234:7 SEL");
        let tiny: String = status_cells(&state, 3, Theme::storm())
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(tiny, "COP");
    }

    #[test]
    fn a_denied_clipboard_neither_writes_nor_asks_for_the_text() {
        let store = OuterTerminalEffect::ClipboardStore {
            target: ClipboardTarget::Clipboard,
            text: "selected".into(),
        };
        let capable = TermCaps {
            clipboard_osc52: true,
            ..TermCaps::default()
        };

        // Policy denied: nothing is written and nothing is requested, so the
        // user's scrollback never crosses the wire for this client at all.
        let mut terminal = b"rendered frame".to_vec();
        let before = terminal.clone();
        assert!(
            !apply_copy(&mut terminal, capable, EffectPolicy::default(), &store,)
                .expect("a denied copy does not write")
        );
        assert_eq!(terminal, before);
        assert_eq!(
            copy_request(capable, EffectPolicy::default(), ClipboardTarget::Clipboard),
            None
        );

        // Capability denied: same answer, from the other gate.
        let mut terminal = Vec::new();
        assert!(
            !apply_copy(
                &mut terminal,
                TermCaps::default(),
                EffectPolicy::allow_supported(),
                &store,
            )
            .expect("an incapable terminal is not written to")
        );
        assert!(terminal.is_empty());
        assert_eq!(
            copy_request(
                TermCaps::default(),
                EffectPolicy::allow_supported(),
                ClipboardTarget::Clipboard
            ),
            None
        );
    }

    #[test]
    fn a_permitted_copy_writes_one_osc_52_store_and_nothing_else() {
        let capable = TermCaps {
            clipboard_osc52: true,
            ..TermCaps::default()
        };
        let policy = EffectPolicy::allow_supported();
        let mut terminal = Vec::new();
        assert!(
            apply_copy(
                &mut terminal,
                capable,
                policy,
                &OuterTerminalEffect::ClipboardStore {
                    target: ClipboardTarget::PrimarySelection,
                    text: "hi".into(),
                },
            )
            .expect("the in-memory terminal accepts one store")
        );
        assert_eq!(terminal, b"\x1b]52;p;aGk=\x1b\\");
        assert_eq!(
            copy_request(capable, policy, ClipboardTarget::PrimarySelection),
            Some(ClientMessage::Command(Action::CopySelection(
                ClipboardTarget::PrimarySelection
            )))
        );

        // The explicit copy path accepts only the answer it asked for: a title
        // change is a pane's request and has its own policy bit.
        let mut terminal = Vec::new();
        assert!(
            !apply_copy(
                &mut terminal,
                capable,
                policy,
                &OuterTerminalEffect::SetTitle("agent".into()),
            )
            .expect("a non-clipboard effect is not written on the copy path")
        );
        assert!(terminal.is_empty());
    }
}
