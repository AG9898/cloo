//! Key chords, their configuration spellings, and the prefix keymap.
//!
//! Three things live here, and they are deliberately separate:
//!
//! 1. [`Key`] — one chord: a [`KeyCode`] and its [`KeyMods`]. It has a *spelling*
//!    ([`Key::parse`] and [`Display`](fmt::Display)) and nothing else. What bytes
//!    a terminal sends for a chord is the client's business, because that depends
//!    on the terminal; what a chord is called in `config.toml` is cloo's, and it
//!    must not drift between the parser and the documentation.
//! 2. The [`Action`] vocabulary — [`parse_action`] and [`action_name`], one
//!    kebab-case spelling per bindable action. An action that needs an argument a
//!    key cannot carry — text ([`Action::RenameTab`], [`Action::CopySearch`]) or a
//!    pane id ([`Action::FocusPane`], [`Action::ResizePane`], which the mouse
//!    supplies by pointing) — has **no spelling at all**, so a binding can never
//!    name a command the keypress could not supply an argument for.
//! 3. [`Keymap`] — the prefix chord plus the table reached after it. The
//!    defaults are tmux's, with `C-b` as the prefix, because the secondary user
//!    is a fluent tmux user (see `DECISIONS.md` RESOLVED-04).
//!
//! # Resolution and conflicts
//!
//! [`Keymap::bind`] replaces a key's previous action **in place** and returns
//! what it displaced. In place, because the order of the table is the order a
//! user reads it in, and returning the displaced action, because overriding a
//! default and colliding with an earlier user binding are the same operation to
//! this type and different messages to the person who wrote the file. Two keys
//! bound to the same action are not a conflict — that is how a user adds an alias
//! without losing the original.
//!
//! Nothing here consumes anything on its own. A [`Keymap`] answers "what is this
//! chord bound to *after the prefix*", and only after the prefix — `cloo-client`
//! owns the state machine that decides a chord is cloo's at all, which is what
//! keeps ordinary typing out of it.
//!
//! ```
//! use cloo_core::keymap::{Key, Keymap};
//! use cloo_proto::Action;
//!
//! let mut keys = Keymap::defaults();
//! assert_eq!(keys.prefix(), Key::parse("C-b").expect("a spelling"));
//! assert_eq!(keys.action(Key::parse("c").expect("a spelling")), Some(&Action::NewTab));
//!
//! // An override replaces one entry and leaves every other default alone.
//! let displaced = keys.bind(Key::parse("c").expect("a spelling"), Action::ClosePane);
//! assert_eq!(displaced, Some(Action::NewTab));
//! assert_eq!(keys.action(Key::parse("z").expect("a spelling")), Some(&Action::ToggleZoom));
//! ```

use core::fmt;

use cloo_proto::{Action, ClipboardTarget, CopyMotion, SearchDirection};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything a key spelling can be rejected for.
///
/// Like the rest of `cloo-core`'s errors, every variant is a *rejection*: the
/// keymap is left exactly as it was, so a configuration loader can drop the one
/// binding, warn about it, and keep the defaults it would have replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// An empty spelling. There is no key called nothing.
    Empty,
    /// Modifiers with no key after them, as in `"C-"`.
    MissingKey,
    /// A modifier prefix cloo does not know. Only `C-`, `M-`/`A-`, and `S-` are
    /// modifiers.
    UnknownModifier(char),
    /// A multi-character key name that is not in the table, as in `"pgdown"`.
    UnknownKey(String),
    /// A control character written literally where a name belongs. Escape is
    /// `"escape"`, not a raw `0x1b` byte in the document.
    Unprintable(char),
    /// `S-` on a printable character, as in `"S-a"`. A terminal reports a
    /// shifted `a` as `A`, so shift is part of the character itself and a
    /// binding that asked for both could never fire.
    ShiftedChar(char),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a key spelling cannot be empty"),
            Self::MissingKey => f.write_str("modifiers with no key after them"),
            Self::UnknownModifier(ch) => {
                write!(f, "unknown modifier {ch:?}; cloo knows C-, M-, A-, and S-")
            }
            Self::UnknownKey(name) => write!(f, "unknown key name {name:?}"),
            Self::Unprintable(ch) => write!(
                f,
                "a control character ({}) must be spelled by name",
                ch.escape_debug()
            ),
            Self::ShiftedChar(ch) => write!(
                f,
                "shift is part of the character itself; write the shifted form of {ch:?}"
            ),
        }
    }
}

impl std::error::Error for KeyError {}

// ---------------------------------------------------------------------------
// Chords
// ---------------------------------------------------------------------------

/// Which modifiers a chord holds.
///
/// Shift is here for named keys only — see [`KeyError::ShiftedChar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyMods {
    /// Control.
    pub ctrl: bool,
    /// Alt, which a terminal reports as an `ESC` prefix.
    pub alt: bool,
    /// Shift, reportable only for keys that are not a printable character.
    pub shift: bool,
}

impl KeyMods {
    /// No modifiers at all.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };

    /// Control alone.
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
    };

    /// Alt alone.
    pub const ALT: Self = Self {
        ctrl: false,
        alt: true,
        shift: false,
    };

    /// Shift alone.
    pub const SHIFT: Self = Self {
        ctrl: false,
        alt: false,
        shift: true,
    };
}

/// One key, before modifiers.
///
/// A printable character is [`Char`](Self::Char) and carries its own case;
/// everything a terminal reports as a sequence has a name, because a name is what
/// a configuration file can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A printable character, exactly as it would be typed.
    Char(char),
    /// Return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// A function key, `F1` through `F12`.
    Function(u8),
}

/// Every named key and its canonical spelling, in the order the documentation
/// lists them. The alternate spellings a user may write are in [`Key::parse`].
const NAMED: [(&str, KeyCode); 15] = [
    ("enter", KeyCode::Enter),
    ("tab", KeyCode::Tab),
    ("backspace", KeyCode::Backspace),
    ("escape", KeyCode::Escape),
    ("delete", KeyCode::Delete),
    ("insert", KeyCode::Insert),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("pageup", KeyCode::PageUp),
    ("pagedown", KeyCode::PageDown),
    ("space", KeyCode::Char(' ')),
];

impl KeyCode {
    /// This key's canonical configuration spelling.
    fn spelling(self) -> String {
        if let Self::Function(n) = self {
            return format!("f{n}");
        }
        for (name, code) in NAMED {
            if code == self {
                return name.to_owned();
            }
        }
        match self {
            Self::Char(ch) => ch.to_string(),
            // Unreachable while `NAMED` covers every non-`Char` variant, which
            // the round-trip test is what keeps true.
            _ => String::new(),
        }
    }
}

/// One chord: a key and the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// The key itself.
    pub code: KeyCode,
    /// What was held with it.
    pub mods: KeyMods,
}

impl Key {
    /// A chord from its parts.
    #[must_use]
    pub const fn new(code: KeyCode, mods: KeyMods) -> Self {
        Self { code, mods }
    }

    /// An unmodified character.
    #[must_use]
    pub const fn char(ch: char) -> Self {
        Self::new(KeyCode::Char(ch), KeyMods::NONE)
    }

    /// Control and a character.
    #[must_use]
    pub const fn ctrl(ch: char) -> Self {
        Self::new(KeyCode::Char(ch), KeyMods::CTRL)
    }

    /// An unmodified named key.
    #[must_use]
    pub const fn code(code: KeyCode) -> Self {
        Self::new(code, KeyMods::NONE)
    }

    /// The same chord with alt added, which is how a terminal's `ESC` prefix is
    /// folded into a decoded chord.
    #[must_use]
    pub const fn with_alt(mut self) -> Self {
        self.mods.alt = true;
        self
    }

    /// Parses one configuration spelling: modifiers, then a key.
    ///
    /// `C-` is control, `M-` and `A-` are alt, and `S-` is shift. The key is
    /// either a single printable character, written as itself and case
    /// sensitively, or one of the names in [`NAMED`] plus the accepted
    /// alternates `return`, `esc`, `bspace`, `del`, `ins`, `pgup`, `pgdn`, and
    /// `f1`–`f12`. Names are matched case-insensitively; a character is not.
    ///
    /// # Errors
    ///
    /// [`KeyError`] for an empty spelling, a modifier with no key, an unknown
    /// modifier or key name, a literal control character, or `S-` on a printable
    /// character — see [`KeyError::ShiftedChar`] for why that last one cannot be
    /// accepted quietly.
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        if text.is_empty() {
            return Err(KeyError::Empty);
        }

        let mut mods = KeyMods::NONE;
        let mut rest = text;
        // A trailing `-` is the key `-`, never a dangling modifier, so a
        // modifier is only stripped while something follows it.
        while rest.len() >= 2 && rest.as_bytes()[1] == b'-' {
            match rest.as_bytes()[0] {
                b'C' | b'c' => mods.ctrl = true,
                b'M' | b'm' | b'A' | b'a' => mods.alt = true,
                b'S' | b's' => mods.shift = true,
                other => return Err(KeyError::UnknownModifier(char::from(other))),
            }
            rest = &rest[2..];
        }

        if rest.is_empty() {
            return Err(KeyError::MissingKey);
        }

        let code = parse_code(rest)?;
        if mods.shift && matches!(code, KeyCode::Char(ch) if ch != ' ') {
            let KeyCode::Char(ch) = code else {
                unreachable!("matched a character above")
            };
            return Err(KeyError::ShiftedChar(ch));
        }
        Ok(Self::new(code, mods))
    }
}

/// Parses the key part of a spelling, with no modifiers left on it.
fn parse_code(text: &str) -> Result<KeyCode, KeyError> {
    let mut chars = text.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ch.is_control() {
            return Err(KeyError::Unprintable(ch));
        }
        return Ok(KeyCode::Char(ch));
    }

    let lower = text.to_ascii_lowercase();
    for (name, code) in NAMED {
        if lower == name {
            return Ok(code);
        }
    }
    let alternate = match lower.as_str() {
        "return" => Some(KeyCode::Enter),
        "esc" => Some(KeyCode::Escape),
        "bspace" => Some(KeyCode::Backspace),
        "del" => Some(KeyCode::Delete),
        "ins" => Some(KeyCode::Insert),
        "pgup" => Some(KeyCode::PageUp),
        "pgdn" | "pgdown" => Some(KeyCode::PageDown),
        _ => None,
    };
    if let Some(code) = alternate {
        return Ok(code);
    }
    if let Some(number) = lower.strip_prefix('f') {
        if let Ok(n) = number.parse::<u8>() {
            if (1..=12).contains(&n) {
                return Ok(KeyCode::Function(n));
            }
        }
    }
    Err(KeyError::UnknownKey(text.to_owned()))
}

impl fmt::Display for Key {
    /// The canonical spelling, which [`Key::parse`] reads back as this chord.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, prefix) in [
            (self.mods.ctrl, "C-"),
            (self.mods.alt, "M-"),
            (self.mods.shift, "S-"),
        ] {
            if held {
                f.write_str(prefix)?;
            }
        }
        f.write_str(&self.code.spelling())
    }
}

// ---------------------------------------------------------------------------
// The action vocabulary
// ---------------------------------------------------------------------------

/// Every bindable action's configuration spelling, in documentation order.
///
/// Deliberately not every [`Action`]: a spelling exists only where a keypress
/// carries everything the action needs. [`Action::RenameTab`] and
/// [`Action::CopySearch`] take text the user has to type somewhere, so they are
/// reached from a surface that can ask for it rather than from a chord that
/// would have to invent it. [`Action::FocusPane`] and [`Action::ResizePane`] are
/// out for the same reason with a different argument: they name a pane, which a
/// pointer supplies and a chord cannot.
pub const ACTION_NAMES: [&str; 31] = [
    "split-vertical",
    "split-horizontal",
    "close-pane",
    "focus-left",
    "focus-right",
    "focus-up",
    "focus-down",
    "toggle-zoom",
    "new-tab",
    "close-tab",
    "next-tab",
    "prev-tab",
    "enter-copy-mode",
    "exit-copy-mode",
    "copy-left",
    "copy-right",
    "copy-up",
    "copy-down",
    "copy-word-forward",
    "copy-word-backward",
    "copy-line-start",
    "copy-line-end",
    "copy-first-line",
    "copy-last-line",
    "begin-copy-selection",
    "clear-copy-selection",
    "next-copy-match",
    "prev-copy-match",
    "copy-to-clipboard",
    "copy-to-primary",
    "detach-client",
];

/// Parses one action spelling.
///
/// Returns `None` for anything not in [`ACTION_NAMES`], including the actions
/// that carry user text — a binding must not be able to name a command it cannot
/// supply an argument for.
#[must_use]
pub fn parse_action(name: &str) -> Option<Action> {
    Some(match name {
        "split-vertical" => Action::SplitVertical,
        "split-horizontal" => Action::SplitHorizontal,
        "close-pane" => Action::ClosePane,
        "focus-left" => Action::FocusLeft,
        "focus-right" => Action::FocusRight,
        "focus-up" => Action::FocusUp,
        "focus-down" => Action::FocusDown,
        "toggle-zoom" => Action::ToggleZoom,
        "new-tab" => Action::NewTab,
        "close-tab" => Action::CloseTab,
        "next-tab" => Action::NextTab,
        "prev-tab" => Action::PrevTab,
        "enter-copy-mode" => Action::EnterCopyMode,
        "exit-copy-mode" => Action::ExitCopyMode,
        "copy-left" => Action::CopyMotion(CopyMotion::Left),
        "copy-right" => Action::CopyMotion(CopyMotion::Right),
        "copy-up" => Action::CopyMotion(CopyMotion::Up),
        "copy-down" => Action::CopyMotion(CopyMotion::Down),
        "copy-word-forward" => Action::CopyMotion(CopyMotion::WordForward),
        "copy-word-backward" => Action::CopyMotion(CopyMotion::WordBackward),
        "copy-line-start" => Action::CopyMotion(CopyMotion::LineStart),
        "copy-line-end" => Action::CopyMotion(CopyMotion::LineEnd),
        "copy-first-line" => Action::CopyMotion(CopyMotion::FirstLine),
        "copy-last-line" => Action::CopyMotion(CopyMotion::LastLine),
        "begin-copy-selection" => Action::BeginCopySelection,
        "clear-copy-selection" => Action::ClearCopySelection,
        "next-copy-match" => Action::NextCopyMatch(SearchDirection::Forward),
        "prev-copy-match" => Action::NextCopyMatch(SearchDirection::Backward),
        "copy-to-clipboard" => Action::CopySelection(ClipboardTarget::Clipboard),
        "copy-to-primary" => Action::CopySelection(ClipboardTarget::PrimarySelection),
        "detach-client" => Action::DetachClient,
        _ => return None,
    })
}

/// This action's configuration spelling, or `None` for one that has none.
#[must_use]
pub fn action_name(action: &Action) -> Option<&'static str> {
    Some(match action {
        Action::SplitVertical => "split-vertical",
        Action::SplitHorizontal => "split-horizontal",
        Action::ClosePane => "close-pane",
        Action::FocusLeft => "focus-left",
        Action::FocusRight => "focus-right",
        Action::FocusUp => "focus-up",
        Action::FocusDown => "focus-down",
        Action::ToggleZoom => "toggle-zoom",
        Action::NewTab => "new-tab",
        Action::CloseTab => "close-tab",
        Action::NextTab => "next-tab",
        Action::PrevTab => "prev-tab",
        Action::EnterCopyMode => "enter-copy-mode",
        Action::ExitCopyMode => "exit-copy-mode",
        Action::CopyMotion(CopyMotion::Left) => "copy-left",
        Action::CopyMotion(CopyMotion::Right) => "copy-right",
        Action::CopyMotion(CopyMotion::Up) => "copy-up",
        Action::CopyMotion(CopyMotion::Down) => "copy-down",
        Action::CopyMotion(CopyMotion::WordForward) => "copy-word-forward",
        Action::CopyMotion(CopyMotion::WordBackward) => "copy-word-backward",
        Action::CopyMotion(CopyMotion::LineStart) => "copy-line-start",
        Action::CopyMotion(CopyMotion::LineEnd) => "copy-line-end",
        Action::CopyMotion(CopyMotion::FirstLine) => "copy-first-line",
        Action::CopyMotion(CopyMotion::LastLine) => "copy-last-line",
        Action::BeginCopySelection => "begin-copy-selection",
        Action::ClearCopySelection => "clear-copy-selection",
        Action::NextCopyMatch(SearchDirection::Forward) => "next-copy-match",
        Action::NextCopyMatch(SearchDirection::Backward) => "prev-copy-match",
        Action::CopySelection(ClipboardTarget::Clipboard) => "copy-to-clipboard",
        Action::CopySelection(ClipboardTarget::PrimarySelection) => "copy-to-primary",
        Action::DetachClient => "detach-client",
        // The first two need text a chord cannot carry; the last two name a
        // pane, which a keypress does not either. The keyboard reaches the same
        // session state through `focus-left` and friends.
        Action::RenameTab(_)
        | Action::CopySearch { .. }
        | Action::FocusPane(_)
        | Action::ResizePane { .. } => return None,
    })
}

// ---------------------------------------------------------------------------
// The keymap
// ---------------------------------------------------------------------------

/// The prefix chord and the table reached after it.
///
/// A keymap says nothing about ordinary typing. Only chords in this table mean
/// anything to cloo, and only after the prefix — which is the whole reason a
/// multiplexer can sit under a full-screen application without eating its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    prefix: Key,
    bindings: Vec<(Key, Action)>,
}

/// cloo's default prefix: tmux's, because the fluent tmux user is who this
/// removes a switching cost for.
pub const DEFAULT_PREFIX: Key = Key::ctrl('b');

impl Keymap {
    /// The tmux-shaped defaults: `C-b` and the table below it.
    #[must_use]
    pub fn defaults() -> Self {
        let bindings = vec![
            (Key::char('%'), Action::SplitVertical),
            (Key::char('"'), Action::SplitHorizontal),
            (Key::char('x'), Action::ClosePane),
            (Key::char('h'), Action::FocusLeft),
            (Key::char('j'), Action::FocusDown),
            (Key::char('k'), Action::FocusUp),
            (Key::char('l'), Action::FocusRight),
            (Key::code(KeyCode::Left), Action::FocusLeft),
            (Key::code(KeyCode::Down), Action::FocusDown),
            (Key::code(KeyCode::Up), Action::FocusUp),
            (Key::code(KeyCode::Right), Action::FocusRight),
            (Key::char('z'), Action::ToggleZoom),
            (Key::char('c'), Action::NewTab),
            (Key::char('&'), Action::CloseTab),
            (Key::char('n'), Action::NextTab),
            (Key::char('p'), Action::PrevTab),
            (Key::char('['), Action::EnterCopyMode),
            (Key::char('d'), Action::DetachClient),
        ];
        Self {
            prefix: DEFAULT_PREFIX,
            bindings,
        }
    }

    /// The chord that makes the *next* chord cloo's.
    #[must_use]
    pub const fn prefix(&self) -> Key {
        self.prefix
    }

    /// Replaces the prefix chord.
    ///
    /// The table is untouched, which is what "rebinding the prefix keeps the
    /// bindings you learned" means.
    pub fn set_prefix(&mut self, key: Key) {
        self.prefix = key;
    }

    /// Binds `key`, replacing any existing binding *in place*.
    ///
    /// Returns the action that was displaced, if there was one — overriding a
    /// default and colliding with an earlier user binding look identical here and
    /// read differently to the person who wrote the file, so the decision is the
    /// caller's.
    pub fn bind(&mut self, key: Key, action: Action) -> Option<Action> {
        match self.bindings.iter_mut().find(|(bound, _)| *bound == key) {
            Some(slot) => Some(core::mem::replace(&mut slot.1, action)),
            None => {
                self.bindings.push((key, action));
                None
            }
        }
    }

    /// Removes a binding, returning what it was bound to.
    pub fn unbind(&mut self, key: Key) -> Option<Action> {
        let at = self.bindings.iter().position(|(bound, _)| *bound == key)?;
        Some(self.bindings.remove(at).1)
    }

    /// What `key` is bound to after the prefix, if anything.
    #[must_use]
    pub fn action(&self, key: Key) -> Option<&Action> {
        self.bindings
            .iter()
            .find(|(bound, _)| *bound == key)
            .map(|(_, action)| action)
    }

    /// Every binding, in the order a reader of the configuration would list
    /// them: the defaults in their default order, then additions as written.
    #[must_use]
    pub fn bindings(&self) -> &[(Key, Action)] {
        &self.bindings
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_proto::{Direction, PaneId};

    fn key(text: &str) -> Key {
        Key::parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    // -- spellings ----------------------------------------------------------

    #[test]
    fn a_chord_is_modifiers_then_a_key() {
        let cases: [(&str, Key); 6] = [
            ("b", Key::char('b')),
            ("C-b", Key::ctrl('b')),
            ("M-x", Key::new(KeyCode::Char('x'), KeyMods::ALT)),
            ("A-x", Key::new(KeyCode::Char('x'), KeyMods::ALT)),
            ("S-tab", Key::new(KeyCode::Tab, KeyMods::SHIFT)),
            (
                "C-M-left",
                Key::new(
                    KeyCode::Left,
                    KeyMods {
                        ctrl: true,
                        alt: true,
                        shift: false,
                    },
                ),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(key(text), expected, "{text}");
        }
    }

    #[test]
    fn a_character_key_is_case_sensitive_and_a_name_is_not() {
        assert_ne!(key("g"), key("G"));
        assert_eq!(key("Escape"), key("esc"));
        assert_eq!(key("PageUp"), key("pgup"));
        assert_eq!(key("F5"), Key::code(KeyCode::Function(5)));
    }

    #[test]
    fn a_trailing_dash_is_the_key_and_not_a_dangling_modifier() {
        assert_eq!(key("-"), Key::char('-'));
        assert_eq!(key("C--"), Key::ctrl('-'));
    }

    /// Every canonical spelling reads back as the chord it names, which is what
    /// keeps the documentation and the parser from drifting apart.
    #[test]
    fn every_canonical_spelling_round_trips() {
        let mut codes: Vec<KeyCode> = NAMED.iter().map(|(_, code)| *code).collect();
        codes.extend((1..=12).map(KeyCode::Function));
        codes.extend("aZ%\"&-[".chars().map(KeyCode::Char));

        for code in codes {
            for mods in [KeyMods::NONE, KeyMods::CTRL, KeyMods::ALT] {
                let chord = Key::new(code, mods);
                let text = chord.to_string();
                assert!(!text.is_empty(), "{code:?} has no spelling");
                assert_eq!(Key::parse(&text), Ok(chord), "{text}");
            }
        }
    }

    #[test]
    fn an_invalid_spelling_is_refused_rather_than_guessed_at() {
        let cases: [(&str, KeyError); 6] = [
            ("", KeyError::Empty),
            ("C-", KeyError::MissingKey),
            ("X-q", KeyError::UnknownModifier('X')),
            ("pgdwn", KeyError::UnknownKey("pgdwn".to_owned())),
            ("f13", KeyError::UnknownKey("f13".to_owned())),
            ("S-a", KeyError::ShiftedChar('a')),
        ];
        for (text, expected) in cases {
            assert_eq!(Key::parse(text), Err(expected), "{text}");
        }
    }

    #[test]
    fn a_literal_control_character_must_be_spelled_by_name() {
        // A raw `0x1b` in a document is a key nobody can read, and the message
        // that reports it must not carry the byte either.
        let err = Key::parse("\u{1b}").expect_err("a control character");
        assert_eq!(err, KeyError::Unprintable('\u{1b}'));
        assert!(!err.to_string().contains('\u{1b}'), "{err}");
    }

    // -- the action vocabulary ----------------------------------------------

    #[test]
    fn every_action_name_parses_back_to_itself() {
        for name in ACTION_NAMES {
            let action = parse_action(name).unwrap_or_else(|| panic!("{name} is bindable"));
            assert_eq!(action_name(&action), Some(name), "{name}");
        }
    }

    #[test]
    fn an_action_that_needs_typed_text_has_no_spelling() {
        // A chord carries no text, so binding one to these would have to invent
        // an argument the user never gave.
        assert_eq!(action_name(&Action::RenameTab("build".into())), None);
        assert_eq!(
            action_name(&Action::CopySearch {
                query: "retry".into(),
                direction: SearchDirection::Forward,
            }),
            None
        );
        assert_eq!(parse_action("rename-tab"), None);
        assert_eq!(parse_action("copy-search"), None);
        assert_eq!(parse_action(""), None);
    }

    /// The mouse's own actions name a pane. A chord cannot, so they are absent
    /// from the vocabulary in both directions — and the keyboard equivalent a
    /// click has is `focus-left` and its three siblings, which are bound.
    #[test]
    fn an_action_that_names_a_pane_has_no_spelling_but_has_a_keyboard_equivalent() {
        assert_eq!(action_name(&Action::FocusPane(PaneId::new(3))), None);
        assert_eq!(
            action_name(&Action::ResizePane {
                pane: PaneId::new(3),
                dir: Direction::Horizontal,
                delta: 2,
            }),
            None
        );
        assert_eq!(parse_action("focus-pane"), None);
        assert_eq!(parse_action("resize-pane"), None);

        for name in ["focus-left", "focus-right", "focus-up", "focus-down"] {
            assert!(
                ACTION_NAMES.contains(&name),
                "{name} must stay bindable: it is what a click's keyboard equivalent is"
            );
        }
    }

    // -- defaults, overrides, and conflicts ---------------------------------

    #[test]
    fn the_default_prefix_is_c_b() {
        assert_eq!(Keymap::defaults().prefix(), key("C-b"));
        assert_eq!(Keymap::defaults().prefix().to_string(), "C-b");
    }

    #[test]
    fn the_defaults_are_the_tmux_shaped_table() {
        let keys = Keymap::defaults();
        let cases: [(&str, Action); 10] = [
            ("%", Action::SplitVertical),
            ("\"", Action::SplitHorizontal),
            ("x", Action::ClosePane),
            ("h", Action::FocusLeft),
            ("l", Action::FocusRight),
            ("left", Action::FocusLeft),
            ("z", Action::ToggleZoom),
            ("c", Action::NewTab),
            ("[", Action::EnterCopyMode),
            ("d", Action::DetachClient),
        ];
        for (text, action) in cases {
            assert_eq!(keys.action(key(text)), Some(&action), "{text}");
        }
        assert_eq!(keys.action(key("Q")), None, "an unbound chord is unbound");
    }

    #[test]
    fn an_override_replaces_one_entry_in_place_and_reports_what_it_displaced() {
        let mut keys = Keymap::defaults();
        let at = keys
            .bindings()
            .iter()
            .position(|(bound, _)| *bound == key("x"))
            .expect("x is bound by default");

        assert_eq!(
            keys.bind(key("x"), Action::ToggleZoom),
            Some(Action::ClosePane)
        );
        assert_eq!(keys.action(key("x")), Some(&Action::ToggleZoom));
        assert_eq!(
            keys.bindings()[at].0,
            key("x"),
            "an override keeps the position the user learned"
        );
        assert_eq!(keys.bindings().len(), Keymap::defaults().bindings().len());
    }

    #[test]
    fn a_new_binding_is_appended_and_displaces_nothing() {
        let mut keys = Keymap::defaults();
        assert_eq!(keys.bind(key("C-q"), Action::ClosePane), None);
        assert_eq!(keys.bindings().last().map(|(k, _)| *k), Some(key("C-q")));
    }

    #[test]
    fn two_keys_for_one_action_is_an_alias_rather_than_a_conflict() {
        // This is how the arrow keys and `hjkl` both move focus, and how a user
        // adds a third without losing either.
        let mut keys = Keymap::defaults();
        assert_eq!(keys.bind(key("C-h"), Action::FocusLeft), None);
        for text in ["h", "left", "C-h"] {
            assert_eq!(keys.action(key(text)), Some(&Action::FocusLeft), "{text}");
        }
    }

    #[test]
    fn rebinding_the_prefix_leaves_every_binding_alone() {
        let mut keys = Keymap::defaults();
        keys.set_prefix(key("C-a"));
        assert_eq!(keys.prefix(), key("C-a"));
        assert_eq!(keys.bindings(), Keymap::defaults().bindings());
    }

    #[test]
    fn unbinding_removes_exactly_one_entry() {
        let mut keys = Keymap::defaults();
        assert_eq!(keys.unbind(key("x")), Some(Action::ClosePane));
        assert_eq!(keys.action(key("x")), None);
        assert_eq!(keys.unbind(key("x")), None, "removing it twice is a no-op");
        assert_eq!(keys.action(key("z")), Some(&Action::ToggleZoom));
    }
}
