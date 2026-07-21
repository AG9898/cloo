//! What the outer terminal can do, and what cloo does instead where it cannot.
//!
//! Capabilities are the client's to know. The server learns only the
//! [`TermCaps`] reported once in [`ClientMessage::Attach`][attach], which is what
//! keeps a capability difference between two attached clients from becoming
//! session state.
//!
//! Two rules compose here, and they answer different questions:
//!
//! - **Refuse when there is nothing to negotiate from.** An unset or `dumb`
//!   `TERM` cannot establish a baseline, so [`attach_caps`] returns a
//!   [`CapsError`] and the attach never happens. See `docs/DECISIONS.md`
//!   RESOLVED-12.
//! - **Degrade when there is.** A `TERM` that resolves but lacks a capability
//!   takes the documented [`Fallback`] for it — never a claim of support, which
//!   would corrupt the user's screen.
//!
//! The local in-process pane has no negotiation and no second client to
//! disagree with, so it runs with whatever [`caps_from_env`] can establish and
//! claims nothing where `TERM` is unresolvable. Both entry points are pure
//! functions of two strings, so neither needs the process environment to be
//! testable; [`detect_attach_caps`] and [`detect_caps`] are the thin wrappers
//! that read it.
//!
//! [attach]: cloo_proto::ClientMessage::Attach

use std::fmt;

use cloo_proto::TermCaps;

/// One negotiable terminal capability.
///
/// The variants mirror the fields of [`TermCaps`] one for one; this is the form
/// that can be enumerated, named in a message, and paired with a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// True colour (24-bit SGR).
    Truecolor,
    /// Bracketed paste mode.
    BracketedPaste,
    /// SGR-encoded mouse reporting.
    SgrMouse,
    /// Focus in/out reporting.
    FocusEvents,
    /// The Kitty extended keyboard protocol.
    ExtendedKeys,
    /// OSC 52 clipboard writes.
    ClipboardOsc52,
    /// OSC 8 hyperlinks.
    Hyperlinks,
    /// Inline graphics.
    Graphics,
}

/// Every capability, in the order [`TermCaps`] declares them.
pub const ALL: [Capability; 8] = [
    Capability::Truecolor,
    Capability::BracketedPaste,
    Capability::SgrMouse,
    Capability::FocusEvents,
    Capability::ExtendedKeys,
    Capability::ClipboardOsc52,
    Capability::Hyperlinks,
    Capability::Graphics,
];

/// The capabilities an interactive agent harness expects of a pane.
///
/// This is the "required" tier of the compatibility contract in
/// `docs/AGENT_WORKFLOWS.md`. Missing one is not a refusal — it is a
/// [`Fallback`], and every fallback here keeps the pane usable.
pub const BASELINE: [Capability; 5] = [
    Capability::Truecolor,
    Capability::BracketedPaste,
    Capability::SgrMouse,
    Capability::FocusEvents,
    Capability::ExtendedKeys,
];

impl Capability {
    /// Whether `caps` claims this capability.
    #[must_use]
    pub const fn present_in(self, caps: TermCaps) -> bool {
        match self {
            Self::Truecolor => caps.truecolor,
            Self::BracketedPaste => caps.bracketed_paste,
            Self::SgrMouse => caps.sgr_mouse,
            Self::FocusEvents => caps.focus_events,
            Self::ExtendedKeys => caps.extended_keys,
            Self::ClipboardOsc52 => caps.clipboard_osc52,
            Self::Hyperlinks => caps.hyperlinks,
            Self::Graphics => caps.graphics,
        }
    }

    /// What cloo does instead when this capability is absent.
    ///
    /// Every capability has exactly one documented answer, and it is the same
    /// answer on every client — a fallback chosen per attach would make two
    /// clients of the same session behave differently for no reason.
    #[must_use]
    pub const fn fallback(self) -> Fallback {
        match self {
            Self::Truecolor => Fallback::Downsample256,
            Self::BracketedPaste => Fallback::PasteAsTypedInput,
            Self::SgrMouse => Fallback::ChromeKeyboardOnly,
            Self::FocusEvents => Fallback::AssumeFocused,
            Self::ExtendedKeys => Fallback::LegacyKeyEncoding,
            Self::ClipboardOsc52 | Self::Hyperlinks | Self::Graphics => Fallback::SuppressEffect,
        }
    }

    /// The name this capability is called by in a message to a user.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Truecolor => "truecolor",
            Self::BracketedPaste => "bracketed paste",
            Self::SgrMouse => "SGR mouse reporting",
            Self::FocusEvents => "focus events",
            Self::ExtendedKeys => "extended keys",
            Self::ClipboardOsc52 => "OSC 52 clipboard",
            Self::Hyperlinks => "OSC 8 hyperlinks",
            Self::Graphics => "inline graphics",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What cloo does in place of a capability the terminal does not have.
///
/// Each variant is a behaviour, not an apology: nothing here degrades to
/// "unusable", and none of it is negotiable per client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// A `Color::Rgb` is downsampled to the nearest 256-palette entry rather
    /// than emitted as a sequence the terminal may not understand.
    Downsample256,
    /// Pasted text arrives as ordinary typed input, without the paste
    /// delimiters, so a harness sees it as keystrokes rather than not at all.
    PasteAsTypedInput,
    /// Chrome is driven from the keyboard only; no mouse routing is enabled and
    /// no mouse mode is sent to the pane.
    ChromeKeyboardOnly,
    /// The client is treated as always focused, so a harness never waits for a
    /// focus report that cannot arrive.
    AssumeFocused,
    /// Keys are encoded in the legacy scheme, losing the disambiguation
    /// extended keys would have carried.
    LegacyKeyEncoding,
    /// The typed outer-terminal effect is suppressed. Suppression is always
    /// safe — an effect may never alter session state.
    SuppressEffect,
}

impl Fallback {
    /// A one-line description, suitable for a message to a user.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Downsample256 => "colours are downsampled to the 256-colour palette",
            Self::PasteAsTypedInput => "pasted text is forwarded as typed input",
            Self::ChromeKeyboardOnly => "chrome is driven from the keyboard only",
            Self::AssumeFocused => "the client is treated as always focused",
            Self::LegacyKeyEncoding => "keys use the legacy encoding",
            Self::SuppressEffect => "the effect is suppressed",
        }
    }
}

impl fmt::Display for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

/// A capability the terminal lacks, and what happens instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Degradation {
    /// The capability that is absent.
    pub capability: Capability,
    /// The documented behaviour taken in its place.
    pub fallback: Fallback,
}

impl fmt::Display for Degradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no {}: {}", self.capability, self.fallback)
    }
}

/// Every baseline capability `caps` lacks, paired with its fallback.
///
/// This is the whole of what "unsupported combinations choose documented
/// fallbacks" means at attach time: the set is derived from the reported
/// capabilities, never from `TERM` again, so a client and anything inspecting
/// its attach agree on the answer.
#[must_use]
pub fn degradations(caps: TermCaps) -> Vec<Degradation> {
    BASELINE
        .into_iter()
        .filter(|cap| !cap.present_in(caps))
        .map(|capability| Degradation {
            capability,
            fallback: capability.fallback(),
        })
        .collect()
}

/// Why capabilities could not be negotiated for an attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsError {
    /// `TERM` is unset or empty.
    TermUnset,
    /// `TERM` is `dumb`, which announces the absence of a baseline rather than
    /// a terminal type to negotiate one from.
    TermDumb,
}

impl fmt::Display for CapsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cause = match self {
            Self::TermUnset => "TERM is not set",
            Self::TermDumb => "TERM is \"dumb\"",
        };
        write!(
            f,
            "{cause}, so cloo cannot negotiate terminal capabilities for an attach; \
             set TERM (for example TERM=xterm-256color) and attach again. \
             Running cloo without attaching still gives you a local pane."
        )
    }
}

impl std::error::Error for CapsError {}

/// Decides what the outer terminal can draw, for a client that is attaching.
///
/// Only capabilities that can be established without writing a query sequence
/// and waiting for a reply are decided here. Everything else stays false: a
/// client must never claim a capability it has not established, because the
/// documented fallback is always safe and a wrongly claimed capability corrupts
/// the user's screen.
///
/// # Errors
///
/// Returns [`CapsError`] when `TERM` is unset, empty, or `dumb`. There is no
/// baseline to negotiate from in that case, and a silently degraded remote
/// session is the harder failure to diagnose.
pub fn attach_caps(term: Option<&str>, colorterm: Option<&str>) -> Result<TermCaps, CapsError> {
    let term = term.unwrap_or("");
    if term.is_empty() {
        return Err(CapsError::TermUnset);
    }
    if term == "dumb" {
        return Err(CapsError::TermDumb);
    }
    let colorterm = colorterm.unwrap_or("");

    let truecolor = colorterm.eq_ignore_ascii_case("truecolor")
        || colorterm.eq_ignore_ascii_case("24bit")
        || term.contains("truecolor")
        || term.contains("direct");

    Ok(TermCaps {
        truecolor,
        // Universal enough among terminals that report a `TERM` at all, and
        // harmless where unsupported: an unrecognized private mode is ignored.
        bracketed_paste: true,
        sgr_mouse: true,
        focus_events: true,
        ..TermCaps::default()
    })
}

/// Reads the process environment and negotiates capabilities for an attach.
///
/// # Errors
///
/// Returns the [`CapsError`] from [`attach_caps`].
pub fn detect_attach_caps() -> Result<TermCaps, CapsError> {
    attach_caps(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

/// Decides what the outer terminal can draw, for the local in-process pane.
///
/// The same detection as [`attach_caps`], except that an unresolvable `TERM`
/// claims nothing rather than refusing. Nothing is negotiated on this path and
/// there is no second client whose capabilities could disagree, so claiming
/// nothing is conservative rather than a guess.
#[must_use]
pub fn caps_from_env(term: Option<&str>, colorterm: Option<&str>) -> TermCaps {
    attach_caps(term, colorterm).unwrap_or_default()
}

/// Reads the process environment for the local in-process pane.
#[must_use]
pub fn detect_caps() -> TermCaps {
    caps_from_env(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unresolvable_term_refuses_an_attach() {
        assert_eq!(
            attach_caps(None, Some("truecolor")),
            Err(CapsError::TermUnset)
        );
        assert_eq!(attach_caps(Some(""), None), Err(CapsError::TermUnset));
        assert_eq!(
            attach_caps(Some("dumb"), Some("truecolor")),
            Err(CapsError::TermDumb)
        );
    }

    #[test]
    fn the_refusal_says_what_to_do_about_it() {
        for err in [CapsError::TermUnset, CapsError::TermDumb] {
            let message = err.to_string();
            assert!(message.contains("set TERM"), "got: {message}");
            assert!(message.contains("xterm-256color"), "got: {message}");
            assert!(
                message.contains("local pane"),
                "the two behaviours differ and the message is one of the two places \
                 that must say so, got: {message}"
            );
        }
    }

    #[test]
    fn a_dumb_or_absent_terminal_still_runs_a_local_pane() {
        assert_eq!(caps_from_env(None, Some("truecolor")), TermCaps::default());
        assert_eq!(caps_from_env(Some(""), None), TermCaps::default());
        assert_eq!(
            caps_from_env(Some("dumb"), Some("truecolor")),
            TermCaps::default()
        );
    }

    #[test]
    fn colorterm_is_what_establishes_truecolor() {
        let caps = |term, colorterm| caps_from_env(Some(term), colorterm).truecolor;
        assert!(caps("xterm-256color", Some("truecolor")));
        assert!(caps("xterm-256color", Some("24bit")));
        assert!(caps("xterm-256color", Some("TrueColor")));
        assert!(!caps("xterm-256color", None));
        assert!(!caps("xterm-256color", Some("")));
    }

    #[test]
    fn a_direct_color_term_entry_also_establishes_truecolor() {
        assert!(caps_from_env(Some("xterm-direct"), None).truecolor);
    }

    #[test]
    fn unestablished_capabilities_stay_false() {
        let caps = caps_from_env(Some("xterm-256color"), Some("truecolor"));
        assert!(!caps.extended_keys, "needs a query and a reply");
        assert!(!caps.clipboard_osc52);
        assert!(!caps.hyperlinks);
        assert!(!caps.graphics);
    }

    #[test]
    fn every_capability_reads_its_own_field() {
        for cap in ALL {
            let mut caps = TermCaps::default();
            match cap {
                Capability::Truecolor => caps.truecolor = true,
                Capability::BracketedPaste => caps.bracketed_paste = true,
                Capability::SgrMouse => caps.sgr_mouse = true,
                Capability::FocusEvents => caps.focus_events = true,
                Capability::ExtendedKeys => caps.extended_keys = true,
                Capability::ClipboardOsc52 => caps.clipboard_osc52 = true,
                Capability::Hyperlinks => caps.hyperlinks = true,
                Capability::Graphics => caps.graphics = true,
            }
            // Exactly the one field, or `present_in` is reading a neighbour and
            // every fallback decision built on it is wrong.
            let set: Vec<Capability> = ALL.into_iter().filter(|c| c.present_in(caps)).collect();
            assert_eq!(set, vec![cap], "{cap} did not read its own field");
        }
    }

    #[test]
    fn a_terminal_with_nothing_falls_back_across_the_whole_baseline() {
        let taken = degradations(TermCaps::default());
        assert_eq!(taken.len(), BASELINE.len());
        for degradation in &taken {
            assert_eq!(
                degradation.fallback,
                degradation.capability.fallback(),
                "a fallback must be the capability's documented one"
            );
        }
        assert!(
            taken
                .iter()
                .any(|d| d.capability == Capability::Truecolor
                    && d.fallback == Fallback::Downsample256),
            "the renderer's downsample is the truecolor fallback"
        );
    }

    #[test]
    fn a_present_capability_takes_no_fallback() {
        let caps = attach_caps(Some("xterm-256color"), Some("truecolor"))
            .expect("a resolvable TERM negotiates");
        let taken = degradations(caps);
        assert_eq!(
            taken,
            vec![Degradation {
                capability: Capability::ExtendedKeys,
                fallback: Fallback::LegacyKeyEncoding,
            }],
            "only what env detection cannot establish should degrade, got {taken:?}"
        );
    }

    #[test]
    fn an_optional_capability_is_never_a_baseline_requirement() {
        assert!(
            !BASELINE.contains(&Capability::Graphics),
            "inline graphics are an enhancement, never a compatibility requirement"
        );
        assert_eq!(Capability::Graphics.fallback(), Fallback::SuppressEffect);
    }
}
