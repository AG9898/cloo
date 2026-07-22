//! Client-local policy and rendering for typed outer-terminal effects.
//!
//! A pane can request an effect, but it can never send raw terminal bytes
//! around the renderer. This module is the one place an allowlisted request
//! becomes an escape sequence, after both local policy and terminal
//! capabilities agree. A denied effect writes nothing at all.

use std::io::{self, Write};

use cloo_proto::{ClipboardTarget, OuterTerminalEffect, TermCaps};

/// The client-local choices that permit outer-terminal effects.
///
/// The default is deliberately deny-all: a child cannot change an outer
/// terminal merely because it is attached. A front end that has obtained the
/// user's consent can opt into the narrowly supported effects below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectPolicy {
    /// Permit setting or resetting the outer terminal title.
    pub title: bool,
    /// Permit OSC 52 clipboard stores when the terminal reported support.
    pub clipboard: bool,
}

impl EffectPolicy {
    /// A policy that permits only terminal-title changes.
    #[must_use]
    pub const fn allow_title() -> Self {
        Self {
            title: true,
            clipboard: false,
        }
    }

    /// A policy that permits every effect this client can currently render.
    ///
    /// The terminal's capabilities still gate clipboard stores. Hyperlinks,
    /// notifications, progress, and graphics have no safe standalone renderer
    /// yet, so they remain suppressed even under this policy.
    #[must_use]
    pub const fn allow_supported() -> Self {
        Self {
            title: true,
            clipboard: true,
        }
    }

    /// Whether this client could store a clipboard at all.
    ///
    /// Both gates in one place, so a caller can decide *before* asking for
    /// text — an explicit copy on a client that would refuse the store should
    /// not put a user's scrollback on the wire to be discarded. The rendering
    /// path below asks the same question.
    #[must_use]
    pub const fn permits_clipboard(self, caps: TermCaps) -> bool {
        self.clipboard && caps.clipboard_osc52
    }
}

/// Renders one permitted effect to `output`.
///
/// Returns `Ok(true)` when an effect was written and `Ok(false)` when local
/// policy, capabilities, or payload validation denied it. The false case does
/// not touch `output`, which keeps both terminal and rendered grid intact.
///
/// # Errors
///
/// Returns the output writer's error after a permitted effect begins writing.
pub fn apply_effect<W: Write>(
    output: &mut W,
    caps: TermCaps,
    policy: EffectPolicy,
    effect: &OuterTerminalEffect,
) -> io::Result<bool> {
    let Some(bytes) = effect_bytes(caps, policy, effect) else {
        return Ok(false);
    };
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(true)
}

/// Serializes one permitted effect without writing it.
///
/// This keeps the policy decision pure for front ends that own their own
/// output loop. It never exposes an arbitrary raw control string.
#[must_use]
pub fn effect_bytes(
    caps: TermCaps,
    policy: EffectPolicy,
    effect: &OuterTerminalEffect,
) -> Option<Vec<u8>> {
    match effect {
        OuterTerminalEffect::SetTitle(title) if policy.title && plain_text(title) => {
            Some(osc(format!("2;{title}")))
        }
        OuterTerminalEffect::ResetTitle if policy.title => Some(osc("2;")),
        OuterTerminalEffect::ClipboardStore { target, text } if policy.permits_clipboard(caps) => {
            let selection = match target {
                ClipboardTarget::Clipboard => 'c',
                ClipboardTarget::PrimarySelection => 'p',
            };
            Some(osc(format!("52;{selection};{}", base64(text.as_bytes()))))
        }
        // An OSC 8 hyperlink needs a concrete span of renderer-owned text, so
        // emitting only its opener would leak terminal state into later frames.
        // Notification and progress protocols are terminal-specific, while the
        // sole graphics variant explicitly says it is unavailable.
        OuterTerminalEffect::Hyperlink { .. }
        | OuterTerminalEffect::Notification { .. }
        | OuterTerminalEffect::Progress(_)
        | OuterTerminalEffect::Graphics(_) => None,
        _ => None,
    }
}

/// Whether text can safely occupy an OSC payload directly.
///
/// OSC delimiters and escape bytes would let a title end its own sequence and
/// inject a second one. Clipboard text is base64 encoded separately.
fn plain_text(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Wraps an already-safe OSC payload with String Terminator framing.
fn osc(payload: impl AsRef<str>) -> Vec<u8> {
    let payload = payload.as_ref();
    let mut bytes = Vec::with_capacity(2 + payload.len() + 2);
    bytes.extend_from_slice(b"\x1b]");
    bytes.extend_from_slice(payload.as_bytes());
    bytes.extend_from_slice(b"\x1b\\");
    bytes
}

/// Encodes bytes for OSC 52 without adding a dependency for one fixed alphabet.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        encoded.push(char::from(ALPHABET[((value >> 18) & 0x3f) as usize]));
        encoded.push(char::from(ALPHABET[((value >> 12) & 0x3f) as usize]));
        encoded.push(if chunk.len() > 1 {
            char::from(ALPHABET[((value >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            char::from(ALPHABET[(value & 0x3f) as usize])
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_proto::{GraphicsEffect, ProgressState};

    #[test]
    fn a_permitted_title_reaches_the_terminal_once() {
        let mut terminal = Vec::new();
        let applied = apply_effect(
            &mut terminal,
            TermCaps::default(),
            EffectPolicy::allow_title(),
            &OuterTerminalEffect::SetTitle("agent task".into()),
        )
        .expect("a vector accepts terminal bytes");

        assert!(applied);
        assert_eq!(terminal, b"\x1b]2;agent task\x1b\\");
        assert_eq!(
            terminal
                .windows(2)
                .filter(|bytes| *bytes == b"\x1b]")
                .count(),
            1
        );
    }

    #[test]
    fn clipboard_requires_both_capability_and_policy() {
        let effect = OuterTerminalEffect::ClipboardStore {
            target: ClipboardTarget::Clipboard,
            text: "hi".into(),
        };
        let capable = TermCaps {
            clipboard_osc52: true,
            ..TermCaps::default()
        };

        assert_eq!(
            effect_bytes(
                TermCaps::default(),
                EffectPolicy::allow_supported(),
                &effect
            ),
            None
        );
        assert_eq!(
            effect_bytes(capable, EffectPolicy::default(), &effect),
            None
        );
        assert_eq!(
            effect_bytes(capable, EffectPolicy::allow_supported(), &effect),
            Some(b"\x1b]52;c;aGk=\x1b\\".to_vec())
        );
    }

    #[test]
    fn denied_effects_leave_the_terminal_output_untouched() {
        let mut terminal = b"rendered grid".to_vec();
        let before = terminal.clone();
        let applied = apply_effect(
            &mut terminal,
            TermCaps {
                clipboard_osc52: true,
                ..TermCaps::default()
            },
            EffectPolicy::default(),
            &OuterTerminalEffect::ClipboardStore {
                target: ClipboardTarget::PrimarySelection,
                text: "do not copy".into(),
            },
        )
        .expect("a denied effect does not write");

        assert!(!applied);
        assert_eq!(terminal, before, "policy denial must be a no-op");
    }

    #[test]
    fn unsafe_or_unimplemented_effects_are_suppressed() {
        let policy = EffectPolicy::allow_supported();
        assert_eq!(
            effect_bytes(
                TermCaps::default(),
                policy,
                &OuterTerminalEffect::SetTitle("ok\x1b]52;c;injected".into())
            ),
            None
        );
        assert_eq!(
            effect_bytes(
                TermCaps {
                    hyperlinks: true,
                    graphics: true,
                    ..TermCaps::default()
                },
                policy,
                &OuterTerminalEffect::Hyperlink {
                    uri: "https://example.invalid".into(),
                }
            ),
            None
        );
        assert_eq!(
            effect_bytes(
                TermCaps::default(),
                policy,
                &OuterTerminalEffect::Progress(ProgressState::Indeterminate)
            ),
            None
        );
        assert_eq!(
            effect_bytes(
                TermCaps::default(),
                policy,
                &OuterTerminalEffect::Graphics(GraphicsEffect::Unavailable)
            ),
            None
        );
    }

    #[test]
    fn base64_covers_each_padding_shape() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }
}
