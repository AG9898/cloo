//! The input modes the *child application* has asked for.
//!
//! A pane's application negotiates with its terminal the same way cloo
//! negotiates with the outer one: it turns bracketed paste, focus reporting,
//! mouse tracking, or the extended keyboard protocol on and off with private
//! mode sets. Those requests land in the emulator, and this is the cloo-owned
//! form of them.
//!
//! They matter because **the encoding of an input event is a function of what
//! the application asked for**, not of what the user's terminal can do. A paste
//! is only bracketed if the application enabled bracketed paste; a mouse click
//! is only worth encoding at all if the application is tracking the mouse — and
//! that same fact is what tells a client whether a click belongs to the
//! application or to cloo's own chrome.
//!
//! Like [`Cell`](crate::Cell), this type is mirrored in `cloo-proto` rather than
//! shared with it: `cloo-term` has no intra-workspace dependencies, and
//! `cloo-core` owns the conversion. The two definitions must change together.

/// How much of the mouse an application is tracking.
///
/// Ordered from least to most: each level reports everything the level below it
/// does. That ordering is the whole of the filtering rule — an event is worth
/// encoding when the application's level is at least the level the event needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum MouseTracking {
    /// The application is not tracking the mouse. Every mouse event belongs to
    /// cloo's chrome.
    #[default]
    Off,
    /// Button presses and releases only (DECSET 1000).
    Click,
    /// Presses, releases, and motion while a button is held (DECSET 1002).
    Drag,
    /// All of the above, plus motion with no button held (DECSET 1003).
    Motion,
}

/// The input modes a pane's application currently has enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaneModes {
    /// How much of the mouse the application wants reported.
    pub mouse: MouseTracking,
    /// Whether mouse reports should use the SGR encoding (DECSET 1006) rather
    /// than the legacy X10 one.
    pub sgr_mouse: bool,
    /// Whether pasted text should be wrapped in paste brackets (DECSET 2004).
    pub bracketed_paste: bool,
    /// Whether focus gain and loss should be reported (DECSET 1004).
    pub focus_events: bool,
    /// Whether the application has pushed a Kitty keyboard protocol flag set.
    pub extended_keys: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_levels_are_ordered_from_least_to_most() {
        assert!(MouseTracking::Off < MouseTracking::Click);
        assert!(MouseTracking::Click < MouseTracking::Drag);
        assert!(MouseTracking::Drag < MouseTracking::Motion);
    }

    #[test]
    fn an_application_that_asked_for_nothing_has_nothing_enabled() {
        let modes = PaneModes::default();
        assert_eq!(modes.mouse, MouseTracking::Off);
        assert!(!modes.sgr_mouse);
        assert!(!modes.bracketed_paste);
        assert!(!modes.focus_events);
        assert!(!modes.extended_keys);
    }
}
