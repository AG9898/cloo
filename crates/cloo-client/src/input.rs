//! What the user's terminal sends, turned into typed events cloo can route.
//!
//! Four things live here, and they compose in one direction:
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
//! 4. [`decode_key`] and [`KeyRouter`] — the keyboard's half of the same
//!    ownership question. `cloo-core` owns what a chord is *called* and what it
//!    is bound to; this is where a run of bytes becomes a [`Key`] and where the
//!    prefix state machine decides whether a chord is cloo's at all.
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
//!
//! The keyboard's ownership rule is narrower and stricter: **nothing is cloo's
//! until the prefix is pressed.** [`KeyRouter`] passes every byte through
//! verbatim — the same slice it was handed, never re-encoded — until it decodes
//! the prefix chord, and only the single chord after that prefix is looked up.
//! An application using `c`, `x`, or an arrow key is therefore untouched, which
//! is the whole reason a multiplexer can sit under a full-screen program.

use cloo_core::keymap::{Key, KeyCode, KeyMods, Keymap};
use cloo_proto::{
    Action, MouseButton, MouseEvent, MouseKind, MouseMods, MouseTracking, PaneId, PaneModes, Point,
    Size, TermCaps,
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

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// The characters `0x1c`–`0x1f` are control forms of, in order.
const CTRL_SYMBOLS: [char; 4] = ['\\', ']', '^', '_'];

/// Decodes the first chord in a run of key bytes, with the bytes it consumed.
///
/// `None` means "not a chord this function can name" — an escape sequence cloo
/// does not model, or a sequence that is not all here yet. A caller must treat
/// that as the pane's, never as an unrecognised command: bytes cloo cannot
/// explain belong to whatever is running, which is the same rule the decoder
/// above follows for a mode that was never negotiated.
///
/// The encodings are the conventional ones. `0x01`–`0x1a` are control letters,
/// `ESC` followed by anything but a sequence introducer is that chord with alt,
/// `CSI`/`SS3` carry the arrows, editing keys, and function keys, and a bare
/// `ESC` is Escape. Two deliberate splits: `0x0d` is Enter while `0x0a` is
/// `C-j`, and `0x7f` is Backspace while `0x08` is `C-h` — that is what those
/// keys actually send in raw mode, and collapsing either pair would make one of
/// the two unbindable.
#[must_use]
pub fn decode_key(bytes: &[u8]) -> Option<(Key, usize)> {
    let first = *bytes.first()?;
    match first {
        0x1b => decode_escape(bytes),
        0x0d => Some((Key::code(KeyCode::Enter), 1)),
        0x09 => Some((Key::code(KeyCode::Tab), 1)),
        0x7f => Some((Key::code(KeyCode::Backspace), 1)),
        0x00 => Some((Key::ctrl(' '), 1)),
        0x01..=0x1a => Some((Key::ctrl(char::from(b'a' + first - 1)), 1)),
        0x1c..=0x1f => Some((Key::ctrl(CTRL_SYMBOLS[usize::from(first - 0x1c)]), 1)),
        _ => decode_char(bytes),
    }
}

/// Decodes one UTF-8 character as an unmodified chord.
fn decode_char(bytes: &[u8]) -> Option<(Key, usize)> {
    let lead = *bytes.first()?;
    let len = match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let ch = core::str::from_utf8(bytes.get(..len)?)
        .ok()?
        .chars()
        .next()?;
    Some((Key::char(ch), len))
}

/// Decodes a sequence that starts with `ESC`.
fn decode_escape(bytes: &[u8]) -> Option<(Key, usize)> {
    match bytes.get(1) {
        // Nothing after it in this run: a lone Escape. The run loop's flush is
        // what guarantees this is the whole story rather than a split sequence.
        None => Some((Key::code(KeyCode::Escape), 1)),
        Some(b'[') => decode_csi(bytes),
        Some(b'O') => decode_ss3(bytes),
        Some(_) => {
            let (key, len) = decode_key(&bytes[1..])?;
            Some((key.with_alt(), len + 1))
        }
    }
}

/// Decodes `ESC O x`: the arrows in application mode, and `F1`–`F4`.
fn decode_ss3(bytes: &[u8]) -> Option<(Key, usize)> {
    let code = match *bytes.get(2)? {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::Function(1),
        b'Q' => KeyCode::Function(2),
        b'R' => KeyCode::Function(3),
        b'S' => KeyCode::Function(4),
        _ => return None,
    };
    Some((Key::code(code), 3))
}

/// Decodes `ESC [ params final`, including the `;modifier` forms.
fn decode_csi(bytes: &[u8]) -> Option<(Key, usize)> {
    let mut params = [0_u32; 2];
    let mut field = 0;
    let mut index = 2;
    loop {
        match *bytes.get(index)? {
            byte @ b'0'..=b'9' => {
                if field < params.len() {
                    params[field] = params[field]
                        .saturating_mul(10)
                        .saturating_add(u32::from(byte - b'0'));
                }
            }
            b';' => field += 1,
            byte @ (b'A'..=b'Z' | b'~') => {
                let code = match byte {
                    b'A' => KeyCode::Up,
                    b'B' => KeyCode::Down,
                    b'C' => KeyCode::Right,
                    b'D' => KeyCode::Left,
                    b'H' => KeyCode::Home,
                    b'F' => KeyCode::End,
                    // `CSI Z` is back-tab, which is the only shifted chord a
                    // terminal spells with its own final byte.
                    b'Z' => return Some((Key::new(KeyCode::Tab, KeyMods::SHIFT), index + 1)),
                    b'~' => numbered_key(params[0])?,
                    _ => return None,
                };
                return Some((Key::new(code, csi_mods(params[1])), index + 1));
            }
            _ => return None,
        }
        index += 1;
    }
}

/// The editing and function keys spelled as `CSI n ~`.
fn numbered_key(number: u32) -> Option<KeyCode> {
    Some(match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::Function(u8::try_from(number - 10).ok()?),
        17..=21 => KeyCode::Function(u8::try_from(number - 11).ok()?),
        23 | 24 => KeyCode::Function(u8::try_from(number - 12).ok()?),
        _ => return None,
    })
}

/// The `1 + bitfield` modifier parameter terminals send as a CSI parameter.
fn csi_mods(param: u32) -> KeyMods {
    if param < 2 {
        return KeyMods::NONE;
    }
    let bits = param - 1;
    KeyMods {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
    }
}

/// What one run of key bytes turned into.
///
/// [`Pane`](Self::Pane) carries the user's bytes *unchanged* — a copy of the
/// slice that arrived, never a re-encoding of a decoded chord — because a client
/// that re-encoded would have to guess at the terminal's own conventions and
/// would corrupt input the moment it guessed differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// Bytes for the focused pane's child, exactly as the terminal sent them.
    Pane(Vec<u8>),
    /// The prefix was pressed. Nothing reaches the child; the next chord is
    /// cloo's. Worth surfacing because the status bar shows the pending prefix.
    Pending,
    /// A bound command. The client sends this as `ClientMessage::Command`.
    Command(Action),
    /// A chord after the prefix that no binding names. It is *consumed*: the
    /// user meant it for cloo, and passing it to the child instead is how a
    /// mistyped command ends up in a shell.
    Unbound,
}

/// The prefix state machine: which keys are the pane's and which are cloo's.
///
/// One rule, and it is the whole safety property: **outside a pending prefix,
/// every byte is the pane's.** A chord bound in the table means nothing until
/// the prefix has been pressed, so an application that uses `c`, `x`, `q`, or an
/// arrow key never notices cloo is there. Only the single chord after the prefix
/// is looked up, and whatever follows it in the same read is passed through
/// again.
///
/// Pressing the prefix twice sends the prefix itself to the child — tmux's
/// `send-prefix`, and the only way to type a `C-b` into a program that wants
/// one.
#[derive(Debug, Clone)]
pub struct KeyRouter {
    keymap: Keymap,
    pending: bool,
}

impl KeyRouter {
    /// A router over `keymap`, with no prefix pending.
    #[must_use]
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            pending: false,
        }
    }

    /// The keymap being resolved against.
    #[must_use]
    pub const fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Whether the prefix has been pressed and the next chord is cloo's.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// Forgets a pending prefix.
    ///
    /// For the events that end a keystroke's context — losing focus, or an
    /// overlay taking the keyboard — where leaving it pending would silently
    /// swallow the user's next keystroke.
    pub const fn reset(&mut self) {
        self.pending = false;
    }

    /// Replaces the keymap, keeping any pending prefix.
    ///
    /// A `SIGHUP` reload is allowed to change the bindings under a user who is
    /// mid-chord; what it must not do is drop the chord they already pressed.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    /// Routes one run of decoded key bytes.
    ///
    /// Pass-through bytes are coalesced: an ordinary line of typing is one
    /// [`KeyRoute::Pane`], and a run is split only where a prefix interrupts it.
    pub fn feed(&mut self, keys: &[u8]) -> Vec<KeyRoute> {
        let mut routes = Vec::new();
        // Where the current run of pass-through bytes started. Everything from
        // here to the cursor is the pane's and is emitted verbatim.
        let mut from = 0;
        let mut index = 0;

        while index < keys.len() {
            let decoded = decode_key(&keys[index..]);
            if self.pending {
                self.pending = false;
                match decoded {
                    Some((key, len)) => {
                        if let Some(action) = self.keymap.action(key) {
                            routes.push(KeyRoute::Command(action.clone()));
                        } else if key == self.keymap.prefix() {
                            routes.push(KeyRoute::Pane(keys[index..index + len].to_vec()));
                        } else {
                            routes.push(KeyRoute::Unbound);
                        }
                        index += len;
                    }
                    // Undecodable after the prefix: the user was talking to
                    // cloo, so the rest of the run is consumed rather than
                    // delivered to a child as a fragment.
                    None => {
                        routes.push(KeyRoute::Unbound);
                        index = keys.len();
                    }
                }
                from = index;
                continue;
            }

            match decoded {
                Some((key, len)) if key == self.keymap.prefix() => {
                    if from < index {
                        routes.push(KeyRoute::Pane(keys[from..index].to_vec()));
                    }
                    index += len;
                    from = index;
                    self.pending = true;
                    routes.push(KeyRoute::Pending);
                }
                Some((_, len)) => index += len,
                // Not a chord cloo can name, so it is the pane's — along with
                // everything after it, which cannot be scanned safely without
                // knowing where this sequence ends.
                None => index = keys.len(),
            }
        }

        if from < keys.len() {
            routes.push(KeyRoute::Pane(keys[from..].to_vec()));
        }
        routes
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

/// A keyboard action against an open overlay.
///
/// The session switcher, the profile launcher, and the pane-details view are
/// one surface with one vocabulary — see [`crate::overlay`]. Like
/// [`QueueAction`], these are cloo's own actions: an open overlay owns the
/// keyboard, and none of this reaches a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAction {
    /// Move the cursor one row down.
    Next,
    /// Move the cursor one row up.
    Prev,
    /// Jump to the first row.
    First,
    /// Jump to the last row.
    Last,
    /// Act on the selected row.
    Confirm,
    /// Close the overlay without acting.
    Dismiss,
}

/// Maps a run of decoded key bytes to an overlay action, or `None` if unbound.
///
/// The bindings mirror [`queue_action`]'s so the overlays and the attention
/// queue are one habit: arrows and `j`/`k` navigate, `g`/`G` and Home/End jump
/// to the ends, Enter confirms, and Escape or `q` dismisses. The configurable
/// keymap lands in M4 and supersedes them.
///
/// Escape is bound in every overlay, deliberately and without exception: an
/// overlay that could not be closed would hold the user's terminal.
#[must_use]
pub fn overlay_action(keys: &[u8]) -> Option<OverlayAction> {
    match keys {
        b"j" | b"\x1b[B" => Some(OverlayAction::Next),
        b"k" | b"\x1b[A" => Some(OverlayAction::Prev),
        b"g" | b"\x1b[H" => Some(OverlayAction::First),
        b"G" | b"\x1b[F" => Some(OverlayAction::Last),
        b"\r" | b"\n" => Some(OverlayAction::Confirm),
        b"\x1b" | b"q" => Some(OverlayAction::Dismiss),
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

    // -- decoding chords ----------------------------------------------------

    /// The bytes each default-bound chord arrives as, which is also the table
    /// the pass-through property below is proved over.
    const DEFAULT_CHORDS: [(&[u8], &str); 18] = [
        (b"%", "%"),
        (b"\"", "\""),
        (b"x", "x"),
        (b"h", "h"),
        (b"j", "j"),
        (b"k", "k"),
        (b"l", "l"),
        (b"\x1b[D", "left"),
        (b"\x1b[B", "down"),
        (b"\x1b[A", "up"),
        (b"\x1b[C", "right"),
        (b"z", "z"),
        (b"c", "c"),
        (b"&", "&"),
        (b"n", "n"),
        (b"p", "p"),
        (b"[", "["),
        (b"d", "d"),
    ];

    fn chord(text: &str) -> Key {
        Key::parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    fn router() -> KeyRouter {
        KeyRouter::new(Keymap::defaults())
    }

    /// One fixture per encoding a terminal actually uses, checked against the
    /// spelling `cloo-core` would parse. This is the join between the two
    /// halves: a chord a user can write must be a chord a terminal can send.
    #[test]
    fn every_chord_a_terminal_sends_decodes_to_its_spelling() {
        let cases: [(&[u8], &str); 20] = [
            (b"a", "a"),
            (b"G", "G"),
            (b" ", "space"),
            (b"\x02", "C-b"),
            (b"\x00", "C-space"),
            (b"\x1f", "C-_"),
            (b"\x0a", "C-j"),
            (b"\x08", "C-h"),
            (b"\r", "enter"),
            (b"\t", "tab"),
            (b"\x7f", "backspace"),
            (b"\x1b", "escape"),
            (b"\x1bx", "M-x"),
            (b"\x1b\x02", "C-M-b"),
            (b"\x1b[Z", "S-tab"),
            (b"\x1bOA", "up"),
            (b"\x1b[1;5A", "C-up"),
            (b"\x1b[3~", "delete"),
            (b"\x1b[6~", "pagedown"),
            (b"\x1b[24~", "f12"),
        ];
        for (bytes, spelling) in cases {
            assert_eq!(
                decode_key(bytes),
                Some((chord(spelling), bytes.len())),
                "{bytes:?} is {spelling}"
            );
        }
    }

    #[test]
    fn a_multi_byte_character_is_one_chord() {
        assert_eq!(decode_key("é".as_bytes()), Some((Key::char('é'), 2)));
    }

    #[test]
    fn a_sequence_cloo_does_not_model_is_not_a_chord() {
        // Answering with a guess here is what would steal a reply or a key an
        // application is using.
        assert_eq!(decode_key(b"\x1b[?1;2c"), None);
        assert_eq!(decode_key(b"\x1b["), None, "and neither is half of one");
        assert_eq!(decode_key(b""), None);
    }

    // -- the prefix state machine -------------------------------------------

    /// The acceptance property: outside a pending prefix nothing is consumed,
    /// not even a chord the keymap binds. An application using `c`, `x`, or an
    /// arrow key must never notice cloo is there.
    #[test]
    fn a_bound_chord_without_the_prefix_is_still_the_panes() {
        for (bytes, spelling) in DEFAULT_CHORDS {
            let mut router = router();
            assert!(
                router.keymap().action(chord(spelling)).is_some(),
                "{spelling} is bound, so this fixture is not vacuous"
            );
            assert_eq!(
                router.feed(bytes),
                vec![KeyRoute::Pane(bytes.to_vec())],
                "{spelling} without the prefix is the pane's"
            );
            assert!(!router.is_pending());
        }
    }

    #[test]
    fn ordinary_typing_is_passed_through_byte_for_byte() {
        assert_eq!(
            router().feed(b"ls -la\r"),
            vec![KeyRoute::Pane(b"ls -la\r".to_vec())],
            "one run, and the same bytes the terminal sent"
        );
    }

    #[test]
    fn the_prefix_is_held_and_the_next_chord_resolves() {
        let mut router = router();
        assert_eq!(router.feed(b"\x02"), vec![KeyRoute::Pending]);
        assert!(router.is_pending(), "nothing reached the child");
        assert_eq!(
            router.feed(b"c"),
            vec![KeyRoute::Command(Action::NewTab)],
            "and the chord after it is cloo's"
        );
        assert!(!router.is_pending());
    }

    #[test]
    fn a_prefix_and_its_chord_in_one_read_are_still_one_command() {
        assert_eq!(
            router().feed(b"\x02%"),
            vec![KeyRoute::Pending, KeyRoute::Command(Action::SplitVertical)]
        );
    }

    #[test]
    fn an_escape_sequence_after_the_prefix_resolves_as_a_chord() {
        assert_eq!(
            router().feed(b"\x02\x1b[D"),
            vec![KeyRoute::Pending, KeyRoute::Command(Action::FocusLeft)]
        );
    }

    #[test]
    fn typing_around_a_command_keeps_its_order_and_its_bytes() {
        assert_eq!(
            router().feed(b"ab\x02cdef"),
            vec![
                KeyRoute::Pane(b"ab".to_vec()),
                KeyRoute::Pending,
                KeyRoute::Command(Action::NewTab),
                KeyRoute::Pane(b"def".to_vec()),
            ],
            "only the one chord after the prefix is taken"
        );
    }

    #[test]
    fn an_unbound_chord_after_the_prefix_is_consumed_rather_than_typed() {
        // Delivering it instead is how a mistyped command ends up in a shell.
        let mut router = router();
        assert_eq!(
            router.feed(b"\x02Q"),
            vec![KeyRoute::Pending, KeyRoute::Unbound]
        );
        assert!(!router.is_pending());
    }

    #[test]
    fn an_undecodable_sequence_after_the_prefix_is_consumed_whole() {
        assert_eq!(
            router().feed(b"\x02\x1b[?1;2c"),
            vec![KeyRoute::Pending, KeyRoute::Unbound],
            "a fragment of what the user meant for cloo is not the child's"
        );
    }

    #[test]
    fn a_sequence_cloo_cannot_name_is_the_panes_along_with_the_rest_of_the_run() {
        let bytes: &[u8] = b"\x1b[?1;2crest";
        assert_eq!(router().feed(bytes), vec![KeyRoute::Pane(bytes.to_vec())]);
    }

    #[test]
    fn pressing_the_prefix_twice_sends_it_to_the_child() {
        // tmux's `send-prefix`, and the only way to type a `C-b` into a program
        // that wants one.
        assert_eq!(
            router().feed(b"\x02\x02"),
            vec![KeyRoute::Pending, KeyRoute::Pane(b"\x02".to_vec())]
        );
    }

    #[test]
    fn a_rebound_prefix_gives_the_old_one_back_to_the_pane() {
        let mut keymap = Keymap::defaults();
        keymap.set_prefix(chord("C-a"));
        let mut router = KeyRouter::new(keymap);
        assert_eq!(
            router.feed(b"\x02c"),
            vec![KeyRoute::Pane(b"\x02c".to_vec())],
            "C-b is an application's key again"
        );
        assert_eq!(
            router.feed(b"\x01c"),
            vec![KeyRoute::Pending, KeyRoute::Command(Action::NewTab)]
        );
    }

    #[test]
    fn a_configured_binding_resolves_through_the_router() {
        let mut keymap = Keymap::defaults();
        keymap.bind(chord("|"), Action::SplitVertical);
        keymap.unbind(chord("x"));
        let mut router = KeyRouter::new(keymap);
        assert_eq!(
            router.feed(b"\x02|"),
            vec![KeyRoute::Pending, KeyRoute::Command(Action::SplitVertical)]
        );
        assert_eq!(
            router.feed(b"\x02x"),
            vec![KeyRoute::Pending, KeyRoute::Unbound],
            "an unbound default is unbound, not a fallback to what it was"
        );
    }

    #[test]
    fn a_reset_forgets_a_pending_prefix() {
        let mut router = router();
        let _ = router.feed(b"\x02");
        router.reset();
        assert_eq!(
            router.feed(b"c"),
            vec![KeyRoute::Pane(b"c".to_vec())],
            "the context that made it cloo's is gone"
        );
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

    // -- overlay actions ----------------------------------------------------

    #[test]
    fn overlay_keys_map_to_navigation_and_actions() {
        let cases: [(&[u8], OverlayAction); 12] = [
            (b"j", OverlayAction::Next),
            (b"\x1b[B", OverlayAction::Next),
            (b"k", OverlayAction::Prev),
            (b"\x1b[A", OverlayAction::Prev),
            (b"g", OverlayAction::First),
            (b"\x1b[H", OverlayAction::First),
            (b"G", OverlayAction::Last),
            (b"\x1b[F", OverlayAction::Last),
            (b"\r", OverlayAction::Confirm),
            (b"\n", OverlayAction::Confirm),
            (b"\x1b", OverlayAction::Dismiss),
            (b"q", OverlayAction::Dismiss),
        ];
        for (keys, action) in cases {
            assert_eq!(overlay_action(keys), Some(action), "{keys:?}");
        }
    }

    #[test]
    fn an_unbound_key_is_not_an_overlay_action() {
        assert_eq!(overlay_action(b"x"), None);
        assert_eq!(overlay_action(b""), None);
    }
}
