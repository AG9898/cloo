//! What the user's terminal sends, turned into typed events cloo can route.
//!
//! Three things live here, and they compose in one direction:
//!
//! 1. [`OuterModes`] — which reporting modes cloo asks the *outer* terminal to
//!    turn on, derived from the capabilities negotiated at attach. Nothing is
//!    requested that the terminal did not report, because a mode that is asked
//!    for and not supported still leaves its enable sequence printed on the
//!    user's screen.
//! 2. [`InputDecoder`] — the byte stream from that terminal, split back into
//!    [`InputEvent`]s. A paste, a focus report, and a mouse report all arrive as
//!    ordinary bytes on stdin, and telling them apart is the only way they can
//!    be routed differently.
//! 3. [`ScreenLayout`] and [`route_mouse`] — where a mouse report landed, and
//!    whether that place belongs to the pane's application or to cloo's own
//!    chrome. [`mouse_owner`] is the ownership rule on its own, for a caller
//!    that has already done its own hit testing.
//!
//! **The client decodes; the server encodes.** A paste leaves here as text and
//! is bracketed on the far side, because whether the child wants brackets is a
//! mode the child set and only the server can see. The same split is why a mouse
//! event crosses the wire as a [`MouseEvent`] rather than as an escape sequence.
//!
//! Ownership deserves its own sentence, because it is the property the whole
//! mouse path is built on. A mouse event is the application's only when the
//! application is actually tracking the mouse, the pointer is over that
//! application's pane rather than over chrome, and the user did not hold the
//! shift override. Everything else is cloo's, and **cloo's events never reach
//! the wire** — a chrome click that leaked into a pane would appear in the
//! user's shell as garbage. That is why [`route_mouse`] returns a
//! [`MouseRoute`]: an application event arrives already shaped as the
//! [`MouseEvent`] the wire takes, and a chrome event has no such shape at all,
//! so there is nothing for a caller to send by mistake.

use cloo_proto::{
    MouseButton, MouseEvent, MouseKind, MouseMods, MouseTracking, PaneId, PaneModes, Point, Size,
    TermCaps,
};

/// Turns on bracketed paste in the outer terminal.
const PASTE_ON: &[u8] = b"\x1b[?2004h";
/// Turns it off again.
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
/// Turns on button, drag, and SGR mouse reporting.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
/// Turns them off, in the reverse order.
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";
/// Turns on focus reporting.
const FOCUS_ON: &[u8] = b"\x1b[?1004h";
/// Turns it off again.
const FOCUS_OFF: &[u8] = b"\x1b[?1004l";
/// Pushes a Kitty keyboard flag set: disambiguate escape codes.
const KEYS_ON: &[u8] = b"\x1b[>1u";
/// Pops it again.
const KEYS_OFF: &[u8] = b"\x1b[<u";

/// The start of a bracketed paste.
const PASTE_START: &[u8] = b"\x1b[200~";
/// The end of one.
const PASTE_END: &[u8] = b"\x1b[201~";
/// Focus gained.
const FOCUS_IN: &[u8] = b"\x1b[I";
/// Focus lost.
const FOCUS_OUT: &[u8] = b"\x1b[O";

/// The reporting modes cloo has asked the outer terminal for.
///
/// Derived from [`TermCaps`] and from nothing else: a capability the client
/// could not establish takes its documented fallback, which for every mode here
/// means simply not asking. Silence is always safe — the fallback for absent
/// mouse reporting is keyboard-driven chrome, and for absent bracketed paste it
/// is pasted text arriving as typed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OuterModes {
    /// Bracketed paste was requested, so a paste can be told from typing.
    pub bracketed_paste: bool,
    /// Mouse reporting was requested, in the SGR encoding.
    pub sgr_mouse: bool,
    /// Focus reporting was requested.
    pub focus_events: bool,
    /// The extended keyboard protocol was pushed.
    pub extended_keys: bool,
}

impl OuterModes {
    /// The modes worth asking for, given what the terminal reported.
    #[must_use]
    pub const fn negotiated(caps: TermCaps) -> Self {
        Self {
            bracketed_paste: caps.bracketed_paste,
            sgr_mouse: caps.sgr_mouse,
            focus_events: caps.focus_events,
            extended_keys: caps.extended_keys,
        }
    }

    /// Nothing requested. The state every fallback leaves the terminal in.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            bracketed_paste: false,
            sgr_mouse: false,
            focus_events: false,
            extended_keys: false,
        }
    }

    /// The bytes that turn these modes on.
    #[must_use]
    pub fn enable(self) -> Vec<u8> {
        let mut out = Vec::new();
        for (wanted, sequence) in [
            (self.bracketed_paste, PASTE_ON),
            (self.sgr_mouse, MOUSE_ON),
            (self.focus_events, FOCUS_ON),
            (self.extended_keys, KEYS_ON),
        ] {
            if wanted {
                out.extend_from_slice(sequence);
            }
        }
        out
    }

    /// The bytes that turn them off again.
    ///
    /// Written in the reverse order of [`enable`](Self::enable), and it must
    /// stay exactly as symmetric: a mode left on after cloo exits keeps the
    /// user's shell reporting mouse motion at a program that has no idea what to
    /// do with it.
    #[must_use]
    pub fn disable(self) -> Vec<u8> {
        let mut out = Vec::new();
        for (wanted, sequence) in [
            (self.extended_keys, KEYS_OFF),
            (self.focus_events, FOCUS_OFF),
            (self.sgr_mouse, MOUSE_OFF),
            (self.bracketed_paste, PASTE_OFF),
        ] {
            if wanted {
                out.extend_from_slice(sequence);
            }
        }
        out
    }
}

/// A mouse report as the outer terminal sent it, before it is placed in a pane.
///
/// Coordinates are zero-based cells of the *whole* terminal. Turning them into a
/// pane and a cell within it is hit testing, which belongs with whoever drew the
/// chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    /// What happened.
    pub kind: MouseKind,
    /// Which modifiers were held.
    pub mods: MouseMods,
    /// Column, zero-based, from the left of the terminal.
    pub col: u16,
    /// Row, zero-based, from the top of the terminal.
    pub row: u16,
}

/// One thing the user did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Ordinary typed bytes, already encoded by the terminal.
    Keys(Vec<u8>),
    /// Text the user pasted, with the paste brackets removed.
    Paste(Vec<u8>),
    /// The terminal gained or lost focus.
    Focus(bool),
    /// A mouse report, not yet routed.
    Mouse(MouseReport),
}

/// Splits a terminal's byte stream into [`InputEvent`]s.
///
/// Sequences are recognised only for modes cloo actually asked for. That is not
/// an optimisation: `\x1b[I` is a legitimate thing for a program to send when
/// focus reporting was never enabled, and stealing it would corrupt input that
/// belongs to the pane.
///
/// A sequence split across two reads is held rather than mis-decoded, which
/// leaves one case needing care — a lone `\x1b` is a prefix of every sequence
/// here, so pressing Escape would be held forever waiting for bytes that never
/// come. [`flush`](Self::flush) is the answer: the run loop calls it on the
/// frame tick, so a held prefix is released within a frame.
#[derive(Debug, Clone)]
pub struct InputDecoder {
    modes: OuterModes,
    /// Bytes not yet resolved into an event.
    pending: Vec<u8>,
    /// Set between a paste's start and end markers.
    pasting: bool,
    /// The paste being accumulated.
    paste: Vec<u8>,
}

/// What a look at the head of the buffer found.
enum Found {
    /// A complete sequence of this length.
    Complete(InputEvent, usize),
    /// The start of one, but not all of it yet.
    Partial,
    /// Not a sequence this decoder claims.
    Other,
}

impl InputDecoder {
    /// A decoder for a terminal in `modes`.
    #[must_use]
    pub fn new(modes: OuterModes) -> Self {
        Self {
            modes,
            pending: Vec::new(),
            pasting: false,
            paste: Vec::new(),
        }
    }

    /// The modes this decoder recognises sequences for.
    #[must_use]
    pub fn modes(&self) -> OuterModes {
        self.modes
    }

    /// Whether anything is being held back for more bytes.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Decodes everything `bytes` completes, holding back any partial sequence.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut keys = Vec::new();
        let mut index = 0;

        while index < self.pending.len() {
            if self.pasting {
                match find(&self.pending[index..], PASTE_END) {
                    Some(at) => {
                        self.paste
                            .extend_from_slice(&self.pending[index..index + at]);
                        index += at + PASTE_END.len();
                        self.pasting = false;
                        events.push(InputEvent::Paste(std::mem::take(&mut self.paste)));
                        continue;
                    }
                    None => {
                        // Keep back only as much as could still become the
                        // terminator; the rest is paste content already.
                        let held = partial_suffix(&self.pending[index..], PASTE_END);
                        let upto = self.pending.len() - held;
                        self.paste.extend_from_slice(&self.pending[index..upto]);
                        index = upto;
                        break;
                    }
                }
            }

            match self.at(index) {
                Found::Complete(event, len) => {
                    if !keys.is_empty() {
                        events.push(InputEvent::Keys(std::mem::take(&mut keys)));
                    }
                    index += len;
                    // A paste start has no event of its own: the paste is the
                    // event, and it is emitted once the terminator arrives.
                    if !matches!(event, InputEvent::Paste(_)) {
                        events.push(event);
                    }
                }
                Found::Partial => break,
                Found::Other => {
                    keys.push(self.pending[index]);
                    index += 1;
                }
            }
        }

        self.pending.drain(..index);
        if !keys.is_empty() {
            events.push(InputEvent::Keys(keys));
        }
        events
    }

    /// Releases anything held back as ordinary keys.
    ///
    /// This is what makes a lone Escape reach the pane. A paste in progress is
    /// never flushed: its terminator really is still coming, and turning half a
    /// paste into keystrokes is the failure bracketed paste exists to avoid.
    pub fn flush(&mut self) -> Option<InputEvent> {
        if self.pasting || self.pending.is_empty() {
            return None;
        }
        Some(InputEvent::Keys(std::mem::take(&mut self.pending)))
    }

    /// Looks at the sequence starting at `index`.
    fn at(&mut self, index: usize) -> Found {
        let buf = &self.pending[index..];
        if buf[0] != 0x1b {
            return Found::Other;
        }

        if self.modes.bracketed_paste {
            match compare(buf, PASTE_START) {
                Compare::Equal => {
                    self.pasting = true;
                    return Found::Complete(InputEvent::Paste(Vec::new()), PASTE_START.len());
                }
                Compare::Prefix => return Found::Partial,
                Compare::Different => {}
            }
        }

        if self.modes.focus_events {
            for (sequence, focused) in [(FOCUS_IN, true), (FOCUS_OUT, false)] {
                match compare(buf, sequence) {
                    Compare::Equal => {
                        return Found::Complete(InputEvent::Focus(focused), sequence.len());
                    }
                    Compare::Prefix => return Found::Partial,
                    Compare::Different => {}
                }
            }
        }

        if self.modes.sgr_mouse {
            match decode_sgr_mouse(buf) {
                Found::Other => {}
                found => return found,
            }
        }

        Found::Other
    }
}

/// Whether one buffer holds, starts, or contradicts a sequence.
enum Compare {
    /// The sequence is entirely present.
    Equal,
    /// The buffer ended part-way through it.
    Prefix,
    /// The buffer is something else.
    Different,
}

/// Compares the head of `buf` with `sequence`.
fn compare(buf: &[u8], sequence: &[u8]) -> Compare {
    if buf.len() >= sequence.len() {
        if buf.starts_with(sequence) {
            return Compare::Equal;
        }
        return Compare::Different;
    }
    if sequence.starts_with(buf) {
        return Compare::Prefix;
    }
    Compare::Different
}

/// Decodes an SGR mouse report: `ESC [ < code ; col ; row (M|m)`.
fn decode_sgr_mouse(buf: &[u8]) -> Found {
    for (position, expected) in [(1, b'['), (2, b'<')] {
        match buf.get(position) {
            None => return Found::Partial,
            Some(byte) if *byte == expected => {}
            Some(_) => return Found::Other,
        }
    }

    let mut fields = [0_u32; 3];
    let mut field = 0;
    let mut digits = 0;
    let mut index = 3;
    loop {
        let Some(byte) = buf.get(index).copied() else {
            return Found::Partial;
        };
        match byte {
            b'0'..=b'9' => {
                // A report longer than this is a desync, not a coordinate.
                let Some(value) = fields[field]
                    .checked_mul(10)
                    .and_then(|scaled| scaled.checked_add(u32::from(byte - b'0')))
                else {
                    return Found::Other;
                };
                fields[field] = value;
                digits += 1;
            }
            b';' if field < 2 && digits > 0 => {
                field += 1;
                digits = 0;
            }
            b'M' | b'm' if field == 2 && digits > 0 => {
                let Some(report) = sgr_report(fields, byte == b'm') else {
                    return Found::Other;
                };
                return Found::Complete(InputEvent::Mouse(report), index + 1);
            }
            _ => return Found::Other,
        }
        index += 1;
    }
}

/// Builds a report from a decoded `code;col;row` triple.
fn sgr_report(fields: [u32; 3], released: bool) -> Option<MouseReport> {
    let code = fields[0];
    let mods = MouseMods {
        shift: code & 4 != 0,
        alt: code & 8 != 0,
        ctrl: code & 16 != 0,
    };
    let motion = code & 32 != 0;
    let button = match code & 0b1100_0011 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        // The "no button" code, which only means anything while moving.
        3 => None,
        64 => return build(MouseKind::ScrollUp, mods, fields),
        65 => return build(MouseKind::ScrollDown, mods, fields),
        _ => return None,
    };

    let kind = match (motion, released, button) {
        (true, _, held) => MouseKind::Motion(held),
        (false, false, Some(button)) => MouseKind::Press(button),
        (false, true, Some(button)) => MouseKind::Release(button),
        // A press or release of no button is not a thing a terminal reports.
        (false, _, None) => return None,
    };
    build(kind, mods, fields)
}

/// Turns one-based SGR coordinates into zero-based cells.
fn build(kind: MouseKind, mods: MouseMods, fields: [u32; 3]) -> Option<MouseReport> {
    Some(MouseReport {
        kind,
        mods,
        col: u16::try_from(fields[1].checked_sub(1)?).ok()?,
        row: u16::try_from(fields[2].checked_sub(1)?).ok()?,
    })
}

/// The first index at which `needle` appears in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How many trailing bytes of `haystack` could still grow into `needle`.
fn partial_suffix(haystack: &[u8], needle: &[u8]) -> usize {
    let most = haystack.len().min(needle.len() - 1);
    (1..=most)
        .rev()
        .find(|len| needle.starts_with(&haystack[haystack.len() - len..]))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Who a mouse event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseOwner {
    /// cloo's own chrome: borders, the status bar, pane selection. Never
    /// forwarded to a child.
    Chrome,
    /// The pane's application, which asked to hear about the mouse.
    Application,
}

/// Whether a mouse event is the application's or cloo's chrome's.
///
/// Three rules, in order, and each one alone is enough to keep it:
///
/// 1. A pointer that is not over the pane `modes` describes is over chrome,
///    whatever that application has enabled. Chrome is everything that is not a
///    pane's own grid, and so is any pane whose modes cloo does not hold —
///    guessing at an application's tracking level is exactly the claim that
///    would steal an event or invent one.
/// 2. Holding shift is the conventional "this one is for the multiplexer"
///    override, and it is the only way to reach chrome inside a pane run by a
///    full-screen application.
/// 3. An application not tracking the mouse cannot own a mouse event, so cloo
///    takes it — this is what makes click-to-focus work in an ordinary shell.
///
/// [`route_mouse`] is the form to reach for when the caller has a screen rather
/// than a single pane; this is the rule underneath it.
#[must_use]
pub fn mouse_owner(modes: PaneModes, report: &MouseReport, over_pane: bool) -> MouseOwner {
    if !over_pane || report.mods.shift || modes.mouse == MouseTracking::Off {
        return MouseOwner::Chrome;
    }
    MouseOwner::Application
}

/// One pane as the client drew it, in the outer terminal's own cells.
///
/// The server sends a pane's grid rectangle in the tab's coordinates; where that
/// tab area starts, and whether a header row was drawn above the grid, are the
/// client's answers because the client is what drew them. Building this from
/// what was rendered — rather than re-deriving it from the wire — is what keeps
/// a hit test agreeing with the picture the user is pointing at.
///
/// `size` is the grid, and the grid alone. The header row sits immediately above
/// it and is chrome: per `docs/STYLEGUIDE.md` the header row *is* the pane's top
/// border, so a click on it is a click on the pane's frame, never on its
/// contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneArea {
    /// Which pane.
    pub pane: PaneId,
    /// Left edge of the grid, in terminal columns.
    pub x: u16,
    /// Top edge of the grid, in terminal rows.
    pub y: u16,
    /// The grid's size, which is the size the server gave the pane.
    pub size: Size,
    /// Whether a header row was drawn on the row above the grid.
    pub header: bool,
}

impl PaneArea {
    /// A pane's grid at `(x, y)`, with a header row above it.
    #[must_use]
    pub const fn new(pane: PaneId, x: u16, y: u16, size: Size) -> Self {
        Self {
            pane,
            x,
            y,
            size,
            header: true,
        }
    }

    /// The same area with no header row drawn above it.
    #[must_use]
    pub const fn headerless(mut self) -> Self {
        self.header = false;
        self
    }

    /// Whether `(col, row)` is inside the pane's grid.
    #[must_use]
    pub const fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.x
            && row >= self.y
            && col < self.x.saturating_add(self.size.cols)
            && row < self.y.saturating_add(self.size.rows)
    }

    /// The cell `(col, row)` names inside this pane, if it is inside at all.
    ///
    /// Pane-local and zero-based, which is what [`MouseEvent`] carries: the
    /// server encodes the coordinates the *application* sees, and an application
    /// has never heard of the pane's place on a screen.
    #[must_use]
    pub const fn local(&self, col: u16, row: u16) -> Option<Point> {
        if !self.contains(col, row) {
            return None;
        }
        Some(Point::new(col - self.x, row - self.y))
    }

    /// Whether `(col, row)` is on this pane's header row.
    #[must_use]
    pub const fn on_header(&self, col: u16, row: u16) -> bool {
        self.header
            && self.y > 0
            && row == self.y - 1
            && col >= self.x
            && col < self.x.saturating_add(self.size.cols)
    }
}

/// What the client drew, in enough detail to place a mouse report.
///
/// Deliberately a description rather than a renderer: it holds the outer
/// terminal's size, which rows the tab bar and status bar took, which pane is
/// focused, and where each visible pane's grid sits. Everything else on screen —
/// gutters, borders, the space a header does not fill — is chrome by
/// construction, because a cell that is not in a pane's grid cannot be the
/// application's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLayout {
    size: Size,
    tab_row: Option<u16>,
    status_row: Option<u16>,
    focused: Option<PaneId>,
    panes: Vec<PaneArea>,
}

impl ScreenLayout {
    /// An empty screen of `size`: no chrome rows and no panes, so every cell is
    /// chrome.
    #[must_use]
    pub fn new(size: Size) -> Self {
        Self {
            size,
            tab_row: None,
            status_row: None,
            focused: None,
            panes: Vec::new(),
        }
    }

    /// One pane filling the whole terminal, with no chrome at all.
    ///
    /// The local smoke path's screen: a single grid, no tab row, no status bar,
    /// and no header. Every report is over the pane, which is what it has always
    /// been — now stated as a layout rather than assumed at the call site.
    #[must_use]
    pub fn single(size: Size, pane: PaneId) -> Self {
        Self::new(size)
            .focus(Some(pane))
            .pane(PaneArea::new(pane, 0, 0, size).headerless())
    }

    /// Records the terminal row the tab bar occupies.
    #[must_use]
    pub const fn tab_row(mut self, row: u16) -> Self {
        self.tab_row = Some(row);
        self
    }

    /// Records the terminal row the status bar occupies.
    #[must_use]
    pub const fn status_row(mut self, row: u16) -> Self {
        self.status_row = Some(row);
        self
    }

    /// Records which pane holds focus, and so whose modes the client has.
    #[must_use]
    pub const fn focus(mut self, pane: Option<PaneId>) -> Self {
        self.focused = pane;
        self
    }

    /// Adds one drawn pane.
    #[must_use]
    pub fn pane(mut self, area: PaneArea) -> Self {
        self.panes.push(area);
        self
    }

    /// The focused pane, if the screen has one.
    #[must_use]
    pub const fn focused(&self) -> Option<PaneId> {
        self.focused
    }

    /// Every pane on the screen, in the order they were added.
    #[must_use]
    pub fn panes(&self) -> &[PaneArea] {
        &self.panes
    }

    /// Where `(col, row)` landed.
    ///
    /// The order is the safety property, not a detail. Off-screen is answered
    /// first, then the chrome rows, and only then the panes: a layout that
    /// wrongly described a pane as overlapping the status bar still cannot
    /// deliver a status-bar click into a child, because the row is claimed
    /// before any pane is consulted. A header is checked after the grids for the
    /// same reason in reverse — a header row belongs to chrome, but it may never
    /// swallow a cell some pane's grid actually occupies.
    #[must_use]
    pub fn hit(&self, col: u16, row: u16) -> MouseTarget {
        if col >= self.size.cols || row >= self.size.rows {
            return MouseTarget::Chrome(ChromeTarget::Outside);
        }
        if self.tab_row == Some(row) {
            return MouseTarget::Chrome(ChromeTarget::TabRow { col });
        }
        if self.status_row == Some(row) {
            return MouseTarget::Chrome(ChromeTarget::StatusBar { col });
        }
        for area in &self.panes {
            if let Some(at) = area.local(col, row) {
                return MouseTarget::Pane {
                    pane: area.pane,
                    at,
                };
            }
        }
        for area in &self.panes {
            if area.on_header(col, row) {
                return MouseTarget::Chrome(ChromeTarget::Header {
                    pane: area.pane,
                    col: col - area.x,
                });
            }
        }
        MouseTarget::Chrome(ChromeTarget::Gutter)
    }
}

/// Where a mouse report landed on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTarget {
    /// Inside a pane's grid, at a pane-local cell.
    Pane {
        /// The pane whose grid holds the cell.
        pane: PaneId,
        /// The cell, zero-based within that pane.
        at: Point,
    },
    /// On a piece of cloo's own chrome.
    Chrome(ChromeTarget),
}

impl MouseTarget {
    /// The pane whose grid was hit, or `None` for every chrome target.
    ///
    /// Deliberately not "the pane this is about": a header names a pane too, and
    /// answering with it here would let a caller treat a border click as a click
    /// on the pane's contents.
    #[must_use]
    pub const fn pane(&self) -> Option<PaneId> {
        match self {
            Self::Pane { pane, .. } => Some(*pane),
            Self::Chrome(_) => None,
        }
    }
}

/// Which piece of chrome a mouse report landed on.
///
/// Every variant carries enough to act on without a second hit test, because the
/// thing that will act on it — click-to-focus, a tab click, a gutter drag — is a
/// different layer that must not re-derive geometry the renderer already knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeTarget {
    /// The tab row, at a terminal column.
    TabRow {
        /// The column, from the left of the terminal.
        col: u16,
    },
    /// The always-on status bar, at a terminal column.
    StatusBar {
        /// The column, from the left of the terminal.
        col: u16,
    },
    /// A pane's header row, which is that pane's top border.
    Header {
        /// The pane the header describes.
        pane: PaneId,
        /// The column, from the left of the header.
        col: u16,
    },
    /// Inside a pane's grid, but the application does not own it: it is not
    /// tracking the mouse, the user held the shift override, or the pane is not
    /// the one whose modes cloo holds. This is the event click-to-focus is made
    /// of.
    PaneBody {
        /// The pane whose grid was clicked.
        pane: PaneId,
        /// The cell, zero-based within that pane.
        at: Point,
    },
    /// The space between panes.
    Gutter,
    /// Off the described screen entirely.
    Outside,
}

/// Where one mouse report is going.
///
/// The two arms are deliberately different shapes. An application event is
/// already the [`MouseEvent`] the wire takes, so sending it is one call with
/// nothing left to decide; a chrome event is a [`ChromeTarget`] and cannot be
/// sent at all, which is how "a chrome event never reaches the wire" stops being
/// a rule someone has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRoute {
    /// The pane's application owns it. Send it with `Attached::send_mouse`.
    Application(MouseEvent),
    /// cloo's chrome owns it. It never crosses the wire.
    Chrome(ChromeTarget),
}

impl MouseRoute {
    /// The wire event, if this route has one.
    ///
    /// `None` for every chrome route, which is the whole of what a caller has to
    /// know to keep a chrome click out of a child's input.
    #[must_use]
    pub const fn wire_event(&self) -> Option<&MouseEvent> {
        match self {
            Self::Application(event) => Some(event),
            Self::Chrome(_) => None,
        }
    }
}

/// Routes one decoded mouse report against the screen the client drew.
///
/// `modes` describes the *focused* pane's application, because that is the only
/// pane the server reports modes for. A report over any other pane is therefore
/// chrome: cloo does not know whether that application is tracking the mouse,
/// and the honest answer to "is this the application's?" for an application
/// whose modes are unknown is no. That is also the behaviour a user expects —
/// clicking an unfocused pane selects it rather than poking whatever runs there.
#[must_use]
pub fn route_mouse(layout: &ScreenLayout, modes: PaneModes, report: &MouseReport) -> MouseRoute {
    match layout.hit(report.col, report.row) {
        MouseTarget::Chrome(target) => MouseRoute::Chrome(target),
        MouseTarget::Pane { pane, at } => {
            let known = layout.focused() == Some(pane);
            if mouse_owner(modes, report, known) == MouseOwner::Application {
                MouseRoute::Application(MouseEvent {
                    pane,
                    at,
                    kind: report.kind,
                    mods: report.mods,
                })
            } else {
                MouseRoute::Chrome(ChromeTarget::PaneBody { pane, at })
            }
        }
    }
}

/// A keyboard action against the attention queue overlay.
///
/// The overlay is a navigation surface: the user walks the entries, jumps to the
/// pane one names, or dismisses one. These are cloo's own actions and never
/// reach a child — the overlay owns the keyboard while it is open, exactly as
/// chrome owns a mouse click over a border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    /// Move the cursor to the next (older) entry.
    Next,
    /// Move the cursor to the previous (newer) entry.
    Prev,
    /// Focus the pane the selected entry names.
    Focus,
    /// Acknowledge and remove the selected entry.
    Acknowledge,
    /// Close the overlay.
    Dismiss,
}

/// Maps a run of decoded key bytes to a queue action, or `None` if unbound.
///
/// The bindings are conventional and deliberately self-contained: arrow keys and
/// `j`/`k` navigate, Enter focuses, `a` or Space acknowledges, and Escape or `q`
/// dismisses. The configurable keymap lands in M4 and supersedes them; until
/// then this is enough to drive the overlay and to test its actions in
/// isolation.
#[must_use]
pub fn queue_action(keys: &[u8]) -> Option<QueueAction> {
    match keys {
        b"j" | b"\x1b[B" => Some(QueueAction::Next),
        b"k" | b"\x1b[A" => Some(QueueAction::Prev),
        b"\r" | b"\n" => Some(QueueAction::Focus),
        b"a" | b" " => Some(QueueAction::Acknowledge),
        b"\x1b" | b"q" => Some(QueueAction::Dismiss),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_modes() -> OuterModes {
        OuterModes {
            bracketed_paste: true,
            sgr_mouse: true,
            focus_events: true,
            extended_keys: true,
        }
    }

    fn decoder() -> InputDecoder {
        InputDecoder::new(all_modes())
    }

    fn tracking(level: MouseTracking) -> PaneModes {
        PaneModes {
            mouse: level,
            ..PaneModes::default()
        }
    }

    fn report(kind: MouseKind) -> MouseReport {
        MouseReport {
            kind,
            mods: MouseMods::NONE,
            col: 4,
            row: 2,
        }
    }

    // -- negotiating with the outer terminal --------------------------------

    #[test]
    fn only_negotiated_modes_are_requested() {
        let caps = TermCaps {
            bracketed_paste: true,
            focus_events: true,
            ..TermCaps::default()
        };
        let modes = OuterModes::negotiated(caps);
        assert!(modes.bracketed_paste && modes.focus_events);
        assert!(
            !modes.sgr_mouse && !modes.extended_keys,
            "a mode the terminal did not report is never asked for: its enable \
             sequence would be printed on the user's screen"
        );
    }

    /// One fixture per negotiated mode: enabling it emits its own sequence and
    /// no other, and disabling it emits exactly the matching reset.
    #[test]
    fn every_negotiated_mode_has_a_request_and_a_matching_reset() {
        /// A named fixture: set the mode, then its request and its reset.
        type ModeCase = (
            &'static str,
            fn(&mut OuterModes),
            &'static [u8],
            &'static [u8],
        );
        let cases: [ModeCase; 4] = [
            (
                "bracketed paste",
                |m| m.bracketed_paste = true,
                PASTE_ON,
                PASTE_OFF,
            ),
            ("SGR mouse", |m| m.sgr_mouse = true, MOUSE_ON, MOUSE_OFF),
            (
                "focus events",
                |m| m.focus_events = true,
                FOCUS_ON,
                FOCUS_OFF,
            ),
            (
                "extended keys",
                |m| m.extended_keys = true,
                KEYS_ON,
                KEYS_OFF,
            ),
        ];

        for (name, set, on, off) in cases {
            let mut modes = OuterModes::none();
            set(&mut modes);
            assert_eq!(modes.enable(), on, "{name} enable");
            assert_eq!(modes.disable(), off, "{name} disable");
        }
    }

    #[test]
    fn asking_for_nothing_writes_nothing() {
        assert!(OuterModes::none().enable().is_empty());
        assert!(OuterModes::none().disable().is_empty());
    }

    #[test]
    fn every_mode_turned_on_is_turned_off_again() {
        let enable = all_modes().enable();
        let disable = all_modes().disable();
        for (set, reset) in [
            (PASTE_ON, PASTE_OFF),
            (MOUSE_ON, MOUSE_OFF),
            (FOCUS_ON, FOCUS_OFF),
            (KEYS_ON, KEYS_OFF),
        ] {
            assert!(find(&enable, set).is_some());
            assert!(
                find(&disable, reset).is_some(),
                "a mode left on outlives cloo and breaks the user's shell"
            );
        }
    }

    // -- decoding -----------------------------------------------------------

    #[test]
    fn ordinary_typing_is_one_run_of_keys() {
        assert_eq!(
            decoder().feed(b"ls -la\r"),
            vec![InputEvent::Keys(b"ls -la\r".to_vec())]
        );
    }

    #[test]
    fn a_paste_arrives_as_text_without_its_brackets() {
        assert_eq!(
            decoder().feed(b"\x1b[200~echo hi\x1b[201~"),
            vec![InputEvent::Paste(b"echo hi".to_vec())],
            "the brackets are the server's to re-apply, if the child wants them"
        );
    }

    #[test]
    fn typing_around_a_paste_stays_in_order() {
        assert_eq!(
            decoder().feed(b"a\x1b[200~pasted\x1b[201~b"),
            vec![
                InputEvent::Keys(b"a".to_vec()),
                InputEvent::Paste(b"pasted".to_vec()),
                InputEvent::Keys(b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn a_paste_split_across_reads_is_still_one_paste() {
        let mut decoder = decoder();
        assert!(decoder.feed(b"\x1b[200~one").is_empty());
        assert!(decoder.feed(b" two").is_empty());
        // The terminator itself split down the middle.
        assert!(decoder.feed(b"\x1b[20").is_empty());
        assert_eq!(
            decoder.feed(b"1~"),
            vec![InputEvent::Paste(b"one two".to_vec())]
        );
    }

    #[test]
    fn a_half_arrived_paste_is_never_flushed_as_keystrokes() {
        let mut decoder = decoder();
        let _ = decoder.feed(b"\x1b[200~rm -rf /");
        assert_eq!(
            decoder.flush(),
            None,
            "flushing half a paste is exactly what bracketed paste prevents"
        );
    }

    #[test]
    fn focus_reports_are_decoded_in_both_directions() {
        assert_eq!(
            decoder().feed(b"\x1b[I\x1b[O"),
            vec![InputEvent::Focus(true), InputEvent::Focus(false)]
        );
    }

    #[test]
    fn a_lone_escape_is_released_by_a_flush() {
        let mut decoder = decoder();
        assert!(
            decoder.feed(b"\x1b").is_empty(),
            "it could still become a sequence"
        );
        assert!(decoder.is_holding());
        assert_eq!(decoder.flush(), Some(InputEvent::Keys(vec![0x1b])));
        assert!(!decoder.is_holding());
    }

    #[test]
    fn a_sequence_for_a_mode_that_was_never_requested_is_left_alone() {
        let mut decoder = InputDecoder::new(OuterModes::none());
        assert_eq!(
            decoder.feed(b"\x1b[I"),
            vec![InputEvent::Keys(b"\x1b[I".to_vec())],
            "stealing this would corrupt input that belongs to the pane"
        );
    }

    #[test]
    fn an_unrecognised_escape_sequence_passes_through_as_keys() {
        assert_eq!(
            decoder().feed(b"\x1b[A"),
            vec![InputEvent::Keys(b"\x1b[A".to_vec())],
            "an arrow key is the pane's business, not the decoder's"
        );
    }

    /// One fixture per mouse event kind, each as its terminal sends it.
    #[test]
    fn every_mouse_report_kind_is_decoded() {
        let cases: [(&[u8], MouseKind); 6] = [
            (b"\x1b[<0;5;3M", MouseKind::Press(MouseButton::Left)),
            (b"\x1b[<1;5;3m", MouseKind::Release(MouseButton::Middle)),
            (b"\x1b[<64;5;3M", MouseKind::ScrollUp),
            (b"\x1b[<65;5;3M", MouseKind::ScrollDown),
            (
                b"\x1b[<34;5;3M",
                MouseKind::Motion(Some(MouseButton::Right)),
            ),
            (b"\x1b[<35;5;3M", MouseKind::Motion(None)),
        ];

        for (bytes, kind) in cases {
            assert_eq!(
                decoder().feed(bytes),
                vec![InputEvent::Mouse(report(kind))],
                "{kind:?}"
            );
        }
    }

    #[test]
    fn mouse_modifiers_are_recovered_from_the_button_code() {
        let events = decoder().feed(b"\x1b[<28;5;3M");
        let [InputEvent::Mouse(decoded)] = events.as_slice() else {
            panic!("expected one mouse event, got {events:?}");
        };
        assert_eq!(
            decoded.mods,
            MouseMods {
                shift: true,
                alt: true,
                ctrl: true,
            }
        );
        assert_eq!(decoded.kind, MouseKind::Press(MouseButton::Left));
    }

    #[test]
    fn mouse_coordinates_come_back_zero_based() {
        let events = decoder().feed(b"\x1b[<0;1;1M");
        let [InputEvent::Mouse(decoded)] = events.as_slice() else {
            panic!("expected one mouse event, got {events:?}");
        };
        assert_eq!((decoded.col, decoded.row), (0, 0), "SGR is one-based");
    }

    #[test]
    fn a_mouse_report_split_across_reads_is_held_then_decoded() {
        let mut decoder = decoder();
        assert!(decoder.feed(b"\x1b[<0;5;").is_empty());
        assert_eq!(
            decoder.feed(b"3M"),
            vec![InputEvent::Mouse(report(MouseKind::Press(
                MouseButton::Left
            )))]
        );
    }

    #[test]
    fn a_malformed_mouse_report_is_passed_through_rather_than_guessed_at() {
        assert_eq!(
            decoder().feed(b"\x1b[<;;M"),
            vec![InputEvent::Keys(b"\x1b[<;;M".to_vec())]
        );
    }

    // -- routing ------------------------------------------------------------

    #[test]
    fn an_application_tracking_the_mouse_owns_a_click_over_its_pane() {
        assert_eq!(
            mouse_owner(
                tracking(MouseTracking::Click),
                &report(MouseKind::Press(MouseButton::Left)),
                true,
            ),
            MouseOwner::Application
        );
    }

    #[test]
    fn an_application_not_tracking_the_mouse_never_owns_one() {
        assert_eq!(
            mouse_owner(
                tracking(MouseTracking::Off),
                &report(MouseKind::Press(MouseButton::Left)),
                true,
            ),
            MouseOwner::Chrome,
            "this is what makes click-to-focus work in an ordinary shell"
        );
    }

    #[test]
    fn a_click_outside_every_pane_is_chrome_whatever_the_application_wants() {
        assert_eq!(
            mouse_owner(
                tracking(MouseTracking::Motion),
                &report(MouseKind::Press(MouseButton::Left)),
                false,
            ),
            MouseOwner::Chrome
        );
    }

    #[test]
    fn shift_is_the_override_that_reaches_chrome_inside_a_full_screen_app() {
        let mut shifted = report(MouseKind::Press(MouseButton::Left));
        shifted.mods.shift = true;
        assert_eq!(
            mouse_owner(tracking(MouseTracking::Motion), &shifted, true),
            MouseOwner::Chrome
        );
        shifted.mods.shift = false;
        assert_eq!(
            mouse_owner(tracking(MouseTracking::Motion), &shifted, true),
            MouseOwner::Application,
            "and without it the application still gets everything"
        );
    }

    // -- hit testing --------------------------------------------------------

    /// Two panes side by side under a tab row, over a status bar:
    ///
    /// ```text
    ///  row 0        tab row
    ///  row 1        header(1)              header(2)
    ///  rows 2..=8   pane 1 grid, cols 0..=9   |   pane 2 grid, cols 11..=19
    ///  row 9        status bar
    ///  col 10       gutter
    /// ```
    fn screen() -> ScreenLayout {
        ScreenLayout::new(Size::new(20, 10))
            .tab_row(0)
            .status_row(9)
            .focus(Some(PaneId::new(1)))
            .pane(PaneArea::new(PaneId::new(1), 0, 2, Size::new(10, 7)))
            .pane(PaneArea::new(PaneId::new(2), 11, 2, Size::new(9, 7)))
    }

    fn at(col: u16, row: u16) -> MouseReport {
        MouseReport {
            kind: MouseKind::Press(MouseButton::Left),
            mods: MouseMods::NONE,
            col,
            row,
        }
    }

    #[test]
    fn every_region_of_a_drawn_screen_hit_tests_to_itself() {
        let screen = screen();
        let cases: [(&str, u16, u16, MouseTarget); 7] = [
            (
                "the tab row",
                3,
                0,
                MouseTarget::Chrome(ChromeTarget::TabRow { col: 3 }),
            ),
            (
                "a pane header",
                4,
                1,
                MouseTarget::Chrome(ChromeTarget::Header {
                    pane: PaneId::new(1),
                    col: 4,
                }),
            ),
            (
                "the left pane's grid",
                2,
                3,
                MouseTarget::Pane {
                    pane: PaneId::new(1),
                    at: Point::new(2, 1),
                },
            ),
            (
                "the right pane's grid",
                12,
                4,
                MouseTarget::Pane {
                    pane: PaneId::new(2),
                    at: Point::new(1, 2),
                },
            ),
            (
                "the gutter between them",
                10,
                4,
                MouseTarget::Chrome(ChromeTarget::Gutter),
            ),
            (
                "the status bar",
                7,
                9,
                MouseTarget::Chrome(ChromeTarget::StatusBar { col: 7 }),
            ),
            (
                "past the right edge",
                20,
                4,
                MouseTarget::Chrome(ChromeTarget::Outside),
            ),
        ];

        for (name, col, row, expected) in cases {
            assert_eq!(screen.hit(col, row), expected, "{name}");
        }
    }

    #[test]
    fn a_pane_local_cell_is_zero_based_within_its_own_grid() {
        // The top-left cell of the right pane, which sits at terminal (11, 2).
        assert_eq!(
            screen().hit(11, 2),
            MouseTarget::Pane {
                pane: PaneId::new(2),
                at: Point::new(0, 0),
            },
            "the server encodes what the application sees, and it has never \
             heard of the pane's place on a screen"
        );
    }

    #[test]
    fn a_chrome_row_is_claimed_before_any_pane_is_consulted() {
        // A layout that wrongly describes a pane as reaching over the status
        // row. The row still answers as the status bar.
        let screen = ScreenLayout::new(Size::new(20, 10))
            .status_row(9)
            .focus(Some(PaneId::new(1)))
            .pane(PaneArea::new(PaneId::new(1), 0, 0, Size::new(20, 10)));
        assert_eq!(
            screen.hit(4, 9),
            MouseTarget::Chrome(ChromeTarget::StatusBar { col: 4 }),
            "a mis-described pane must not be able to swallow a chrome row"
        );
    }

    #[test]
    fn a_header_never_swallows_a_cell_some_pane_actually_occupies() {
        // Pane 2's grid starts on the same row pane 1's header would claim.
        let screen = ScreenLayout::new(Size::new(20, 10))
            .pane(PaneArea::new(PaneId::new(1), 0, 5, Size::new(20, 4)))
            .pane(PaneArea::new(PaneId::new(2), 0, 0, Size::new(20, 5)));
        assert_eq!(
            screen.hit(3, 4),
            MouseTarget::Pane {
                pane: PaneId::new(2),
                at: Point::new(3, 4),
            }
        );
    }

    #[test]
    fn a_pane_at_the_top_of_the_screen_has_no_header_row_above_it() {
        let screen = ScreenLayout::new(Size::new(20, 10)).pane(PaneArea::new(
            PaneId::new(1),
            0,
            0,
            Size::new(20, 10),
        ));
        assert_eq!(screen.hit(3, 0).pane(), Some(PaneId::new(1)));
    }

    #[test]
    fn the_local_screen_is_one_pane_and_no_chrome() {
        let screen = ScreenLayout::single(Size::new(80, 24), PaneId::new(1));
        assert_eq!(
            screen.hit(0, 0),
            MouseTarget::Pane {
                pane: PaneId::new(1),
                at: Point::new(0, 0),
            }
        );
        assert_eq!(
            screen.hit(79, 23),
            MouseTarget::Pane {
                pane: PaneId::new(1),
                at: Point::new(79, 23),
            }
        );
        assert_eq!(
            screen.hit(80, 23),
            MouseTarget::Chrome(ChromeTarget::Outside)
        );
    }

    // -- routing over a screen ----------------------------------------------

    #[test]
    fn an_application_event_is_routed_as_the_wire_event_the_server_takes() {
        let route = route_mouse(&screen(), tracking(MouseTracking::Click), &at(2, 3));
        assert_eq!(
            route,
            MouseRoute::Application(MouseEvent {
                pane: PaneId::new(1),
                at: Point::new(2, 1),
                kind: MouseKind::Press(MouseButton::Left),
                mods: MouseMods::NONE,
            }),
            "an application tracking the mouse is not stolen from"
        );
        assert!(route.wire_event().is_some());
    }

    /// Every chrome region routes to chrome and produces no wire event, which
    /// is the property that keeps escape bytes out of a child's input.
    #[test]
    fn no_chrome_region_ever_produces_a_wire_event() {
        let screen = screen();
        // The most permissive application state there is: full motion tracking
        // in the focused pane. Even so, none of these are its business.
        let modes = tracking(MouseTracking::Motion);
        let cases: [(&str, u16, u16, ChromeTarget); 5] = [
            ("the tab row", 3, 0, ChromeTarget::TabRow { col: 3 }),
            (
                "a pane header",
                4,
                1,
                ChromeTarget::Header {
                    pane: PaneId::new(1),
                    col: 4,
                },
            ),
            ("the gutter", 10, 4, ChromeTarget::Gutter),
            ("the status bar", 7, 9, ChromeTarget::StatusBar { col: 7 }),
            ("off screen", 40, 40, ChromeTarget::Outside),
        ];

        for (name, col, row, target) in cases {
            let route = route_mouse(&screen, modes, &at(col, row));
            assert_eq!(route, MouseRoute::Chrome(target), "{name}");
            assert_eq!(
                route.wire_event(),
                None,
                "{name} must have nothing a caller could send"
            );
        }
    }

    #[test]
    fn a_click_in_an_unfocused_pane_is_chrome_because_its_modes_are_unknown() {
        // The modes belong to the focused pane; pane 2's are not reported at
        // all, and guessing at them is what would steal or invent an event.
        let route = route_mouse(&screen(), tracking(MouseTracking::Motion), &at(12, 4));
        assert_eq!(
            route,
            MouseRoute::Chrome(ChromeTarget::PaneBody {
                pane: PaneId::new(2),
                at: Point::new(1, 2),
            }),
            "and naming the pane is what click-to-focus is made of"
        );
    }

    #[test]
    fn a_click_in_a_shell_that_tracks_nothing_is_chrome_that_still_names_the_pane() {
        assert_eq!(
            route_mouse(&screen(), tracking(MouseTracking::Off), &at(2, 3)),
            MouseRoute::Chrome(ChromeTarget::PaneBody {
                pane: PaneId::new(1),
                at: Point::new(2, 1),
            })
        );
    }

    #[test]
    fn shift_takes_an_event_back_from_a_full_screen_application() {
        let mut shifted = at(2, 3);
        shifted.mods.shift = true;
        assert_eq!(
            route_mouse(&screen(), tracking(MouseTracking::Motion), &shifted),
            MouseRoute::Chrome(ChromeTarget::PaneBody {
                pane: PaneId::new(1),
                at: Point::new(2, 1),
            })
        );
    }

    /// Motion under click-only tracking is still the application's to refuse,
    /// not the client's to reroute: the tracking *level* is a server-side
    /// encoding question, and rerouting it here would turn a drag over a pane
    /// into a chrome action the user did not ask for.
    #[test]
    fn a_tracking_level_below_the_event_is_left_for_the_server_to_drop() {
        let motion = MouseReport {
            kind: MouseKind::Motion(None),
            mods: MouseMods::NONE,
            col: 2,
            row: 3,
        };
        assert!(matches!(
            route_mouse(&screen(), tracking(MouseTracking::Click), &motion),
            MouseRoute::Application(_)
        ));
    }

    // -- queue actions ------------------------------------------------------

    #[test]
    fn queue_keys_map_to_navigation_and_actions() {
        let cases: [(&[u8], QueueAction); 10] = [
            (b"j", QueueAction::Next),
            (b"\x1b[B", QueueAction::Next),
            (b"k", QueueAction::Prev),
            (b"\x1b[A", QueueAction::Prev),
            (b"\r", QueueAction::Focus),
            (b"\n", QueueAction::Focus),
            (b"a", QueueAction::Acknowledge),
            (b" ", QueueAction::Acknowledge),
            (b"\x1b", QueueAction::Dismiss),
            (b"q", QueueAction::Dismiss),
        ];
        for (keys, action) in cases {
            assert_eq!(queue_action(keys), Some(action), "{keys:?}");
        }
    }

    #[test]
    fn an_unbound_key_is_not_a_queue_action() {
        assert_eq!(queue_action(b"x"), None);
        assert_eq!(queue_action(b""), None);
    }
}
