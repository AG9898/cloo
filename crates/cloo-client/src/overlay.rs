//! Keyboard-first overlays: the help surface, the session switcher, the profile
//! launcher, the attention queue, and pane details.
//!
//! `docs/STYLEGUIDE.md` gives every overlay one language — dim the background,
//! keep a clear selected row, show keyboard hints, dismiss with Escape — so this
//! module is one model and one renderer rather than five of each. An overlay is
//! a list, a cursor into it, and a title; what differs between them is what a
//! row says and what confirming one *means*.
//!
//! Six rules are load-bearing.
//!
//! - **The keyboard owns an open overlay.** [`OverlayAction`] is cloo's own
//!   vocabulary, decoded by [`crate::input::overlay_action`], and none of it
//!   reaches a child — exactly as chrome owns a mouse click over a border.
//! - **Every overlay is dismissible from every state.** [`OverlayAction::Dismiss`]
//!   answers [`OverlayOutcome::Dismissed`] whatever the list holds, including an
//!   empty one, so an overlay can never trap the terminal.
//! - **The help surface is read from the keymap, never from a list of what the
//!   defaults used to be.** [`Overlay::help`] takes the [`Keymap`] the router is
//!   actually resolving against, so a rebound chord and a rebound prefix are
//!   shown verbatim and an unbound action has no row at all.
//! - **A launch names a profile, and only a profile.** A launcher row is built
//!   from a validated [`Profile`] and from nothing else, and confirming one
//!   yields a [`LaunchRequest`] carrying that profile's ID. There is no
//!   free-text command field to type into, which is what makes "explicit
//!   profiles only" a fact about the types rather than a rule someone remembers.
//! - **A launch the workspace never made is said out loud.** The daemon resolves
//!   an identifier against its own table and refuses an unknown one in silence,
//!   so [`LaunchNotice`] tracks the client's *own* request — never a grid — and
//!   turns a request that produced no pane into a visible refusal.
//! - **Acknowledging a queue row is a session action, not a view flag.** The
//!   server owns [`cloo_proto::PaneAttention::acknowledged`], so
//!   [`OverlayOutcome::Acknowledge`] names a pane for the wire and the row
//!   leaves only when the next attention projection says it has. A locally
//!   dismissed row would be a second source of truth two clients could disagree
//!   about.
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

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use cloo_core::keymap::{Key, Keymap, action_name};
use cloo_core::{Profile, ProfileCommand, ProfileId};
use cloo_proto::{Action, Cell, CellAttrs, Color, PaneId, PaneInfo, Point, SessionId, Size};

use crate::chrome::{Attention, QueueEntry, dim_cell_with_theme};
use crate::input::{OverlayAction, QueueAction};
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

/// One pane waiting on the user, as the attention overlay lists it.
///
/// A [`crate::chrome::QueueEntry`] plus the [`PaneId`] the wire uses. The queue
/// model is keyed by the *position* a user refers to a pane by, which is the
/// right key for coalescing and the wrong one for an action: focus and
/// acknowledgment name a pane on the wire, and a position can quietly become a
/// different pane when one closes. Pairing the two here is what lets a row
/// produce a typed action about the pane the user actually saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionEntry {
    /// The pane the row acts on.
    pub pane: PaneId,
    /// Its position in the tab, as the chrome numbers panes.
    pub index: u16,
    /// Its name.
    pub title: String,
    /// The actionable state that put it in the queue.
    pub attention: Attention,
}

impl AttentionEntry {
    /// Pairs one queue row with the pane it names.
    #[must_use]
    pub fn new(pane: PaneId, entry: &QueueEntry) -> Self {
        Self {
            pane,
            index: entry.index,
            title: entry.title.clone(),
            attention: entry.attention,
        }
    }
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

/// The chord that opens the help surface, when the keymap leaves it free.
pub const HELP_KEY: char = '?';
/// The chord that opens the focused pane's details.
pub const DETAILS_KEY: char = 'i';
/// The chord that opens the session surface.
pub const SESSIONS_KEY: char = 's';
/// The chord that opens the profile launcher.
pub const ADD_PANE_KEY: char = 'a';
/// The chord that opens the attention queue.
///
/// `!` rather than a letter, because it is the glyph the status row's attention
/// count already wears: the key to reach the queue is the mark that says there
/// is something in it.
pub const ATTENTION_KEY: char = '!';

/// The bound actions the help surface lists, in the order it lists them.
///
/// Deliberately a *curated* list rather than every binding in the table: the
/// surface exists so a first-time user can act, and twenty-odd copy motions
/// between `split` and `detach` would bury the controls it is there to teach.
/// Each row's chord is still looked up in the live keymap, so nothing here
/// claims a key the user did not configure.
const HELP_ACTIONS: [(Action, &str); 14] = [
    (Action::SplitVertical, "split right"),
    (Action::SplitHorizontal, "split down"),
    (Action::ClosePane, "close pane"),
    (Action::FocusLeft, "focus left"),
    (Action::FocusDown, "focus down"),
    (Action::FocusUp, "focus up"),
    (Action::FocusRight, "focus right"),
    (Action::ToggleZoom, "zoom pane"),
    (Action::NewTab, "new tab"),
    (Action::NextTab, "next tab"),
    (Action::PrevTab, "previous tab"),
    (Action::CloseTab, "close tab"),
    (Action::EnterCopyMode, "copy mode"),
    (Action::DetachClient, "detach"),
];

/// The client-local surfaces, in the order the help surface lists them.
///
/// `(key, label, note)`. These never cross the wire, so they are not in the
/// keymap — but a user who binds one of these keys to a real action takes it
/// from the client, and the row disappears with it rather than lying.
const HELP_CLIENT_KEYS: [(char, &str, &str); 5] = [
    (ADD_PANE_KEY, "add pane", "client"),
    (SESSIONS_KEY, "sessions", "client"),
    (ATTENTION_KEY, "attention queue", "client"),
    (DETAILS_KEY, "pane details", "client"),
    (HELP_KEY, "this help", "client"),
];

/// One line of the help surface: a chord, what it does, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    key: String,
    label: String,
    note: String,
}

impl HelpEntry {
    /// The chord to press *after* the prefix, spelled as the keymap spells it.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What pressing it does, in the user's words rather than the wire's.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Where the binding comes from: its `[keys]` name, or that it is the
    /// client's own surface.
    #[must_use]
    pub fn note(&self) -> &str {
        &self.note
    }
}

/// The effective key bindings, as one readable surface.
///
/// Built from a [`Keymap`] and from nothing else — there is no constructor
/// taking a hand-written table — which is what makes "the help surface cannot
/// disagree with the router" a fact about the type rather than a rule someone
/// has to remember when they rebind a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpKeys {
    prefix: String,
    title: String,
    entries: Vec<HelpEntry>,
}

impl HelpKeys {
    /// Reads the effective bindings out of `keymap`.
    #[must_use]
    pub fn new(keymap: &Keymap) -> Self {
        let prefix = keymap.prefix().to_string();
        let mut entries = Vec::with_capacity(HELP_ACTIONS.len() + HELP_CLIENT_KEYS.len());
        for (action, label) in HELP_ACTIONS {
            // An action the user unbound has no chord to show, and inventing
            // one would send them pressing a key that does nothing.
            let Some(key) = bound_key(keymap, &action) else {
                continue;
            };
            entries.push(HelpEntry {
                key,
                label: label.to_owned(),
                note: action_name(&action).unwrap_or_default().to_owned(),
            });
        }
        for (key, label, note) in HELP_CLIENT_KEYS {
            if keymap.action(Key::char(key)).is_none() {
                entries.push(HelpEntry {
                    key: key.to_string(),
                    label: label.to_owned(),
                    note: note.to_owned(),
                });
            }
        }
        Self {
            title: format!("keys - prefix {prefix}"),
            prefix,
            entries,
        }
    }

    /// The prefix chord these bindings are reached through, drawn verbatim.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Every listed binding, in display order.
    #[must_use]
    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }
}

/// The first chord bound to `action`, if any.
///
/// First rather than every chord: the defaults bind both `h` and `Left` to
/// `focus-left`, and a help surface that listed each alias would spend four rows
/// saying one thing.
fn bound_key(keymap: &Keymap, action: &Action) -> Option<String> {
    keymap
        .bindings()
        .iter()
        .find(|(_, bound)| bound == action)
        .map(|(key, _)| key.to_string())
}

// ---------------------------------------------------------------------------
// The overlay
// ---------------------------------------------------------------------------

/// Which overlay is open, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayKind {
    /// The effective key bindings.
    Help(HelpKeys),
    /// The session switcher.
    Sessions(Vec<SessionEntry>),
    /// The profile launcher.
    Launcher(Vec<ProfileEntry>),
    /// The attention queue.
    Attention(Vec<AttentionEntry>),
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
    /// Opens the help surface over the keymap the client is resolving against.
    ///
    /// The one surface a user reaches before they know any other key, so it is
    /// read from the live keymap rather than from a written-down copy of the
    /// defaults: a rebound prefix, a rebound chord, and an unbound action are
    /// all shown as they actually are.
    #[must_use]
    pub fn help(keymap: &Keymap) -> Self {
        Self {
            kind: OverlayKind::Help(HelpKeys::new(keymap)),
            selected: 0,
        }
    }

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

    /// Opens the attention queue over the panes currently waiting on the user.
    ///
    /// The rows are the live [`crate::chrome::AttentionQueue`]'s, newest first
    /// and one per pane, paired with the pane each names. An empty queue still
    /// opens: "nothing is waiting" is an answer, and a surface that refused to
    /// appear would be indistinguishable from a key that does nothing.
    #[must_use]
    pub fn attention(entries: Vec<AttentionEntry>) -> Self {
        Self {
            kind: OverlayKind::Attention(entries),
            selected: 0,
        }
    }

    /// Replaces the attention rows with a newer projection, keeping the cursor.
    ///
    /// The queue is the one overlay whose contents change while it is open: the
    /// server owns both the states it lists and the acknowledgment that removes
    /// a row, so an open queue that kept its opening snapshot would show a pane
    /// the user just cleared. The selection is kept by *pane* rather than by
    /// position, so a row disappearing above the cursor does not move it onto a
    /// neighbour the user was not looking at.
    pub fn refresh_attention(&mut self, entries: Vec<AttentionEntry>) {
        let OverlayKind::Attention(current) = &mut self.kind else {
            return;
        };
        let selected = current.get(self.selected).map(|entry| entry.pane);
        *current = entries;
        self.selected = selected
            .and_then(|pane| current.iter().position(|entry| entry.pane == pane))
            .unwrap_or(self.selected);
        let last = self.len().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
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
    ///
    /// Borrowed rather than `'static` because the help surface's title carries
    /// the effective prefix, which is configuration and not a constant.
    #[must_use]
    pub fn title(&self) -> &str {
        match &self.kind {
            OverlayKind::Help(help) => &help.title,
            OverlayKind::Sessions(_) => "sessions",
            OverlayKind::Launcher(_) => "launch profile",
            OverlayKind::Attention(_) => "attention",
            OverlayKind::Details(_) => "pane details",
        }
    }

    /// How many rows the overlay lists.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.kind {
            OverlayKind::Help(help) => help.entries.len(),
            OverlayKind::Sessions(entries) => entries.len(),
            OverlayKind::Launcher(entries) => entries.len(),
            OverlayKind::Attention(entries) => entries.len(),
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

    /// Applies one attention-queue keyboard action.
    ///
    /// The queue keeps its own vocabulary — [`QueueAction`], decoded by
    /// [`crate::input::queue_action`] — because it is the one overlay with a
    /// verb the others do not have: acknowledging a row is not confirming it.
    /// Everything else maps onto the shared model, so navigation and dismissal
    /// behave identically to every other surface.
    pub fn apply_queue(&mut self, action: QueueAction) -> OverlayOutcome {
        match action {
            QueueAction::Next => {
                self.select_next();
                OverlayOutcome::Open
            }
            QueueAction::Prev => {
                self.select_prev();
                OverlayOutcome::Open
            }
            QueueAction::Focus => self.confirm(),
            QueueAction::Acknowledge => match &self.kind {
                OverlayKind::Attention(entries) => entries
                    .get(self.selected)
                    .map_or(OverlayOutcome::Open, |entry| {
                        OverlayOutcome::Acknowledge(entry.pane)
                    }),
                // Nothing else has anything to acknowledge, so the key is spent
                // rather than passed on: an open overlay owns the keyboard.
                _ => OverlayOutcome::Open,
            },
            QueueAction::Dismiss => OverlayOutcome::Dismissed,
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
            // The queue is a navigation surface: confirming a row means going to
            // the pane it names, which is the whole reason the list exists.
            OverlayKind::Attention(entries) => entries
                .get(self.selected)
                .map_or(OverlayOutcome::Open, |entry| {
                    OverlayOutcome::FocusPane(entry.pane)
                }),
            // Details and help are reading surfaces: there is nothing to act
            // on, so Enter does the only other thing a user could mean by it.
            OverlayKind::Help(_) | OverlayKind::Details(_) => OverlayOutcome::Dismissed,
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
            OverlayKind::Help(help) => {
                let entry = &help.entries[index];
                RowSpec {
                    selected,
                    lead: Field::new(
                        entry.key.clone(),
                        theme.color(ThemeToken::Accent),
                        CellAttrs::BOLD,
                    ),
                    title: Field::new(entry.label.clone(), primary, CellAttrs::NONE),
                    extras: vec![Field::new(entry.note.clone(), muted, CellAttrs::NONE)],
                }
            }
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
            // The pane number leads, exactly as it leads a pane header, and the
            // state is the trailing field: glyph *and* label, coloured through
            // the client theme, so the row never rests on colour and reads the
            // same as the header of the pane it names.
            OverlayKind::Attention(entries) => {
                let entry = &entries[index];
                RowSpec {
                    selected,
                    lead: Field::new(entry.index.to_string(), muted, CellAttrs::NONE),
                    title: Field::new(entry.title.clone(), primary, CellAttrs::BOLD),
                    extras: vec![Field::new(
                        format!("{} {}", entry.attention.glyph(), entry.attention.label()),
                        entry.attention.color_in(theme),
                        CellAttrs::NONE,
                    )],
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
            OverlayKind::Help(_) => ["esc close", "enter close", "j/k move"],
            OverlayKind::Sessions(_) => ["esc close", "enter switch", "j/k move"],
            OverlayKind::Launcher(_) => ["esc close", "enter launch", "j/k move"],
            // `a` earns the middle slot over navigation: acknowledging is the
            // verb this surface has that no other overlay does, and a user who
            // cannot find it will never clear the queue.
            OverlayKind::Attention(_) => ["esc close", "enter focus", "a ack"],
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

/// How long a sent launch has to produce its pane before the client says it
/// did not.
///
/// Generous next to a `fork`/`exec` on a loaded machine, and short enough that
/// a refusal does not read as a keystroke cloo simply ignored.
pub const LAUNCH_DEADLINE: Duration = Duration::from_secs(2);

/// How long a refusal stays on the status row once it is shown.
///
/// The style guide's rule for transient notices: long enough to read, and never
/// covering a harness the user is typing into indefinitely.
pub const NOTICE_LINGER: Duration = Duration::from_secs(4);

/// What the client last asked the workspace to launch, and what came of it.
///
/// The daemon resolves a profile identifier against its own table and refuses an
/// unknown one *silently* — no pane, no reply — so a client that showed nothing
/// would leave a confirmed launcher row looking like a key that did nothing.
/// This closes that gap without inventing a wire message and without reading a
/// grid: the notice remembers which panes existed when *this client* sent the
/// request, and a pane it has not seen before carrying that profile is the
/// launch arriving. Silence past [`LAUNCH_DEADLINE`] is the other answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchNotice {
    profile: String,
    /// The panes that already existed when the request went out, so a pane the
    /// user launched earlier from the same profile cannot answer for this one.
    before: BTreeSet<PaneId>,
    /// When the current state stops holding: the launch deadline while waiting,
    /// the linger once refused.
    until: Instant,
    refused: bool,
}

impl LaunchNotice {
    /// Records a launch this client has just sent.
    ///
    /// Takes the [`LaunchRequest`] itself rather than a string, so a notice can
    /// only ever describe a profile a launcher row actually named.
    #[must_use]
    pub fn sent(request: &LaunchRequest, before: BTreeSet<PaneId>, now: Instant) -> Self {
        Self {
            profile: request.profile().as_str().to_owned(),
            before,
            until: now + LAUNCH_DEADLINE,
            refused: false,
        }
    }

    /// The profile the launch named.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Whether the deadline has already turned this into a refusal.
    #[must_use]
    pub const fn refused(&self) -> bool {
        self.refused
    }

    /// Whether `panes` holds the pane this launch asked for.
    ///
    /// A pane the client had not seen when it sent the request, carrying the
    /// profile it named. Both halves matter: without the profile a *concurrent*
    /// split would answer for the launch, and without the "not seen before" set
    /// an existing pane of the same profile would.
    #[must_use]
    pub fn arrived(&self, panes: &[PaneInfo]) -> bool {
        panes
            .iter()
            .any(|info| info.profile == self.profile && !self.before.contains(&info.pane))
    }

    /// Turns a waiting notice whose deadline has passed into a refusal.
    ///
    /// Reports whether the notice now says something different, which is what
    /// decides whether the frame has to be redrawn.
    pub fn settle(&mut self, now: Instant) -> bool {
        if self.refused || now < self.until {
            return false;
        }
        self.refused = true;
        self.until = now + NOTICE_LINGER;
        true
    }

    /// Whether the notice has finished saying what it had to say.
    #[must_use]
    pub fn finished(&self, now: Instant) -> bool {
        self.refused && now >= self.until
    }

    /// The one line the status row draws, in the user's words.
    ///
    /// ASCII throughout, like every other chrome string: the message a user
    /// needs when a launch did not happen is the last one that may depend on a
    /// terminal rendering a glyph.
    #[must_use]
    pub fn text(&self) -> String {
        if self.refused {
            format!("{} did not start", self.profile)
        } else {
            format!("launching {}", self.profile)
        }
    }
}

/// Builds the launch notice row, exactly `width` cells wide.
///
/// Spends width in the same fixed order as an overlay row — the `launch` lead is
/// what the row *is* and the message truncates — so the status row degrades like
/// the rest of the chrome. A refusal is carried by the word as well as the
/// colour, because a 16-colour terminal has to say the same thing.
#[must_use]
pub fn launch_notice_cells(notice: &LaunchNotice, width: u16, theme: Theme) -> Vec<Cell> {
    let tone = if notice.refused() {
        ThemeToken::Warning
    } else {
        ThemeToken::Info
    };
    row_cells(
        &RowSpec {
            selected: false,
            lead: Field::new("launch", theme.color(ThemeToken::Muted), CellAttrs::NONE),
            title: Field::new(notice.text(), theme.color(tone), CellAttrs::BOLD),
            extras: Vec::new(),
        },
        width,
        theme,
    )
}

/// The launch notice as a positioned span.
#[must_use]
pub fn launch_notice_span(at: Point, notice: &LaunchNotice, width: u16, theme: Theme) -> Span {
    Span::new(at, launch_notice_cells(notice, width, theme))
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
    /// Move focus to the pane this row names.
    FocusPane(PaneId),
    /// Mark this pane's attention state as seen.
    ///
    /// Distinct from every other outcome in that the overlay *stays open*: the
    /// row goes away when the server's next attention projection says it has,
    /// which is what keeps acknowledgment single-owned rather than something two
    /// clients could disagree about.
    Acknowledge(PaneId),
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

    use cloo_core::{ProfileCommand, ThemeChoice, ThemeName};
    use cloo_proto::TermCaps;

    fn help() -> Overlay {
        Overlay::help(&Keymap::defaults())
    }

    /// The labels of every row the help surface lists, in order.
    fn help_rows(overlay: &Overlay) -> Vec<(String, String, String)> {
        let OverlayKind::Help(keys) = overlay.kind() else {
            panic!("expected the help overlay");
        };
        keys.entries()
            .iter()
            .map(|entry| {
                (
                    entry.key().to_owned(),
                    entry.label().to_owned(),
                    entry.note().to_owned(),
                )
            })
            .collect()
    }

    /// The chord the help surface shows for `label`, if it shows one at all.
    fn help_key(overlay: &Overlay, label: &str) -> Option<String> {
        help_rows(overlay)
            .into_iter()
            .find(|(_, shown, _)| shown == label)
            .map(|(key, _, _)| key)
    }

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

    /// Two panes waiting, newest first, exactly as the live queue orders them.
    fn attention_rows() -> Vec<AttentionEntry> {
        vec![
            AttentionEntry {
                pane: PaneId::new(4),
                index: 2,
                title: "claude".to_owned(),
                attention: Attention::NeedsInput,
            },
            AttentionEntry {
                pane: PaneId::new(9),
                index: 3,
                title: "build".to_owned(),
                attention: Attention::Failed,
            },
        ]
    }

    fn attention_queue() -> Overlay {
        Overlay::attention(attention_rows())
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
        let cases: [(&str, Overlay); 8] = [
            ("help", help()),
            ("sessions", sessions()),
            ("launcher", launcher()),
            ("attention queue", attention_queue()),
            ("details", details()),
            ("no sessions", Overlay::sessions(Vec::new())),
            ("no profiles", Overlay::launcher(&[])),
            ("nothing waiting", Overlay::attention(Vec::new())),
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

    /// A notice can only ever describe a profile a launcher row named, and it
    /// says both states in words rather than only in colour.
    #[test]
    fn a_launch_notice_says_which_profile_and_which_outcome_in_words() {
        let mut launcher = Overlay::launcher(&Profile::built_ins());
        let OverlayOutcome::Launch(request) = launcher.apply(OverlayAction::Confirm) else {
            panic!("the first row confirms to a launch");
        };
        let sent = Instant::now();
        let mut notice = LaunchNotice::sent(&request, BTreeSet::new(), sent);
        assert_eq!(notice.profile(), "generic");
        assert_eq!(notice.text(), "launching generic");
        assert!(!notice.refused());

        assert!(!notice.settle(sent), "a launch inside its deadline waits");
        assert!(notice.settle(sent + LAUNCH_DEADLINE));
        assert!(notice.refused());
        assert_eq!(notice.text(), "generic did not start");
        assert!(
            !notice.settle(sent + LAUNCH_DEADLINE),
            "a refusal settles once"
        );
        assert!(!notice.finished(sent + LAUNCH_DEADLINE));
        assert!(notice.finished(sent + LAUNCH_DEADLINE + NOTICE_LINGER));
    }

    /// The pane the request asked for is a pane the client had not seen, and
    /// only for the profile it named — the two halves that keep an unrelated
    /// split from answering for a launch.
    #[test]
    fn a_notice_is_only_answered_by_the_pane_its_own_request_produced() {
        let mut launcher = Overlay::launcher(&Profile::built_ins());
        let OverlayOutcome::Launch(request) = launcher.apply(OverlayAction::Confirm) else {
            panic!("the first row confirms to a launch");
        };
        let existing = PaneId::new(1);
        let notice = LaunchNotice::sent(&request, BTreeSet::from([existing]), Instant::now());

        let pane = |pane, profile: &str| PaneInfo {
            pane,
            profile: profile.to_owned(),
            name: "shell".to_owned(),
            task: None,
            cwd: "/".to_owned(),
        };
        assert!(
            !notice.arrived(&[pane(existing, "generic")]),
            "a pane that was already there cannot answer for a new launch"
        );
        assert!(
            !notice.arrived(&[pane(existing, "generic"), pane(PaneId::new(2), "codex")]),
            "a new pane of another profile is somebody else's launch"
        );
        assert!(notice.arrived(&[pane(existing, "generic"), pane(PaneId::new(2), "generic")]));
    }

    /// The notice is chrome like every other row: exactly the width asked for,
    /// at every width, with the message truncating rather than overflowing.
    #[test]
    fn a_launch_notice_row_is_exactly_the_width_asked_for() {
        let mut launcher = Overlay::launcher(&Profile::built_ins());
        let OverlayOutcome::Launch(request) = launcher.apply(OverlayAction::Confirm) else {
            panic!("the first row confirms to a launch");
        };
        let notice = LaunchNotice::sent(&request, BTreeSet::new(), Instant::now());
        for width in 0..=60u16 {
            assert_eq!(
                launch_notice_cells(&notice, width, Theme::storm()).len(),
                usize::from(width),
                "the notice must be exactly {width} cells"
            );
        }
        let drawn: String = launch_notice_cells(&notice, 40, Theme::storm())
            .iter()
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(drawn, "  launch launching generic              ");
    }

    #[test]
    fn confirming_the_details_view_closes_it_because_there_is_nothing_to_act_on() {
        assert_eq!(details().confirm(), OverlayOutcome::Dismissed);
    }

    // -- help ---------------------------------------------------------------

    /// The surface's whole job: a user who knows nothing but `?` can read the
    /// effective prefix and every control the milestone promises off one box.
    #[test]
    fn the_help_surface_names_the_prefix_and_every_promised_control() {
        let overlay = help();
        assert_eq!(overlay.title(), "keys - prefix C-b");
        for (label, key) in [
            ("split right", "%"),
            ("split down", "\""),
            ("focus left", "h"),
            ("focus down", "j"),
            ("focus up", "k"),
            ("focus right", "l"),
            ("zoom pane", "z"),
            ("new tab", "c"),
            ("next tab", "n"),
            ("previous tab", "p"),
            ("copy mode", "["),
            ("detach", "d"),
            ("add pane", "a"),
        ] {
            assert_eq!(
                help_key(&overlay, label).as_deref(),
                Some(key),
                "{label} must be reachable from the help surface"
            );
        }
    }

    /// A row says which `[keys]` name to write, so the surface doubles as the
    /// answer to "how do I change this" — and cloo's own surfaces, which have no
    /// `[keys]` name to give, say where they come from instead.
    #[test]
    fn a_help_row_says_where_its_binding_comes_from() {
        let rows = help_rows(&help());
        assert!(rows.contains(&(
            "%".to_owned(),
            "split right".to_owned(),
            "split-vertical".to_owned()
        )));
        assert!(rows.contains(&("a".to_owned(), "add pane".to_owned(), "client".to_owned())));
        assert!(rows.contains(&("?".to_owned(), "this help".to_owned(), "client".to_owned())));
    }

    /// The property that makes the surface worth reading from the keymap: a
    /// hand-written table would keep showing `C-b %` here.
    #[test]
    fn a_rebound_prefix_and_chord_are_shown_verbatim() {
        let mut keys = Keymap::defaults();
        keys.set_prefix(Key::parse("M-Space").expect("a spelling"));
        keys.unbind(Key::char('%'));
        keys.bind(Key::char('v'), Action::SplitVertical);
        let overlay = Overlay::help(&keys);
        assert_eq!(
            overlay.title(),
            "keys - prefix M-space",
            "the keymap's own canonical spelling, not the test's"
        );
        assert_eq!(help_key(&overlay, "split right").as_deref(), Some("v"));
    }

    #[test]
    fn an_unbound_action_has_no_help_row_at_all() {
        let mut keys = Keymap::defaults();
        keys.unbind(Key::char('d'));
        assert_eq!(
            help_key(&Overlay::help(&keys), "detach"),
            None,
            "a row for a chord that does nothing sends the user pressing it"
        );
    }

    /// The client's own surfaces are not in the keymap, so the honest rule is
    /// that a user who binds one of those keys takes it — and the row goes with
    /// it, because the chord will never reach `open_overlay` again.
    #[test]
    fn a_keymap_that_claims_a_client_key_takes_its_help_row_too() {
        let mut keys = Keymap::defaults();
        keys.bind(Key::char('?'), Action::ToggleZoom);
        let overlay = Overlay::help(&keys);
        assert_eq!(help_key(&overlay, "this help"), None);
        assert_eq!(
            help_key(&overlay, "sessions").as_deref(),
            Some("s"),
            "the neighbouring surfaces are untouched"
        );
    }

    #[test]
    fn the_help_surface_is_a_reading_surface_and_confirms_to_a_close() {
        assert_eq!(help().confirm(), OverlayOutcome::Dismissed);
    }

    /// The 16-colour contract: the surface has to be legible where colour and
    /// non-ASCII glyphs are both unavailable, so every character is ASCII and
    /// the key column carries the accent as bold as well.
    #[test]
    fn the_help_surface_is_ascii_and_marks_its_keys_without_colour() {
        let theme = Theme::storm();
        let overlay = help();
        for row in overlay_cells(&overlay, Size::new(48, 20), theme) {
            assert!(
                row.iter().all(|cell| cell.ch.is_ascii()),
                "{:?} must survive a terminal with no glyph support",
                text(&row)
            );
        }
        let row = row_cells(&overlay.row(0, false, theme), 48, theme);
        assert!(
            row[2].attrs.contains(CellAttrs::BOLD),
            "the chord is the one thing on the row a user has to find"
        );
    }

    #[test]
    fn a_help_row_spends_width_in_the_documented_order() {
        let theme = Theme::storm();
        let overlay = help();
        for (width, expected) in [
            (32_u16, "  % split right split-vertical  "),
            (20, "  % split right     "),
            (10, "  % split "),
            (4, "  % "),
        ] {
            assert_eq!(
                text(&row_cells(&overlay.row(0, false, theme), width, theme)),
                expected,
                "at width {width}"
            );
        }
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

    // -- the attention queue -------------------------------------------------

    /// The queue is reached like every other client surface: `!` is offered
    /// while the keymap leaves it free, and a user who binds it takes both the
    /// chord and its help row, because the chord will never reach the client
    /// again.
    #[test]
    fn the_attention_queue_key_is_offered_only_while_the_keymap_leaves_it_free() {
        assert_eq!(
            help_key(&help(), "attention queue").as_deref(),
            Some("!"),
            "a surface a user cannot discover is one they do not have"
        );
        let mut keys = Keymap::defaults();
        keys.bind(Key::char(ATTENTION_KEY), Action::ToggleZoom);
        assert_eq!(help_key(&Overlay::help(&keys), "attention queue"), None);
    }

    /// The queue's two verbs, and the difference between them: focusing names
    /// the pane and is done, acknowledging names the pane and leaves the surface
    /// up for the next row.
    #[test]
    fn an_attention_queue_focuses_and_acknowledges_the_pane_a_row_names() {
        let mut queue = attention_queue();
        assert_eq!(
            queue.apply_queue(QueueAction::Focus),
            OverlayOutcome::FocusPane(PaneId::new(4))
        );
        queue.apply_queue(QueueAction::Next);
        assert_eq!(
            queue.apply_queue(QueueAction::Acknowledge),
            OverlayOutcome::Acknowledge(PaneId::new(9)),
            "acknowledgment names the pane the user is looking at, not a position"
        );
    }

    /// The whole reason the rows carry a `PaneId`: a queue position is a number
    /// a closing pane can hand to a different pane entirely, and neither verb
    /// may act on a pane the user was not looking at.
    #[test]
    fn an_attention_queue_acts_on_a_pane_and_never_on_a_position() {
        let mut queue = Overlay::attention(vec![AttentionEntry {
            pane: PaneId::new(9),
            // The same position the first fixture row occupies, held by a
            // different pane after its neighbour closed.
            index: 2,
            title: "build".to_owned(),
            attention: Attention::Failed,
        }]);
        assert_eq!(
            queue.apply_queue(QueueAction::Focus),
            OverlayOutcome::FocusPane(PaneId::new(9))
        );
    }

    #[test]
    fn an_attention_queue_navigates_and_dismisses_like_every_other_overlay() {
        let mut queue = attention_queue();
        assert_eq!(queue.apply_queue(QueueAction::Prev), OverlayOutcome::Open);
        assert_eq!(queue.selection(), 0, "the top does not wrap");
        for expected in [1, 1] {
            assert_eq!(queue.apply_queue(QueueAction::Next), OverlayOutcome::Open);
            assert_eq!(queue.selection(), expected);
        }
        assert_eq!(
            queue.apply_queue(QueueAction::Dismiss),
            OverlayOutcome::Dismissed
        );
    }

    #[test]
    fn an_empty_attention_queue_acts_on_nothing_at_all() {
        let mut queue = Overlay::attention(Vec::new());
        for action in [
            QueueAction::Next,
            QueueAction::Prev,
            QueueAction::Focus,
            QueueAction::Acknowledge,
        ] {
            assert_eq!(
                queue.apply_queue(action),
                OverlayOutcome::Open,
                "{action:?}"
            );
            assert_eq!(queue.selection(), 0);
        }
    }

    /// Acknowledgment is the server's answer, so the row leaves on the next
    /// projection rather than when the client hid it — and the cursor stays on
    /// the pane the user had selected rather than on whatever slid into its row.
    #[test]
    fn an_attention_queue_refresh_keeps_the_cursor_on_its_pane() {
        let mut queue = attention_queue();
        queue.apply_queue(QueueAction::Next);
        assert_eq!(queue.selection(), 1);
        queue.refresh_attention(vec![attention_rows()[1].clone()]);
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.apply_queue(QueueAction::Focus),
            OverlayOutcome::FocusPane(PaneId::new(9)),
            "the row above going away must not move the cursor onto a neighbour"
        );
    }

    #[test]
    fn an_attention_queue_refresh_clamps_onto_a_shorter_list() {
        let mut queue = attention_queue();
        queue.apply_queue(QueueAction::Next);
        queue.refresh_attention(vec![attention_rows()[0].clone()]);
        assert_eq!(queue.selection(), 0);
        queue.refresh_attention(Vec::new());
        assert_eq!(queue.selection(), 0);
        assert_eq!(queue.apply_queue(QueueAction::Focus), OverlayOutcome::Open);
    }

    /// Refreshing is the queue's own affair: nothing else changes while it is
    /// open, and a stray call must not silently empty another surface.
    #[test]
    fn refreshing_attention_queue_rows_leaves_another_overlay_alone() {
        let mut overlay = sessions();
        overlay.refresh_attention(Vec::new());
        assert_eq!(overlay.len(), 3);
    }

    /// The 16-colour and no-glyph contract: the row says the state in words and
    /// in a shape, and the surface resolves through the client theme rather than
    /// a fixed palette, so the same expectation holds at both colour depths.
    #[test]
    fn an_attention_queue_row_is_ascii_and_states_itself_without_colour() {
        let queue = attention_queue();
        for (name, theme) in [
            ("truecolor", Theme::storm()),
            (
                "16-colour",
                Theme::new(ThemeChoice::Named(ThemeName::Storm), TermCaps::default()),
            ),
        ] {
            for row in overlay_cells(&queue, Size::new(40, 6), theme) {
                assert!(
                    row.iter().all(|cell| cell.ch.is_ascii()),
                    "{name}: {:?} must survive a terminal with no glyph support",
                    text(&row)
                );
            }
            let row = row_cells(&queue.row(0, false, theme), 40, theme);
            assert!(
                text(&row).contains("! needs input"),
                "{name}: the state is text as well as a colour"
            );
            let state = row
                .iter()
                .find(|cell| cell.ch == '!')
                .expect("the state glyph is on the row");
            assert_eq!(
                state.fg,
                Attention::NeedsInput.color_in(theme),
                "{name}: the state colour is the theme's, not a fixed palette's"
            );
        }
    }

    #[test]
    fn an_attention_queue_row_spends_width_in_the_documented_order() {
        let theme = Theme::storm();
        let queue = attention_queue();
        for (width, expected) in [
            (32_u16, "  2 claude ! needs input        "),
            (20, "  2 claude          "),
            (8, "  2 clau"),
            (4, "  2 "),
        ] {
            assert_eq!(
                text(&row_cells(&queue.row(0, false, theme), width, theme)),
                expected,
                "at width {width}"
            );
        }
    }

    /// Dismissal leads the hints in every overlay; here the middle slot goes to
    /// the verb no other surface has, because a user who cannot find `a` never
    /// clears the queue.
    #[test]
    fn a_narrow_attention_queue_still_says_how_to_close_and_how_to_clear() {
        let theme = Theme::storm();
        let queue = attention_queue();
        let box_cells = overlay_cells(&queue, Size::new(34, 4), theme);
        assert_eq!(text(&box_cells[0]).trim_end(), "  attention 1/2");
        assert_eq!(
            text(&box_cells[3]).trim_end(),
            "  esc close enter focus a ack"
        );
        assert_eq!(
            text(&hint_cells(&queue, 14, theme)).trim_end(),
            "  esc close",
            "the last hint standing is the one that says how to get out"
        );
    }

    /// An empty queue is an answer, not a broken surface: it opens, it says what
    /// it is, it claims no position in a list that does not exist, and it closes.
    #[test]
    fn an_empty_attention_queue_still_renders_a_legible_box() {
        let theme = Theme::storm();
        let queue = Overlay::attention(Vec::new());
        let box_cells = overlay_cells(&queue, Size::new(30, 4), theme);
        assert_eq!(text(&box_cells[0]).trim_end(), "  attention");
        assert!(text(&box_cells[1]).trim().is_empty());
        assert!(text(&box_cells[3]).contains("esc close"));
    }

    // -- rendering ----------------------------------------------------------

    /// The exact-width guarantee, at every width, for every overlay. This loop
    /// — not the pretty cases — is what catches an off-by-one in the gap
    /// arithmetic, exactly as it does for the pane header.
    #[test]
    fn every_overlay_row_is_exactly_the_width_asked_for() {
        let theme = Theme::storm();
        for (name, overlay) in [
            ("help", help()),
            ("sessions", sessions()),
            ("launcher", launcher()),
            ("attention queue", attention_queue()),
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
