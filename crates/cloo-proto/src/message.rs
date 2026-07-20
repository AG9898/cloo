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
/// documented fallback rather than claim support; the server uses this to decide
/// which typed outer-terminal effects are worth sending at all.
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
    /// Toggle zoom on the focused pane.
    ToggleZoom,
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
    Input(Vec<u8>),
    /// A mouse event already resolved to a pane.
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
