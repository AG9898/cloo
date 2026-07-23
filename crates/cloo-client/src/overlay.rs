//! Keyboard-first overlays: the session switcher, the profile launcher, and
//! pane details.
//!
//! `docs/STYLEGUIDE.md` gives every overlay one language — dim the background,
//! keep a clear selected row, show keyboard hints, dismiss with Escape — so this
//! module is one model and one renderer rather than three of each. An overlay is
//! a list, a cursor into it, and a title; what differs between the three is what
//! a row says and what confirming one *means*.
//!
//! Three rules are load-bearing.
//!
//! - **The keyboard owns an open overlay.** [`OverlayAction`] is cloo's own
//!   vocabulary, decoded by [`crate::input::overlay_action`], and none of it
//!   reaches a child — exactly as chrome owns a mouse click over a border.
//! - **Every overlay is dismissible from every state.** [`OverlayAction::Dismiss`]
//!   answers [`OverlayOutcome::Dismissed`] whatever the list holds, including an
//!   empty one, so an overlay can never trap the terminal.
//! - **A launch names a profile, and only a profile.** A launcher row is built
//!   from a validated [`Profile`] and from nothing else, and confirming one
//!   yields a [`LaunchRequest`] carrying that profile's ID. There is no
//!   free-text command field to type into, which is what makes "explicit
//!   profiles only" a fact about the types rather than a rule someone remembers.
//!
//! Like [`crate::chrome`], everything here is a pure function into [`Cell`]s:
//! nothing writes to a descriptor, so a row is testable against an exact string
//! and [`crate::renderer`] stays the only place bytes are produced.
//!
//! ```
//! use cloo_client::input::OverlayAction;
//! use cloo_client::overlay::{Overlay, OverlayOutcome};
//! use cloo_core::Profile;
//!
//! let mut launcher = Overlay::launcher(&Profile::built_ins());
//! launcher.apply(OverlayAction::Next);
//! let OverlayOutcome::Launch(request) = launcher.apply(OverlayAction::Confirm) else {
//!     panic!("confirming a launcher row launches its profile");
//! };
//! assert_eq!(request.profile().as_str(), "codex");
//! ```

use cloo_core::{Profile, ProfileCommand, ProfileId};
use cloo_proto::{Cell, CellAttrs, Color, PaneId, PaneInfo, Point, SessionId, Size};

use crate::chrome::{Attention, dim_cell_with_theme};
use crate::input::OverlayAction;
use crate::renderer::Span;
use crate::theme::{Theme, ThemeToken};

/// The marker on the row the keyboard cursor is on.
///
/// Text, not only an accent: the style guide's "colour is never the only
/// signal" applies to a selected overlay row exactly as it applies to the
/// active tab's `>` and a focused pane's marker.
const SELECTED_MARKER: &str = "> ";
/// The same width, unmarked, so a row never shifts as the cursor moves.
const PLAIN_MARKER: &str = "  ";

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// One session the switcher can jump to.
///
/// Client-side view state assembled from whatever the daemon reported; it is
/// never authoritative and never inferred from a grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// Which session.
    pub session: SessionId,
    /// The session's user-visible name.
    pub title: String,
    /// How many panes it holds.
    pub panes: usize,
    /// Whether this client is currently attached to it.
    pub attached: bool,
}

impl SessionEntry {
    /// Describes one session for the switcher.
    #[must_use]
    pub fn new(session: SessionId, title: impl Into<String>, panes: usize) -> Self {
        Self {
            session,
            title: title.into(),
            panes,
            attached: false,
        }
    }

    /// Marks the session this client is attached to.
    #[must_use]
    pub const fn attached(mut self, attached: bool) -> Self {
        self.attached = attached;
        self
    }
}

/// One profile the launcher can start a pane from.
///
/// Constructed from a [`Profile`] and from nothing else — there is deliberately
/// no constructor taking a command line, a program name, or a title. A launcher
/// row therefore always corresponds to a profile the configuration actually
/// defines, which is what "launch uses explicit profiles only" means in
/// practice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    profile: ProfileId,
    default_name: String,
    command: String,
}

impl ProfileEntry {
    /// Describes one profile for the launcher.
    ///
    /// `None` when the profile's own [`Profile::validate`] refuses it: a row the
    /// user can select must name something the server could actually launch, and
    /// offering an unlaunchable one turns a configuration warning into a
    /// mysterious failure at the moment of use.
    #[must_use]
    pub fn new(profile: &Profile) -> Option<Self> {
        profile.validate().ok()?;
        Some(Self {
            profile: profile.id.clone(),
            default_name: profile.default_name.clone(),
            command: command_summary(&profile.command),
        })
    }

    /// The profile's ID, which is what the user types and what a launch names.
    #[must_use]
    pub const fn profile(&self) -> &ProfileId {
        &self.profile
    }

    /// The pane name this profile gives a pane the user does not name.
    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.default_name
    }

    /// A one-line rendering of what the profile runs.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }
}

/// What a profile launches, as one line of muted text.
fn command_summary(command: &ProfileCommand) -> String {
    match command {
        ProfileCommand::LoginShell => "login shell".to_owned(),
        ProfileCommand::Program { program, args } if args.is_empty() => program.clone(),
        ProfileCommand::Program { program, args } => format!("{program} {}", args.join(" ")),
    }
}

/// Everything the pane-details overlay shows about one pane.
///
/// Assembled from the [`PaneInfo`] the server sent plus the attention state it
/// reported. Nothing here is derived from the pane's output: a details view that
/// guessed at a task or a state would be the screen-scraping the whole attention
/// contract exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDetails {
    /// Which pane.
    pub pane: PaneId,
    /// The ID of the profile it was launched from.
    pub profile: String,
    /// Its user-visible name.
    pub name: String,
    /// What the user said it is for, if they said.
    pub task: Option<String>,
    /// The absolute directory its child was launched in.
    pub cwd: String,
    /// Its reported workspace state.
    pub attention: Attention,
}

impl PaneDetails {
    /// Describes a pane from what the server reported about it.
    #[must_use]
    pub fn from_info(info: &PaneInfo, attention: Attention) -> Self {
        Self {
            pane: info.pane,
            profile: info.profile.clone(),
            name: info.name.clone(),
            task: info.task.clone(),
            cwd: info.cwd.clone(),
            attention,
        }
    }

    /// The labelled fields, in display order.
    ///
    /// A task the user never set is absent rather than blank: the row would
    /// otherwise read as a task cloo failed to show.
    #[must_use]
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("pane", self.pane.get().to_string()),
            ("profile", self.profile.clone()),
            ("name", self.name.clone()),
        ];
        if let Some(task) = &self.task {
            fields.push(("task", task.clone()));
        }
        fields.push(("cwd", self.cwd.clone()));
        fields.push((
            "state",
            format!("{} {}", self.attention.glyph(), self.attention.label()),
        ));
        fields
    }
}

// ---------------------------------------------------------------------------
// The overlay
// ---------------------------------------------------------------------------

/// Which overlay is open, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKind {
    /// The session switcher.
    Sessions(Vec<SessionEntry>),
    /// The profile launcher.
    Launcher(Vec<ProfileEntry>),
    /// The pane-details view.
    Details(PaneDetails),
}

/// An open overlay: a list, a keyboard cursor, and what confirming means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    kind: OverlayKind,
    selected: usize,
}

impl Overlay {
    /// Opens the session switcher over a list of sessions.
    #[must_use]
    pub fn sessions(entries: Vec<SessionEntry>) -> Self {
        Self {
            kind: OverlayKind::Sessions(entries),
            selected: 0,
        }
    }

    /// Opens the profile launcher over the configured profiles.
    ///
    /// Every row comes from one of `profiles`; a profile that does not validate
    /// is left out rather than offered and then refused at launch.
    #[must_use]
    pub fn launcher(profiles: &[Profile]) -> Self {
        Self {
            kind: OverlayKind::Launcher(profiles.iter().filter_map(ProfileEntry::new).collect()),
            selected: 0,
        }
    }

    /// Opens the pane-details view.
    #[must_use]
    pub fn details(details: PaneDetails) -> Self {
        Self {
            kind: OverlayKind::Details(details),
            selected: 0,
        }
    }

    /// What this overlay is showing.
    #[must_use]
    pub const fn kind(&self) -> &OverlayKind {
        &self.kind
    }

    /// The overlay's title.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self.kind {
            OverlayKind::Sessions(_) => "sessions",
            OverlayKind::Launcher(_) => "launch profile",
            OverlayKind::Details(_) => "pane details",
        }
    }

    /// How many rows the overlay lists.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.kind {
            OverlayKind::Sessions(entries) => entries.len(),
            OverlayKind::Launcher(entries) => entries.len(),
            OverlayKind::Details(details) => details.fields().len(),
        }
    }

    /// Whether the overlay has nothing to list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where the keyboard cursor is.
    #[must_use]
    pub const fn selection(&self) -> usize {
        self.selected
    }

    /// Moves the cursor one row down, stopping at the last row.
    pub fn select_next(&mut self) {
        if self.selected + 1 < self.len() {
            self.selected += 1;
        }
    }

    /// Moves the cursor one row up, stopping at the first row.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Applies one keyboard action.
    ///
    /// Navigation leaves the overlay [`Open`](OverlayOutcome::Open); only a
    /// confirmation or a dismissal produces anything a caller acts on.
    pub fn apply(&mut self, action: OverlayAction) -> OverlayOutcome {
        match action {
            OverlayAction::Next => {
                self.select_next();
                OverlayOutcome::Open
            }
            OverlayAction::Prev => {
                self.select_prev();
                OverlayOutcome::Open
            }
            OverlayAction::First => {
                self.selected = 0;
                OverlayOutcome::Open
            }
            OverlayAction::Last => {
                self.selected = self.len().saturating_sub(1);
                OverlayOutcome::Open
            }
            OverlayAction::Confirm => self.confirm(),
            OverlayAction::Dismiss => OverlayOutcome::Dismissed,
        }
    }

    /// What confirming the selected row means.
    ///
    /// An empty list confirms to nothing at all — a launcher with no profile
    /// configured must not invent one, and a switcher with no session must not
    /// name one.
    #[must_use]
    pub fn confirm(&self) -> OverlayOutcome {
        match &self.kind {
            OverlayKind::Sessions(entries) => entries
                .get(self.selected)
                .map_or(OverlayOutcome::Open, |entry| {
                    OverlayOutcome::SwitchSession(entry.session)
                }),
            OverlayKind::Launcher(entries) => {
                entries
                    .get(self.selected)
                    .map_or(OverlayOutcome::Open, |entry| {
                        OverlayOutcome::Launch(LaunchRequest {
                            profile: entry.profile.clone(),
                            default_name: entry.default_name.clone(),
                        })
                    })
            }
            // Details is a reading surface: there is nothing to act on, so
            // Enter does the only other thing a user could mean by it.
            OverlayKind::Details(_) => OverlayOutcome::Dismissed,
        }
    }

    /// The rows the overlay would draw into `visible` list rows.
    fn visible_rows(&self, visible: usize, theme: Theme) -> Vec<RowSpec> {
        let (first, last) = window(self.len(), self.selected, visible);
        (first..last)
            .map(|index| self.row(index, index == self.selected, theme))
            .collect()
    }

    /// One row's fields.
    fn row(&self, index: usize, selected: bool, theme: Theme) -> RowSpec {
        let primary = if selected {
            theme.color(ThemeToken::Accent)
        } else {
            theme.color(ThemeToken::Primary)
        };
        let muted = theme.color(ThemeToken::Muted);
        match &self.kind {
            OverlayKind::Sessions(entries) => {
                let entry = &entries[index];
                let mut extras = vec![Field::new(
                    format!("{} panes", entry.panes),
                    muted,
                    CellAttrs::NONE,
                )];
                if entry.attached {
                    extras.push(Field::new(
                        "attached",
                        theme.color(ThemeToken::Success),
                        CellAttrs::NONE,
                    ));
                }
                RowSpec {
                    selected,
                    lead: Field::new(entry.session.get().to_string(), muted, CellAttrs::NONE),
                    title: Field::new(entry.title.clone(), primary, CellAttrs::BOLD),
                    extras,
                }
            }
            OverlayKind::Launcher(entries) => {
                let entry = &entries[index];
                RowSpec {
                    selected,
                    lead: Field::new(entry.profile.as_str(), muted, CellAttrs::NONE),
                    title: Field::new(entry.default_name.clone(), primary, CellAttrs::BOLD),
                    extras: vec![Field::new(entry.command.clone(), muted, CellAttrs::NONE)],
                }
            }
            OverlayKind::Details(details) => {
                let (label, value) = details.fields().swap_remove(index);
                RowSpec {
                    selected,
                    lead: Field::new(label, muted, CellAttrs::NONE),
                    title: Field::new(value, primary, CellAttrs::NONE),
                    extras: Vec::new(),
                }
            }
        }
    }

    /// The keyboard hints, most important first.
    ///
    /// Dismissal leads because it is the one contract every overlay keeps: a
    /// row that has run out of width still tells the user how to get out.
    fn hints(&self) -> [&'static str; 3] {
        match self.kind {
            OverlayKind::Sessions(_) => ["esc close", "enter switch", "j/k move"],
            OverlayKind::Launcher(_) => ["esc close", "enter launch", "j/k move"],
            OverlayKind::Details(_) => ["esc close", "enter close", "j/k move"],
        }
    }
}

/// What a launch names.
///
/// Carries a [`ProfileId`] and never a command, because it can only be built by
/// confirming a launcher row, and a launcher row can only be built from a
/// validated [`Profile`]. A caller therefore has nothing to send but a profile
/// the configuration defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    profile: ProfileId,
    default_name: String,
}

impl LaunchRequest {
    /// The profile to launch.
    #[must_use]
    pub const fn profile(&self) -> &ProfileId {
        &self.profile
    }

    /// The pane name that profile supplies when the user names nothing.
    #[must_use]
    pub fn default_name(&self) -> &str {
        &self.default_name
    }
}

/// What one keyboard action did to an overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    /// Nothing to act on; the overlay stays open.
    Open,
    /// The overlay closed without acting.
    Dismissed,
    /// Attach to this session instead.
    SwitchSession(SessionId),
    /// Launch a pane from this profile.
    Launch(LaunchRequest),
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// One styled run of overlay text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    text: String,
    fg: Color,
    attrs: CellAttrs,
}

impl Field {
    fn new(text: impl Into<String>, fg: Color, attrs: CellAttrs) -> Self {
        Self {
            text: text.into(),
            fg,
            attrs,
        }
    }
}

/// One overlay row, before it is fitted to a width.
///
/// The three parts are the shared degradation ladder: the marker and the lead
/// are what a row *is*, the extras go first when width runs out, and the title
/// truncates only after every extra is gone — the same order a pane header
/// spends its width in, so a narrow overlay degrades like the rest of the
/// chrome instead of inventing its own layout.
struct RowSpec {
    selected: bool,
    lead: Field,
    title: Field,
    extras: Vec<Field>,
}

/// Builds one overlay row, exactly `width` cells wide.
#[must_use]
fn row_cells(row: &RowSpec, width: u16, theme: Theme) -> Vec<Cell> {
    let width = usize::from(width);
    let surface = theme.color(ThemeToken::RaisedSurface);
    let mut cells = Vec::with_capacity(width);
    if width == 0 {
        return cells;
    }

    let marker = if row.selected {
        Field::new(
            SELECTED_MARKER,
            theme.color(ThemeToken::Accent),
            CellAttrs::BOLD,
        )
    } else {
        Field::new(
            PLAIN_MARKER,
            theme.color(ThemeToken::Border),
            CellAttrs::NONE,
        )
    };

    // Spend width in the documented order: drop extras from the end, then
    // truncate the title, and only then let the fixed part run off the row. A
    // row with no lead — the title and hint rows — spends no gap on it, so the
    // marker column still lines up with the list below.
    let gap = usize::from(!row.lead.text.is_empty());
    let fixed = len(&marker.text) + len(&row.lead.text) + gap;
    let mut kept = row.extras.len();
    while kept > 0 && fixed + len(&row.title.text) + extras_len(&row.extras[..kept]) > width {
        kept -= 1;
    }
    let budget = width
        .saturating_sub(fixed + extras_len(&row.extras[..kept]))
        .min(len(&row.title.text));

    let muted = theme.color(ThemeToken::Muted);
    push(&mut cells, &marker, surface);
    push(&mut cells, &row.lead, surface);
    if gap == 1 {
        push_text(&mut cells, " ", muted, surface);
    }
    push_text(
        &mut cells,
        truncate(&row.title.text, budget),
        row.title.fg,
        surface,
    );
    for extra in &row.extras[..kept] {
        push_text(&mut cells, " ", muted, surface);
        push(&mut cells, extra, surface);
    }

    cells.truncate(width);
    pad(&mut cells, width, surface, theme);
    cells
}

/// The overlay's title row, exactly `width` cells wide.
#[must_use]
pub fn title_cells(overlay: &Overlay, width: u16, theme: Theme) -> Vec<Cell> {
    // An empty overlay has no position to report, and a lone `0/0` would be a
    // claim about a list that does not exist.
    let extras = if overlay.is_empty() {
        Vec::new()
    } else {
        vec![Field::new(
            format!("{}/{}", overlay.selection() + 1, overlay.len()),
            theme.color(ThemeToken::Muted),
            CellAttrs::NONE,
        )]
    };
    row_cells(
        &RowSpec {
            selected: false,
            lead: Field::new("", Color::Default, CellAttrs::NONE),
            title: Field::new(
                overlay.title(),
                theme.color(ThemeToken::Accent),
                CellAttrs::BOLD,
            ),
            extras,
        },
        width,
        theme,
    )
}

/// The overlay's keyboard-hint row, exactly `width` cells wide.
///
/// The hints yield from the end, so the dismissal hint is the last thing
/// standing: an overlay that has run out of width still says how to close.
#[must_use]
pub fn hint_cells(overlay: &Overlay, width: u16, theme: Theme) -> Vec<Cell> {
    let hints = overlay.hints();
    let muted = theme.color(ThemeToken::Muted);
    row_cells(
        &RowSpec {
            selected: false,
            lead: Field::new("", Color::Default, CellAttrs::NONE),
            title: Field::new(hints[0], muted, CellAttrs::NONE),
            extras: vec![
                Field::new(hints[1], muted, CellAttrs::NONE),
                Field::new(hints[2], muted, CellAttrs::NONE),
            ],
        },
        width,
        theme,
    )
}

/// Builds the whole overlay box: a title row, its list, and the hint row.
///
/// Exactly `size.rows` rows of exactly `size.cols` cells, so the box can be
/// painted over a screen without measuring it again. A box too short for both
/// chrome rows keeps the title first and the hints second, because a surface the
/// user cannot read the title of and a surface they cannot close are the two
/// failures worth avoiding in that order.
#[must_use]
pub fn overlay_cells(overlay: &Overlay, size: Size, theme: Theme) -> Vec<Vec<Cell>> {
    let rows = usize::from(size.rows);
    let mut out = Vec::with_capacity(rows);
    if rows == 0 || size.cols == 0 {
        return out;
    }

    out.push(title_cells(overlay, size.cols, theme));
    if rows == 1 {
        return out;
    }
    let list = rows - 2;
    for row in overlay.visible_rows(list, theme) {
        out.push(row_cells(&row, size.cols, theme));
    }
    let surface = theme.color(ThemeToken::RaisedSurface);
    while out.len() + 1 < rows {
        let mut blank = Vec::new();
        pad(&mut blank, usize::from(size.cols), surface, theme);
        out.push(blank);
    }
    out.push(hint_cells(overlay, size.cols, theme));
    out
}

/// The overlay box as positioned spans, ready for
/// [`Renderer::render_spans`](crate::renderer::Renderer::render_spans).
///
/// `at` is the box's top-left corner in outer-terminal cells.
#[must_use]
pub fn overlay_spans(at: Point, overlay: &Overlay, size: Size, theme: Theme) -> Vec<Span> {
    overlay_cells(overlay, size, theme)
        .into_iter()
        .enumerate()
        .map(|(offset, cells)| {
            let row = at
                .row
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            Span::new(Point::new(at.col, row), cells)
        })
        .collect()
}

/// Dims one row of the screen an overlay is drawn over.
///
/// The style guide's overlay language starts with "dim the background", and
/// that is the same contrast reduction an unfocused pane takes — so it is the
/// same function, and a terminal-palette theme takes the same `DIM` fallback
/// rather than a guess. A backdrop never changes a character: the text under an
/// overlay is still the user's session.
#[must_use]
pub fn backdrop_cells(cells: &[Cell], theme: Theme) -> Vec<Cell> {
    cells
        .iter()
        .map(|cell| dim_cell_with_theme(*cell, theme))
        .collect()
}

/// The dimmed backdrop as a positioned span.
#[must_use]
pub fn backdrop_span(at: Point, cells: &[Cell], theme: Theme) -> Span {
    Span::new(at, backdrop_cells(cells, theme))
}

/// The slice of a list that keeps the selection visible.
///
/// Pure, so it needs no stored scroll offset: the window starts at the top
/// until the cursor would leave the bottom, and then follows it by one row. Two
/// clients showing the same overlay at the same size therefore show the same
/// rows.
fn window(len: usize, selected: usize, visible: usize) -> (usize, usize) {
    if len == 0 || visible == 0 {
        return (0, 0);
    }
    let first = if selected < visible {
        0
    } else {
        selected + 1 - visible
    };
    (first, (first + visible).min(len))
}

/// The cells one run of extras costs, including its leading gap.
fn extras_len(extras: &[Field]) -> usize {
    extras.iter().map(|extra| 1 + len(&extra.text)).sum()
}

/// Appends a styled field over the overlay surface.
fn push(cells: &mut Vec<Cell>, field: &Field, bg: Color) {
    push_styled(cells, &field.text, field.fg, bg, field.attrs);
}

/// Appends plain text over the overlay surface.
fn push_text(cells: &mut Vec<Cell>, text: &str, fg: Color, bg: Color) {
    push_styled(cells, text, fg, bg, CellAttrs::NONE);
}

fn push_styled(cells: &mut Vec<Cell>, text: &str, fg: Color, bg: Color, attrs: CellAttrs) {
    for ch in text.chars() {
        cells.push(Cell { ch, fg, bg, attrs });
    }
}

/// Fills a row out to `width` with overlay surface.
fn pad(cells: &mut Vec<Cell>, width: usize, surface: Color, theme: Theme) {
    while cells.len() < width {
        cells.push(Cell {
            ch: ' ',
            fg: theme.color(ThemeToken::DefaultText),
            bg: surface,
            attrs: CellAttrs::NONE,
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

    use cloo_core::ProfileCommand;

    fn sessions() -> Overlay {
        Overlay::sessions(vec![
            SessionEntry::new(SessionId::new(7), "main", 3).attached(true),
            SessionEntry::new(SessionId::new(8), "review", 1),
            SessionEntry::new(SessionId::new(9), "scratch", 2),
        ])
    }

    fn launcher() -> Overlay {
        Overlay::launcher(&Profile::built_ins())
    }

    fn details() -> Overlay {
        Overlay::details(PaneDetails::from_info(
            &PaneInfo {
                pane: PaneId::new(4),
                profile: "claude".to_owned(),
                name: "claude".to_owned(),
                task: Some("refactor the layout pass".to_owned()),
                cwd: "/home/dev/cloo".to_owned(),
            },
            Attention::NeedsInput,
        ))
    }

    fn text(cells: &[Cell]) -> String {
        cells.iter().map(|cell| cell.ch).collect()
    }

    // -- dismissal ----------------------------------------------------------

    /// The one contract every overlay keeps, from every state it can be in:
    /// an overlay that could not be closed would hold the terminal hostage.
    #[test]
    fn every_overlay_is_dismissible_including_an_empty_one() {
        let cases: [(&str, Overlay); 5] = [
            ("sessions", sessions()),
            ("launcher", launcher()),
            ("details", details()),
            ("no sessions", Overlay::sessions(Vec::new())),
            ("no profiles", Overlay::launcher(&[])),
        ];
        for (name, mut overlay) in cases {
            assert_eq!(
                overlay.apply(OverlayAction::Dismiss),
                OverlayOutcome::Dismissed,
                "{name}"
            );
        }
    }

    #[test]
    fn escape_is_the_key_that_dismisses() {
        let action = crate::input::overlay_action(b"\x1b").expect("escape is bound");
        assert_eq!(sessions().apply(action), OverlayOutcome::Dismissed);
    }

    // -- navigation ---------------------------------------------------------

    #[test]
    fn navigation_walks_the_list_and_stops_at_both_ends() {
        let mut overlay = sessions();
        assert_eq!(overlay.selection(), 0);
        overlay.apply(OverlayAction::Prev);
        assert_eq!(overlay.selection(), 0, "the top does not wrap");
        for expected in [1, 2, 2] {
            overlay.apply(OverlayAction::Next);
            assert_eq!(overlay.selection(), expected);
        }
        overlay.apply(OverlayAction::First);
        assert_eq!(overlay.selection(), 0);
        overlay.apply(OverlayAction::Last);
        assert_eq!(overlay.selection(), overlay.len() - 1);
    }

    #[test]
    fn an_empty_overlay_has_nowhere_to_navigate_to() {
        let mut overlay = Overlay::launcher(&[]);
        for action in [
            OverlayAction::Next,
            OverlayAction::Prev,
            OverlayAction::First,
            OverlayAction::Last,
        ] {
            assert_eq!(overlay.apply(action), OverlayOutcome::Open, "{action:?}");
            assert_eq!(overlay.selection(), 0);
        }
    }

    // -- confirming ---------------------------------------------------------

    #[test]
    fn confirming_a_session_row_switches_to_that_session() {
        let mut overlay = sessions();
        overlay.apply(OverlayAction::Next);
        assert_eq!(
            overlay.apply(OverlayAction::Confirm),
            OverlayOutcome::SwitchSession(SessionId::new(8))
        );
    }

    #[test]
    fn confirming_a_launcher_row_names_a_profile_that_was_on_the_list() {
        let profiles = Profile::built_ins();
        let mut overlay = Overlay::launcher(&profiles);
        overlay.apply(OverlayAction::Last);
        let OverlayOutcome::Launch(request) = overlay.apply(OverlayAction::Confirm) else {
            panic!("confirming a launcher row launches");
        };
        assert!(
            profiles
                .iter()
                .any(|profile| &profile.id == request.profile()),
            "a launch can only ever name a profile the caller supplied"
        );
        assert_eq!(request.default_name(), "claude");
    }

    /// The honest version of "launch uses explicit profiles only": a profile
    /// the server could not run is not offered, so there is no row whose
    /// confirmation would fail at `execvp`.
    #[test]
    fn a_profile_that_does_not_validate_never_reaches_the_launcher() {
        let bad = Profile::new(
            ProfileId::new("broken").expect("a valid ID"),
            ProfileCommand::program(""),
            "broken",
        );
        assert!(bad.validate().is_err(), "the fixture must really be bad");
        let overlay = Overlay::launcher(&[bad, Profile::generic()]);
        assert_eq!(overlay.len(), 1);
        let OverlayOutcome::Launch(request) = overlay.confirm() else {
            panic!("the good profile is still launchable");
        };
        assert_eq!(request.profile().as_str(), "generic");
    }

    #[test]
    fn an_empty_launcher_confirms_to_nothing_at_all() {
        assert_eq!(
            Overlay::launcher(&[]).confirm(),
            OverlayOutcome::Open,
            "a launcher with no profile configured must not invent one"
        );
        assert_eq!(
            Overlay::sessions(Vec::new()).confirm(),
            OverlayOutcome::Open
        );
    }

    #[test]
    fn confirming_the_details_view_closes_it_because_there_is_nothing_to_act_on() {
        assert_eq!(details().confirm(), OverlayOutcome::Dismissed);
    }

    // -- details ------------------------------------------------------------

    #[test]
    fn details_show_what_the_server_said_and_nothing_it_inferred() {
        let OverlayKind::Details(details) = details().kind().clone() else {
            panic!("expected the details overlay");
        };
        let fields = details.fields();
        let labels: Vec<&str> = fields.iter().map(|(label, _)| *label).collect();
        assert_eq!(labels, ["pane", "profile", "name", "task", "cwd", "state"]);
        assert_eq!(fields[4].1, "/home/dev/cloo");
        assert_eq!(fields[5].1, "! needs input");
    }

    #[test]
    fn a_task_the_user_never_set_is_absent_rather_than_blank() {
        let details = PaneDetails::from_info(
            &PaneInfo {
                pane: PaneId::new(1),
                profile: "generic".to_owned(),
                name: "shell".to_owned(),
                task: None,
                cwd: "/tmp".to_owned(),
            },
            Attention::Unknown,
        );
        assert!(
            !details.fields().iter().any(|(label, _)| *label == "task"),
            "a blank task row reads as a task cloo failed to show"
        );
    }

    // -- rendering ----------------------------------------------------------

    /// The exact-width guarantee, at every width, for every overlay. This loop
    /// — not the pretty cases — is what catches an off-by-one in the gap
    /// arithmetic, exactly as it does for the pane header.
    #[test]
    fn every_overlay_row_is_exactly_the_width_asked_for() {
        let theme = Theme::storm();
        for (name, overlay) in [
            ("sessions", sessions()),
            ("launcher", launcher()),
            ("details", details()),
        ] {
            for width in 0..=60_u16 {
                for (index, row) in overlay_cells(&overlay, Size::new(width, 12), theme)
                    .iter()
                    .enumerate()
                {
                    assert_eq!(
                        row.len(),
                        usize::from(width),
                        "{name} row {index} at width {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_box_is_exactly_as_tall_as_it_was_asked_for() {
        let theme = Theme::storm();
        for rows in 0..=10_u16 {
            let cells = overlay_cells(&sessions(), Size::new(30, rows), theme);
            assert_eq!(cells.len(), usize::from(rows), "at {rows} rows");
        }
    }

    #[test]
    fn a_box_too_short_for_its_list_keeps_the_title_and_the_hints() {
        let theme = Theme::storm();
        let cells = overlay_cells(&sessions(), Size::new(24, 2), theme);
        assert_eq!(text(&cells[0]).trim_end(), "  sessions 1/3");
        assert!(
            text(&cells[1]).contains("esc close"),
            "a surface the user cannot close is the failure worth avoiding"
        );
    }

    #[test]
    fn the_selected_row_is_marked_with_text_and_not_only_a_colour() {
        let theme = Theme::storm();
        let mut overlay = sessions();
        overlay.apply(OverlayAction::Next);
        let rows = overlay_cells(&overlay, Size::new(30, 5), theme);
        assert!(text(&rows[1]).starts_with("  7"), "unselected keeps a gap");
        assert!(
            text(&rows[2]).starts_with("> 8"),
            "the cursor is a glyph, so a monochrome terminal loses nothing"
        );
    }

    #[test]
    fn a_session_row_spends_width_in_the_documented_order() {
        let theme = Theme::storm();
        let overlay = sessions();
        let full = text(&row_cells(&overlay.row(0, false, theme), 32, theme));
        assert_eq!(full, "  7 main 3 panes attached       ");
        // The extras go first, from the end, and only then does the title
        // truncate — the marker and the session ID are what a row is.
        for (width, expected) in [
            (24_u16, "  7 main 3 panes        "),
            (12, "  7 main    "),
            (7, "  7 mai"),
            (5, "  7 m"),
        ] {
            assert_eq!(
                text(&row_cells(&overlay.row(0, false, theme), width, theme)),
                expected,
                "at width {width}"
            );
        }
    }

    #[test]
    fn the_dismissal_hint_is_the_last_thing_standing() {
        let theme = Theme::storm();
        let overlay = launcher();
        assert_eq!(
            text(&hint_cells(&overlay, 34, theme)),
            "  esc close enter launch j/k move "
        );
        assert_eq!(text(&hint_cells(&overlay, 12, theme)), "  esc close ");
    }

    #[test]
    fn the_window_follows_the_cursor_past_the_bottom_of_the_box() {
        let theme = Theme::storm();
        let mut overlay = sessions();
        // A box with room for exactly two list rows.
        let size = Size::new(20, 4);
        assert_eq!(
            text(&overlay_cells(&overlay, size, theme)[1]).trim_end(),
            "> 7 main 3 panes"
        );
        overlay.apply(OverlayAction::Last);
        let rows = overlay_cells(&overlay, size, theme);
        assert_eq!(text(&rows[1]).trim_end(), "  8 review 1 panes");
        assert_eq!(text(&rows[2]).trim_end(), "> 9 scratch 2 panes");
    }

    #[test]
    fn the_overlay_box_is_positioned_where_it_was_asked_for() {
        let spans = overlay_spans(
            Point::new(4, 2),
            &sessions(),
            Size::new(20, 3),
            Theme::storm(),
        );
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].at, Point::new(4, 2));
        assert_eq!(spans[2].at, Point::new(4, 4));
        assert!(spans.iter().all(|span| span.cells.len() == 20));
    }

    #[test]
    fn the_backdrop_dims_the_screen_without_changing_a_character() {
        let theme = Theme::storm();
        let row: Vec<Cell> = "hello"
            .chars()
            .map(|ch| Cell {
                ch,
                fg: Color::Rgb(0xc0, 0xca, 0xf5),
                bg: Color::Rgb(0x1a, 0x1b, 0x26),
                attrs: CellAttrs::NONE,
            })
            .collect();
        let dimmed = backdrop_cells(&row, theme);
        assert_eq!(text(&dimmed), "hello");
        assert_ne!(
            dimmed, row,
            "a backdrop that changed nothing is no backdrop"
        );
    }
}
