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
//! 3. [`mouse_owner`] — whether a mouse event belongs to the pane's application
//!    or to cloo's own chrome.
//!
//! **The client decodes; the server encodes.** A paste leaves here as text and
//! is bracketed on the far side, because whether the child wants brackets is a
//! mode the child set and only the server can see. The same split is why a mouse
//! event crosses the wire as a [`MouseEvent`] rather than as an escape sequence.
//!
//! Ownership deserves its own sentence, because it is the property M6-01 builds
//! on. A mouse event is the application's only when the application is actually
//! tracking the mouse, the pointer is over a pane rather than over chrome, and
//! the user did not hold the shift override. Everything else is cloo's, and
//! **cloo's events never reach the wire** — a chrome click that leaked into a
//! pane would appear in the user's shell as garbage.

use cloo_proto::{MouseButton, MouseKind, MouseMods, MouseTracking, PaneModes, TermCaps};

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
/// 1. A pointer that is not over a pane is over chrome, whatever the
///    application has enabled.
/// 2. Holding shift is the conventional "this one is for the multiplexer"
///    override, and it is the only way to reach chrome inside a pane run by a
///    full-screen application.
/// 3. An application not tracking the mouse cannot own a mouse event, so cloo
///    takes it — this is what makes click-to-focus work in an ordinary shell.
#[must_use]
pub fn mouse_owner(modes: PaneModes, report: &MouseReport, over_pane: bool) -> MouseOwner {
    if !over_pane || report.mods.shift || modes.mouse == MouseTracking::Off {
        return MouseOwner::Chrome;
    }
    MouseOwner::Application
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
}
