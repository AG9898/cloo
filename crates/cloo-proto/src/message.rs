//! The wire messages themselves, and the value types they carry.
//!
//! Naming follows `docs/CONVENTIONS.md`: message variants are nouns or
//! past-tense events (`Attach`, `Damage`, `Detached`), while [`Action`]
//! variants are imperative (`SplitVertical`, `FocusLeft`).

use serde::{Deserialize, Serialize};

use crate::ids::{PaneId, SessionId, TabId};

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in columns.
    pub cols: u16,
    /// Height in rows.
    pub rows: u16,
}

impl Size {
    /// Builds a size.
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// The per-axis minimum of two sizes.
    ///
    /// Multiple clients of differing sizes render at the minimum of both and the
    /// larger letterboxes, so the session never exceeds what every attached
    /// client can draw.
    #[must_use]
    pub const fn min(self, other: Self) -> Self {
        Self {
            cols: if self.cols < other.cols {
                self.cols
            } else {
                other.cols
            },
            rows: if self.rows < other.rows {
                self.rows
            } else {
                other.rows
            },
        }
    }
}

/// A zero-indexed cell coordinate within a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// Column, from the left edge.
    pub col: u16,
    /// Row, from the top edge.
    pub row: u16,
}

impl Point {
    /// Builds a point.
    #[must_use]
    pub const fn new(col: u16, row: u16) -> Self {
        Self { col, row }
    }
}

/// A split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Children sit side by side.
    Horizontal,
    /// Children sit one above the other.
    Vertical,
}

// ---------------------------------------------------------------------------
// Terminal capabilities
// ---------------------------------------------------------------------------

/// What the client's outer terminal can actually do.
///
/// Reported once at attach. A client that lacks a capability must pick a
/// documented fallback rather than claim support; it combines these values
/// with its local policy before applying a typed outer-terminal effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TermCaps {
    /// True colour (24-bit SGR) rather than only the 256-colour palette.
    pub truecolor: bool,
    /// Bracketed paste mode.
    pub bracketed_paste: bool,
    /// SGR-encoded mouse reporting.
    pub sgr_mouse: bool,
    /// Focus in/out reporting.
    pub focus_events: bool,
    /// The Kitty extended keyboard protocol.
    pub extended_keys: bool,
    /// OSC 52 clipboard writes.
    pub clipboard_osc52: bool,
    /// OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// Inline graphics. Always an enhancement, never a compatibility requirement.
    pub graphics: bool,
}

// ---------------------------------------------------------------------------
// Outer-terminal effects
// ---------------------------------------------------------------------------

/// A narrowly allowlisted change a pane requests of an attached client's
/// terminal.
///
/// These are intent, not escape bytes. The renderer is the only component that
/// turns an allowed effect into a terminal sequence, after applying that
/// client's capability and local-policy checks. There is deliberately no raw
/// OSC or DCS variant: arbitrary terminal passthrough is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OuterTerminalEffect {
    /// Set the outer terminal's window title.
    SetTitle(String),
    /// Restore the outer terminal's default window title.
    ResetTitle,
    /// Store text in one named clipboard target.
    ClipboardStore {
        /// Where the text should be stored.
        target: ClipboardTarget,
        /// Plain UTF-8 text to store.
        text: String,
    },
    /// Make a URI available as a terminal hyperlink.
    Hyperlink {
        /// The link destination.
        uri: String,
    },
    /// Ask the terminal to present an application notification.
    Notification {
        /// Short notification heading.
        title: String,
        /// Notification body.
        body: String,
    },
    /// Update the terminal's progress presentation.
    Progress(ProgressState),
    /// Report the only graphics outcome cloo can currently model safely.
    ///
    /// Graphics bytes are never carried on the wire. A client can treat this
    /// as a no-op while keeping the pane usable.
    Graphics(GraphicsEffect),
}

/// A clipboard target cloo permits an effect to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardTarget {
    /// The regular clipboard (OSC 52's `c` selection).
    Clipboard,
    /// The primary selection (OSC 52's `p` selection).
    PrimarySelection,
}

/// A terminal-progress state with no renderer-specific payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressState {
    /// Remove a previous progress indication.
    Clear,
    /// Show activity whose completion is unknown.
    Indeterminate,
    /// Show a completion percentage from 0 through 100.
    Value(u8),
    /// Show a failed progress state.
    Error,
}

/// The safe graphics model for v1.
///
/// A graphics request is never represented as raw payload. Until cloo has a
/// client-local graphics implementation, unsupported graphics are explicit and
/// suppressible rather than relayed as an opaque DCS or OSC sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsEffect {
    /// The pane remains usable, but no inline graphic can be rendered.
    Unavailable,
}

// ---------------------------------------------------------------------------
// Cell content
// ---------------------------------------------------------------------------

/// A cell colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Color {
    /// The terminal's own default foreground or background.
    #[default]
    Default,
    /// An index into the 256-colour palette.
    Indexed(u8),
    /// A 24-bit colour. Clients without `truecolor` downsample.
    Rgb(u8, u8, u8),
}

/// Rendition flags for a cell, packed into a bitfield rather than a struct of
/// `bool`s — this rides the damage path and postcard gives each `bool` a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    /// No rendition applied.
    pub const NONE: Self = Self(0);
    /// Bold.
    pub const BOLD: Self = Self(1 << 0);
    /// Dim / faint.
    pub const DIM: Self = Self(1 << 1);
    /// Italic.
    pub const ITALIC: Self = Self(1 << 2);
    /// Underline.
    pub const UNDERLINE: Self = Self(1 << 3);
    /// Reverse video.
    pub const REVERSE: Self = Self(1 << 4);
    /// Hidden / concealed.
    pub const HIDDEN: Self = Self(1 << 5);
    /// Strikethrough.
    pub const STRIKETHROUGH: Self = Self(1 << 6);

    /// Combines two sets of flags.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// True when every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One rendered character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The character occupying the cell.
    pub ch: char,
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Rendition flags.
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::NONE,
        }
    }
}

/// A whole row of a pane, replaced wholesale.
///
/// Damage is coalesced per row rather than per cell: a row is the smallest unit
/// worth the framing overhead, and it keeps the client's apply step a memcpy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowUpdate {
    /// Which row, from the top of the pane.
    pub row: u16,
    /// The full row contents. Length equals the pane width.
    pub cells: Vec<Cell>,
}

// ---------------------------------------------------------------------------
// Layout and session shape
// ---------------------------------------------------------------------------

/// A pane's resolved position, in cells, within its tab.
///
/// The authoritative layout is a tree of ratios in `cloo-core`; this is the
/// flattened result of one layout pass. Ratios never cross the wire, because a
/// client has nothing to do with them but draw the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRect {
    /// Which pane.
    pub pane: PaneId,
    /// Left edge, in columns from the tab origin.
    pub x: u16,
    /// Top edge, in rows from the tab origin.
    pub y: u16,
    /// Size of the pane's grid.
    pub size: Size,
}

/// The full resolved geometry of one tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSnapshot {
    /// Which tab this describes.
    pub tab: TabId,
    /// Every visible pane and where it sits.
    pub panes: Vec<PaneRect>,
    /// The focused pane, if the tab has one.
    pub focused: Option<PaneId>,
    /// The pane currently zoomed to fill the tab, if any.
    pub zoomed: Option<PaneId>,
}

/// What a pane *is*, as opposed to where it sits.
///
/// Every field is explicit: it came from the profile the pane was launched from
/// or from what the user typed at launch. Nothing here is ever derived by
/// reading the pane's grid — a task inferred from transcript text would make the
/// rendered output a second source of truth, and it would be wrong the moment a
/// harness changed its wording.
///
/// The client renders this and never computes it. Strings rather than the
/// `cloo-core` newtypes, because the wire is the boundary those types validate
/// *at*: everything here was checked before the pane existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Which pane this describes.
    pub pane: PaneId,
    /// The ID of the profile the pane was launched from.
    pub profile: String,
    /// The pane's user-visible name — the user's, or the profile's default.
    pub name: String,
    /// What the user said the pane is for, if they said.
    pub task: Option<String>,
    /// The absolute directory the pane's child was launched in.
    pub cwd: String,
}

/// A pane's workspace state, as reported to the client.
///
/// Mirrors `cloo_core::pane::AttentionState` — the six states of
/// `docs/STYLEGUIDE.md`. The client turns each into a glyph, a label, and a
/// colour; it never invents a state of its own or derives one by reading a
/// pane's grid. `Unknown` is the honest default for a child nothing reliable has
/// reported on, and is distinct from `Quiet`, which is a *claim* only a source
/// may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AttentionState {
    /// Nothing reliable has reported.
    #[default]
    Unknown,
    /// A source says the pane is making progress.
    Working,
    /// The pane requires a decision or a response.
    NeedsInput,
    /// The pane finished with a result nobody has looked at.
    Ready,
    /// The child exited unsuccessfully, or a source reported failure.
    Failed,
    /// A source says there is nothing to do.
    Quiet,
}

/// Where a pane's attention state came from.
///
/// Mirrors `cloo_core::pane::AttentionSource`. Carried alongside the state
/// rather than folded into it, so the chrome can show an adapter's advisory
/// claim as an adapter's claim rather than as fact. Only [`Adapter`](Self::Adapter)
/// is advisory; the rest are things cloo observed itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AttentionSource {
    /// Nothing has reported. Pairs with [`AttentionState::Unknown`].
    #[default]
    None,
    /// The child rang the terminal bell.
    Bell,
    /// The child started, stopped, or exited.
    Lifecycle,
    /// The user marked the pane explicitly.
    User,
    /// An opt-in local adapter reported it, named here so the chrome can
    /// attribute it.
    Adapter(String),
}

/// A pane's attention state, its provenance, and whether the user has seen it.
///
/// Separate from [`PaneInfo`] on purpose: identity changes only when a pane is
/// launched, closed, or renamed, while attention changes whenever a source
/// reports. A state without its source is exactly the claim the chrome must not
/// make, which is why the source rides along here rather than being flattened
/// away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAttention {
    /// Which pane this describes.
    pub pane: PaneId,
    /// The current state.
    pub state: AttentionState,
    /// Where the current state came from.
    pub source: AttentionSource,
    /// Whether the user has acknowledged the current state.
    pub acknowledged: bool,
}

/// A position in retained pane scrollback, counted from its oldest line.
///
/// Separate from [`Point`], whose coordinates are viewport-relative and fit in
/// one terminal frame. Copy mode needs a stable line position while the
/// viewport moves through server-owned history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScrollPoint {
    /// Zero-based retained line number.
    pub line: u32,
    /// Zero-based terminal column.
    pub column: u16,
}

impl ScrollPoint {
    /// Creates one retained-scrollback position.
    #[must_use]
    pub const fn new(line: u32, column: u16) -> Self {
        Self { line, column }
    }
}

/// One linear visual selection in retained scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopySelection {
    /// Where visual selection began.
    pub anchor: ScrollPoint,
    /// Current copy cursor position.
    pub head: ScrollPoint,
}

/// One non-empty regex match, ending exclusively for direct highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// First matched cell.
    pub start: ScrollPoint,
    /// One cell after the match.
    pub end: ScrollPoint,
}

/// The copy and regex state for one focused pane.
///
/// This travels as its own clock: output can change rows without changing a
/// selection, and a cursor move must not resend the grid to a newly attached
/// client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyModeState {
    /// Pane whose retained scrollback the state addresses.
    pub pane: PaneId,
    /// Retained line currently drawn on the pane's first visible row.
    ///
    /// Every other position here is absolute in server-owned history, and a
    /// client holds only the visible grid. This is the one number that maps the
    /// two together, so a highlight lands on the row the text is actually on
    /// rather than on the row a client guessed.
    pub viewport_top: u32,
    /// Current copy cursor.
    pub cursor: ScrollPoint,
    /// Live visual selection, if any.
    pub selection: Option<CopySelection>,
    /// Current regex text, when a search has been issued.
    pub query: Option<String>,
    /// Non-empty matches in retained scrollback order.
    pub matches: Vec<SearchMatch>,
}

/// Enough about a tab to draw the tab bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSummary {
    /// Which tab.
    pub tab: TabId,
    /// Display title.
    pub title: String,
    /// Whether this is the active tab.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Which mouse button an event concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Tertiary button.
    Middle,
    /// Secondary button.
    Right,
}

/// What a mouse event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    /// A button went down.
    Press(MouseButton),
    /// A button came up.
    Release(MouseButton),
    /// The pointer moved, optionally dragging a held button.
    Motion(Option<MouseButton>),
    /// The wheel scrolled up.
    ScrollUp,
    /// The wheel scrolled down.
    ScrollDown,
}

/// The modifier keys held during a mouse event.
///
/// Carried because they change both halves of routing: `shift` is the
/// conventional "this one is for the multiplexer" override, and all three are
/// part of the button code an SGR mouse report encodes for the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MouseMods {
    /// Shift was held.
    pub shift: bool,
    /// Alt / Meta was held.
    pub alt: bool,
    /// Control was held.
    pub ctrl: bool,
}

impl MouseMods {
    /// No modifiers held.
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
    };
}

/// A mouse event, already resolved to a pane and a cell within it.
///
/// Hit testing is the client's job: it knows the chrome geometry it drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    /// The pane under the pointer.
    pub pane: PaneId,
    /// Where in that pane.
    pub at: Point,
    /// What happened.
    pub kind: MouseKind,
    /// Which modifiers were held.
    pub mods: MouseMods,
}

/// How much of the mouse a pane's application is tracking.
///
/// Mirrors `cloo_term::MouseTracking`; the ordering is the filtering rule, since
/// each level reports everything the level below it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum MouseTracking {
    /// Not tracking. Every mouse event belongs to cloo's chrome.
    #[default]
    Off,
    /// Button presses and releases only.
    Click,
    /// Presses, releases, and motion while a button is held.
    Drag,
    /// All of the above, plus motion with no button held.
    Motion,
}

/// The input modes a pane's application has negotiated with its terminal.
///
/// The server reports these because the client cannot know them: they are set
/// by escape sequences the *child* wrote, which only the emulator sees. A client
/// needs them to decide whether a mouse event belongs to the application or to
/// cloo's own chrome; the server needs them to decide how an event is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaneModes {
    /// How much of the mouse the application wants reported.
    pub mouse: MouseTracking,
    /// Whether mouse reports use the SGR encoding rather than the legacy one.
    pub sgr_mouse: bool,
    /// Whether pasted text is wrapped in paste brackets.
    pub bracketed_paste: bool,
    /// Whether focus gain and loss are reported to the application.
    pub focus_events: bool,
    /// Whether the application reads keys in the extended encoding.
    pub extended_keys: bool,
}

/// A cursor shape, as reported to the client for chrome-accurate drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CursorShape {
    /// Filled block.
    #[default]
    Block,
    /// Vertical bar.
    Beam,
    /// Horizontal bar.
    Underline,
}

/// One vim-like copy-mode cursor motion.
///
/// Named after the key a default keymap binds it to, because the *motion* is
/// the intent and the key is not: a rebound `h` still means "one cell left".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyMotion {
    /// One cell left.
    Left,
    /// One line down.
    Down,
    /// One line up.
    Up,
    /// One cell right.
    Right,
    /// To the start of the next word.
    WordForward,
    /// To the start of the previous word.
    WordBackward,
    /// To the first column.
    LineStart,
    /// To the last occupied column.
    LineEnd,
    /// To the oldest retained line.
    FirstLine,
    /// To the newest retained line.
    LastLine,
}

/// Which way a copy-mode search walks its results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchDirection {
    /// Toward newer retained lines.
    Forward,
    /// Toward older retained lines.
    Backward,
}

/// A bound command, resolved by the client's keymap and sent by name.
///
/// The client sends the *intent*, never the raw key. This is what keeps keymap
/// changes from being a protocol change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Split the focused pane along a vertical divider.
    SplitVertical,
    /// Split the focused pane along a horizontal divider.
    SplitHorizontal,
    /// Close the focused pane.
    ClosePane,
    /// Move focus one pane in a direction.
    FocusLeft,
    /// Move focus one pane in a direction.
    FocusRight,
    /// Move focus one pane in a direction.
    FocusUp,
    /// Move focus one pane in a direction.
    FocusDown,
    /// Move focus to the pane the user named.
    ///
    /// The mouse's half of focus: a click lands on a pane directly rather than
    /// on a direction from wherever focus happens to be. It carries a pane id, so
    /// it has no keymap spelling — the keyboard reaches the same session state
    /// through the four directional actions above, which is the equivalence
    /// every chrome mouse gesture is required to have.
    FocusPane(PaneId),
    /// Toggle zoom on the focused pane.
    ToggleZoom,
    /// Move the divider next to `pane`, growing that pane by `delta` cells.
    ///
    /// The wire half of a gutter drag, and deliberately expressed in *cells*
    /// rather than in a ratio: ratios never cross the wire, and a client that
    /// sent one would be doing arithmetic over a split extent only the server's
    /// layout tree knows. The server turns the delta into exactly one new ratio,
    /// so a drag can never create, close, or reorder a pane.
    ResizePane {
        /// The pane that grows on a positive delta and shrinks on a negative
        /// one. Its nearest ancestor split along `dir` is the divider moved.
        pane: PaneId,
        /// The axis the divider divides along.
        dir: Direction,
        /// How many cells to move it by, signed toward `pane` growing.
        delta: i16,
    },
    /// Create a new tab.
    NewTab,
    /// Close the active tab.
    CloseTab,
    /// Activate the next tab.
    NextTab,
    /// Activate the previous tab.
    PrevTab,
    /// Rename the active tab.
    RenameTab(String),
    /// Enter copy mode on the focused pane.
    ///
    /// Copy mode is session state, not a client mode: the scrollback it moves
    /// through belongs to the server, and a second client attaching finds the
    /// same cursor, selection, and search.
    EnterCopyMode,
    /// Leave copy mode and resume following live output.
    ExitCopyMode,
    /// Move the copy cursor.
    CopyMotion(CopyMotion),
    /// Begin a visual selection at the copy cursor.
    BeginCopySelection,
    /// Drop the visual selection without moving the copy cursor.
    ClearCopySelection,
    /// Run a regex over the focused pane's retained scrollback.
    CopySearch {
        /// The user's regex text, compiled by the server.
        query: String,
        /// Which way to enter the result set.
        direction: SearchDirection,
    },
    /// Visit another result of the active copy-mode search.
    NextCopyMatch(SearchDirection),
    /// Copy the focused pane's copy-mode selection to a clipboard target.
    ///
    /// Explicit by construction. Selected text is server-owned scrollback, so
    /// it crosses the wire only when a user asks for it, and it comes back as a
    /// typed [`OuterTerminalEffect::ClipboardStore`] to the one client that
    /// asked — never as a broadcast, which would put one user's selection in
    /// every attached terminal's clipboard. That client's own policy and
    /// capabilities are still the final gate.
    CopySelection(ClipboardTarget),
    /// Detach this client, leaving the session running.
    DetachClient,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Client → server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// The first message on any connection. Carries the protocol version so a
    /// mismatch is caught before either side interprets a single wire type.
    Attach {
        /// The protocol version this client speaks.
        protocol_version: u16,
        /// The client's terminal size.
        size: Size,
        /// What the client's terminal can do.
        term_caps: TermCaps,
        /// An existing session to reattach to, or `None` to take the default.
        session: Option<SessionId>,
    },
    /// Leave the session running and disconnect.
    Detach,
    /// Keyboard bytes destined for the focused pane's PTY.
    ///
    /// Already encoded by the client, in whichever scheme its terminal
    /// negotiated. Encoding is the client's business; the pane's own bracketing
    /// and reporting modes are the server's, which is why paste, focus, and
    /// mouse are separate variants rather than more bytes on this one.
    Input(Vec<u8>),
    /// Text the user pasted, as text.
    ///
    /// Distinct from [`Input`](Self::Input) because whether it reaches the child
    /// wrapped in paste brackets depends on a mode the *child* set, which only
    /// the server can see. A client that sent pre-bracketed bytes would be
    /// guessing at state it does not hold.
    Paste(Vec<u8>),
    /// The client's terminal gained or lost focus.
    Focus {
        /// Whether the client is now focused.
        focused: bool,
    },
    /// A mouse event already resolved to a pane.
    ///
    /// Sent only for events the client decided belong to the *application*.
    /// Events belonging to cloo's chrome never reach the wire — see
    /// [`PaneModes`].
    Mouse(MouseEvent),
    /// The client's terminal changed size.
    Resize(Size),
    /// A keymap-resolved command.
    Command(Action),
}

/// Server → client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// The reply to a successful [`ClientMessage::Attach`]. Echoes the server's
    /// protocol version so the client can verify the pairing from its side too.
    Hello {
        /// The protocol version the server speaks.
        protocol_version: u16,
        /// The session this client is now attached to.
        session: SessionId,
        /// Every tab in that session.
        tabs: Vec<TabSummary>,
        /// The effective session size, already reduced to the minimum across
        /// all attached clients.
        size: Size,
    },
    /// The attach was refused. The client should report `reason` and exit
    /// rather than continue reading a stream it cannot interpret.
    Refused {
        /// A human-readable explanation, typically a rendered [`crate::ProtoError`].
        reason: String,
    },
    /// Coalesced row damage for one pane.
    Damage {
        /// Which pane changed.
        pane: PaneId,
        /// The rows that changed, in ascending row order.
        rows: Vec<RowUpdate>,
    },
    /// The cursor moved or changed appearance.
    CursorMoved {
        /// Which pane owns the cursor.
        pane: PaneId,
        /// Where it now sits.
        pos: Point,
        /// How it should be drawn.
        shape: CursorShape,
        /// Whether it should be drawn at all.
        visible: bool,
    },
    /// New resolved geometry for a tab.
    Layout(LayoutSnapshot),
    /// Who every pane is: profile, name, task label, and working directory.
    ///
    /// Separate from [`Layout`](Self::Layout) because the two change on
    /// completely different clocks — geometry moves on every resize, while a
    /// pane's identity changes only when one is launched, closed, or renamed.
    /// Sent whole rather than per pane, so a client can replace its map
    /// wholesale and never hold an entry for a pane that no longer exists.
    Panes(Vec<PaneInfo>),
    /// Every pane's attention state, its provenance, and whether it has been
    /// acknowledged.
    ///
    /// Separate from [`Panes`](Self::Panes) because the two travel on different
    /// clocks: a rename is not a state change and a state change is not a
    /// rename. Sent whole, like `Panes`, so a client replaces its map and never
    /// keeps an entry for a pane that closed. An uninstrumented pane is carried
    /// as [`AttentionState::Unknown`] rather than omitted — the client renders
    /// that state too, and never guesses one from the grid.
    Attention(Vec<PaneAttention>),
    /// The focused pane's server-owned copy and search state, or `None` when
    /// copy mode is inactive.
    CopyMode(Option<CopyModeState>),
    /// A pane's application changed which input modes it has negotiated.
    ///
    /// The client cannot observe this for itself — the modes were set by
    /// sequences the child wrote — and it is what decides whether a mouse event
    /// is the application's or cloo's chrome's.
    Modes {
        /// Which pane.
        pane: PaneId,
        /// What its application now has enabled.
        modes: PaneModes,
    },
    /// A typed request to change one attached client's outer terminal.
    ///
    /// The effect is client-local and never changes authoritative session
    /// state. Clients apply only effects allowed by their capabilities and
    /// local policy.
    Effect {
        /// Pane whose application requested the effect.
        pane: PaneId,
        /// The allowlisted request, never raw terminal bytes.
        effect: OuterTerminalEffect,
    },
    /// A pane rang the bell.
    Bell(PaneId),
    /// The tab set changed — added, removed, renamed, or reordered.
    Tabs(Vec<TabSummary>),
    /// The detach the client asked for has completed. The session keeps running.
    Detached,
    /// The session ended. The client should restore the terminal and exit with
    /// this code.
    Exit(i32),
}
