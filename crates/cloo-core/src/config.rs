//! Configuration parsing: local profile and key definitions merged over the
//! built-ins.
//!
//! [`parse`] takes the *text* of `config.toml` and never a path. `cloo-core`
//! performs no I/O, so finding the file, reading it, and deciding what to do
//! when it is absent all belong to the server — this module's whole job is
//! turning a string into a validated [`Config`] plus the list of things it had
//! to refuse.
//!
//! Two kinds of wrongness, answered differently, because they are not the same
//! mistake:
//!
//! - **Syntax is the document's.** Malformed TOML, a misspelled key, a string
//!   where a number belongs: [`parse`] returns [`ConfigError`] and nothing is
//!   loaded. Unknown keys are rejected rather than ignored, so a typo surfaces
//!   as an error instead of as a setting that silently never applied.
//! - **Semantics are each entry's.** A well-formed profile whose ID is not in
//!   the accepted alphabet, whose command holds a control character, or whose
//!   recommended minimum is below cloo's layout floor is dropped *on its own*,
//!   with a [`ConfigWarning`] naming it. One bad profile must not cost the user
//!   the other nine, and it must not silently become something it did not say.
//!   A key binding is the same: an unspellable chord or an action cloo does not
//!   know drops that one line, and the default it would have replaced survives —
//!   an unusable prefix in particular leaves `C-b` in place, because a keymap
//!   nobody can reach is a terminal the user has to kill.
//!
//! Either way the fallback is safe: a caller that gets a [`ConfigError`] keeps
//! [`Config::defaults`], and a caller that gets warnings keeps everything that
//! validated. Nothing here panics and nothing is partially applied.
//!
//! ```
//! use cloo_core::config::parse;
//!
//! let loaded = parse(
//!     r#"
//!     [[profile]]
//!     id = "notes"
//!     command = ["hx", "notes.md"]
//!     "#,
//! )
//! .expect("valid document");
//! assert!(loaded.warnings.is_empty());
//! // The built-ins are still there; `notes` was appended after them.
//! assert_eq!(loaded.config.profiles().len(), 4);
//! assert!(loaded.config.profile("codex").is_some());
//! ```
//!
//! # Document shape
//!
//! ```toml
//! # A profile is an array-of-tables entry. `id` is the only required key.
//! [[profile]]
//! id = "notes"
//! command = ["hx", "notes.md"]   # omit entirely for the user's login shell
//! default_name = "notes"         # defaults to the id
//! min_size = { cols = 60, rows = 15 }
//! adapter = "my-adapter"
//!
//! # Reusing a built-in's id replaces that built-in, in place, keeping its
//! # position in the launcher.
//! [[profile]]
//! id = "codex"
//! command = ["codex", "--model", "o3"]
//!
//! [keys]
//! prefix = "C-a"                 # omit to keep cloo's `C-b`
//!
//! [keys.bindings]
//! "|" = "split-vertical"         # an addition, or an override of a default
//! "x" = "none"                   # `none` removes a binding entirely
//! ```
//!
//! Chord spellings are [`crate::keymap::Key::parse`]'s and action names are
//! [`crate::keymap::ACTION_NAMES`]. Bindings are reached *after* the prefix, so
//! nothing in that table can shadow a key an application is using.

use core::fmt;
use std::collections::BTreeMap;

use serde::Deserialize;

use cloo_proto::Size;

use crate::error::MetadataError;
use crate::keymap::{Key, KeyError, Keymap, parse_action};
use crate::profile::{AdapterId, Profile, ProfileCommand, ProfileId};

/// The value that removes a binding rather than naming an action.
const UNBIND: &str = "none";

// ---------------------------------------------------------------------------
// Errors and warnings
// ---------------------------------------------------------------------------

/// A document that could not be read at all.
///
/// Deliberately one variant: everything the parser can object to — a broken
/// table header, an unknown key, a value of the wrong type — is the same answer
/// to the caller, which is "keep the previous configuration and tell the user".
/// The message is the parser's own, which points at the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl ConfigError {
    /// The parser's message, including its line and column.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// A single profile that was dropped, and why.
///
/// A warning is never fatal. It exists so the user is told what did not load —
/// "invalid config falls back to defaults" is only safe if the fallback is
/// visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// The profile did not validate. Its ID is kept as written so the message
    /// can name the entry even when the ID itself is what was rejected.
    Rejected {
        /// The `id` field as it appeared in the document.
        id: String,
        /// The first field that was unusable.
        reason: MetadataError,
    },
    /// A second definition of an ID already defined earlier in the same
    /// document. The first definition wins and this one is dropped, so the
    /// result never depends on which duplicate the parser happened to see last.
    Duplicate {
        /// The repeated ID.
        id: String,
    },
    /// A prefix chord that could not be spelled. The default `C-b` is kept: a
    /// prefix nobody can press is a session with no way out.
    BadPrefix {
        /// The chord as it appeared in the document.
        key: String,
        /// Why it could not be read.
        reason: KeyError,
    },
    /// A binding whose chord could not be spelled. Whatever that chord was bound
    /// to before — usually a default — is left in place.
    BadBinding {
        /// The chord as it appeared in the document.
        key: String,
        /// Why it could not be read.
        reason: KeyError,
    },
    /// A binding naming an action cloo has no spelling for. `RenameTab` and
    /// `CopySearch` land here on purpose: they need text a chord cannot carry.
    UnknownAction {
        /// The chord the line bound.
        key: String,
        /// The action name as written.
        action: String,
    },
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { id, reason } => {
                write!(f, "profile {id:?} was ignored: {reason}")
            }
            Self::Duplicate { id } => write!(
                f,
                "profile {id:?} is defined more than once; the first definition was kept"
            ),
            Self::BadPrefix { key, reason } => write!(
                f,
                "key prefix {key:?} was ignored: {reason}; the default {} was kept",
                crate::keymap::DEFAULT_PREFIX
            ),
            Self::BadBinding { key, reason } => {
                write!(f, "binding {key:?} was ignored: {reason}")
            }
            Self::UnknownAction { key, action } => write!(
                f,
                "binding {key:?} was ignored: no action is called {action:?}"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// A validated configuration.
///
/// Today it holds profiles and the keymap. Theme selection and the rest of the
/// surface land alongside them; the fields are private and reached through
/// accessors so adding one is not a breaking change for every caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    profiles: Vec<Profile>,
    keys: Keymap,
}

impl Config {
    /// The configuration cloo uses with no config file: the three built-in
    /// profiles and the `C-b` keymap.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            profiles: Profile::built_ins(),
            keys: Keymap::defaults(),
        }
    }

    /// The prefix chord and the table reached after it.
    #[must_use]
    pub const fn keys(&self) -> &Keymap {
        &self.keys
    }

    /// Every profile, in launcher order — built-ins first, in their built-in
    /// order, then local additions in the order the document defined them.
    #[must_use]
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// Looks a profile up by ID.
    #[must_use]
    pub fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id.as_str() == id)
    }

    /// Adds a profile, replacing any existing one with the same ID *in place*.
    ///
    /// Replacing in place is what lets a user override `codex` without the
    /// override jumping to the end of the launcher — the position a profile
    /// occupies is part of what the user learned.
    fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

/// The result of a successful parse: what loaded, and what was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// Everything that validated, merged over the built-ins.
    pub config: Config,
    /// One entry per dropped profile, in document order. Empty on a clean load.
    pub warnings: Vec<ConfigWarning>,
}

// ---------------------------------------------------------------------------
// Raw document
// ---------------------------------------------------------------------------

/// The document as written. Separate from [`Config`] on purpose: this type is
/// permissive about *values* (any string may appear as an ID) and strict about
/// *keys*, and the conversion below is where the values get their answer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    profile: Vec<RawProfile>,
    #[serde(default)]
    keys: Option<RawKeys>,
}

/// The `[keys]` table. TOML itself refuses a repeated key inside `bindings`, so
/// a chord written twice is a document error rather than a silent last-wins.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeys {
    #[serde(default)]
    prefix: Option<String>,
    /// Ordered by chord spelling rather than by document order, so the warnings
    /// a broken table produces do not depend on TOML's map iteration.
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfile {
    id: String,
    /// Omitted entirely means the user's login shell. An explicit empty array
    /// is a mistake rather than a shorthand, and is rejected as such.
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    default_name: Option<String>,
    #[serde(default)]
    min_size: Option<RawSize>,
    #[serde(default)]
    adapter: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSize {
    cols: u16,
    rows: u16,
}

impl RawProfile {
    /// Validates one entry into a [`Profile`].
    ///
    /// Every check is [`crate::profile`]'s, reached through the same public
    /// constructors a built-in uses — a configured profile must not be able to
    /// express anything a built-in could not, and vice versa.
    fn into_profile(self) -> Result<Profile, MetadataError> {
        let id = ProfileId::new(self.id)?;
        let command = match self.command {
            None => ProfileCommand::LoginShell,
            Some(argv) => {
                let mut parts = argv.into_iter();
                let program = parts.next().unwrap_or_default();
                ProfileCommand::Program {
                    program,
                    args: parts.collect(),
                }
            }
        };
        let default_name = self.default_name.unwrap_or_else(|| id.as_str().to_owned());
        let mut profile = Profile::new(id, command, default_name);
        if let Some(size) = self.min_size {
            profile = profile.min_size(Size::new(size.cols, size.rows));
        }
        if let Some(adapter) = self.adapter {
            profile = profile.adapter(AdapterId::new(adapter)?);
        }
        profile.validate()?;
        Ok(profile)
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses configuration text and merges it over the built-in profiles.
///
/// An empty document is valid and yields exactly [`Config::defaults`].
///
/// # Errors
///
/// [`ConfigError`] when the document is not well-formed TOML or names a key
/// cloo does not know. A profile that parses but does not *validate* is not an
/// error — it is dropped with a [`ConfigWarning`] and the rest still loads.
pub fn parse(text: &str) -> Result<Loaded, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;

    let mut config = Config::defaults();
    let mut warnings = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for entry in raw.profile {
        let written = entry.id.clone();
        if seen.contains(&written) {
            warnings.push(ConfigWarning::Duplicate { id: written });
            continue;
        }
        match entry.into_profile() {
            Ok(profile) => {
                seen.push(written);
                config.upsert(profile);
            }
            Err(reason) => warnings.push(ConfigWarning::Rejected {
                id: written,
                reason,
            }),
        }
    }

    if let Some(keys) = raw.keys {
        apply_keys(keys, &mut config.keys, &mut warnings);
    }

    Ok(Loaded { config, warnings })
}

/// Merges a `[keys]` table over the defaults.
///
/// Every line is answered on its own: a chord that cannot be spelled, or an
/// action cloo has no name for, drops that line and leaves whatever it would
/// have replaced in place. That is what "invalid configuration falls back to the
/// defaults" has to mean for a keymap — the alternative is a document with one
/// typo in it leaving the user unable to split a pane.
fn apply_keys(raw: RawKeys, keys: &mut Keymap, warnings: &mut Vec<ConfigWarning>) {
    if let Some(written) = raw.prefix {
        match Key::parse(&written) {
            Ok(key) => keys.set_prefix(key),
            Err(reason) => warnings.push(ConfigWarning::BadPrefix {
                key: written,
                reason,
            }),
        }
    }

    for (written, action) in raw.bindings {
        let key = match Key::parse(&written) {
            Ok(key) => key,
            Err(reason) => {
                warnings.push(ConfigWarning::BadBinding {
                    key: written,
                    reason,
                });
                continue;
            }
        };
        if action == UNBIND {
            keys.unbind(key);
            continue;
        }
        match parse_action(&action) {
            Some(action) => {
                keys.bind(key, action);
            }
            None => warnings.push(ConfigWarning::UnknownAction {
                key: written,
                action,
            }),
        }
    }
}

/// Parses configuration text, falling back to [`Config::defaults`] when the
/// document itself is unreadable.
///
/// The convenience the callers actually want: both failure modes come back as
/// human-readable warnings over a usable configuration, so no caller has to
/// remember that a syntax error must not be fatal.
#[must_use]
pub fn parse_or_defaults(text: &str) -> (Config, Vec<String>) {
    match parse(text) {
        Ok(loaded) => (
            loaded.config,
            loaded.warnings.iter().map(ToString::to_string).collect(),
        ),
        Err(err) => (Config::defaults(), vec![err.to_string()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cloo_proto::Action;

    use crate::layout::MIN_PANE_SIZE;
    use crate::profile::HARNESS_MIN_SIZE;

    fn ids(config: &Config) -> Vec<String> {
        config.profiles().iter().map(|p| p.id.to_string()).collect()
    }

    // -- Defaults and merging -----------------------------------------------

    #[test]
    fn an_empty_document_is_exactly_the_built_ins() {
        let loaded = parse("").expect("empty is valid");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config, Config::defaults());
        assert_eq!(ids(&loaded.config), ["generic", "codex", "claude"]);
    }

    #[test]
    fn a_local_profile_is_appended_after_the_built_ins() {
        let loaded = parse(
            r#"
            [[profile]]
            id = "notes"
            command = ["hx", "notes.md"]
            "#,
        )
        .expect("valid");
        assert_eq!(ids(&loaded.config), ["generic", "codex", "claude", "notes"]);
        let notes = loaded.config.profile("notes").expect("present");
        assert_eq!(
            notes.command,
            ProfileCommand::Program {
                program: "hx".to_owned(),
                args: vec!["notes.md".to_owned()],
            }
        );
        // Both omitted defaults: the name is the id, the recommendation is the
        // layout floor.
        assert_eq!(notes.default_name, "notes");
        assert_eq!(notes.min_size, MIN_PANE_SIZE);
        assert_eq!(notes.adapter, None);
    }

    #[test]
    fn overriding_a_built_in_replaces_it_in_place() {
        // The override must not move `codex` to the end of the launcher — its
        // position is part of what the user learned.
        let loaded = parse(
            r#"
            [[profile]]
            id = "codex"
            command = ["codex", "--model", "o3"]
            "#,
        )
        .expect("valid");
        assert_eq!(ids(&loaded.config), ["generic", "codex", "claude"]);
        let codex = loaded.config.profile("codex").expect("present");
        assert_eq!(
            codex.command,
            ProfileCommand::Program {
                program: "codex".to_owned(),
                args: vec!["--model".to_owned(), "o3".to_owned()],
            }
        );
        // Only what was written changed; the rest is still the built-in's.
        assert_eq!(codex.default_name, "codex");
    }

    #[test]
    fn a_duplicate_id_keeps_the_first_definition() {
        let loaded = parse(
            r#"
            [[profile]]
            id = "notes"
            command = ["first"]

            [[profile]]
            id = "notes"
            command = ["second"]
            "#,
        )
        .expect("valid");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::Duplicate {
                id: "notes".to_owned()
            }]
        );
        let notes = loaded.config.profile("notes").expect("present");
        assert_eq!(notes.command, ProfileCommand::program("first"));
    }

    // -- Command templates --------------------------------------------------

    #[test]
    fn an_omitted_command_means_the_login_shell() {
        // Resolving `$SHELL` is the server's; the document only asks for it.
        let loaded = parse(
            r#"
            [[profile]]
            id = "plain"
            "#,
        )
        .expect("valid");
        let plain = loaded.config.profile("plain").expect("present");
        assert_eq!(plain.command, ProfileCommand::LoginShell);
    }

    #[test]
    fn an_empty_command_array_is_rejected_rather_than_read_as_a_shell() {
        // The difference between "run my login shell" and "run nothing" is not
        // something to guess at.
        let loaded = parse(
            r#"
            [[profile]]
            id = "broken"
            command = []
            "#,
        )
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::Rejected {
                id: "broken".to_owned(),
                reason: MetadataError::Empty("profile command"),
            }]
        );
        assert_eq!(loaded.config, Config::defaults());
    }

    #[test]
    fn a_command_keeps_its_arguments_verbatim() {
        // An argv, never a shell string: a space inside one entry stays one
        // argument and is never word-split on the way to `execvp`.
        let loaded = parse(
            r#"
            [[profile]]
            id = "brief"
            command = ["claude", "--append-system-prompt", "be brief"]
            "#,
        )
        .expect("valid");
        let ProfileCommand::Program { program, args } =
            &loaded.config.profile("brief").expect("present").command
        else {
            unreachable!("a program template")
        };
        assert_eq!(program, "claude");
        assert_eq!(args, &["--append-system-prompt", "be brief"]);
    }

    #[test]
    fn a_control_character_in_a_command_is_rejected() {
        let loaded =
            parse("[[profile]]\nid = \"evil\"\ncommand = [\"sh\", \"-c\", \"\\u001b[2J\"]\n")
                .expect("well-formed");
        assert!(matches!(
            loaded.warnings.as_slice(),
            [ConfigWarning::Rejected {
                reason: MetadataError::BadChar { .. },
                ..
            }]
        ));
    }

    // -- Size recommendations -----------------------------------------------

    #[test]
    fn a_size_recommendation_is_read_as_written() {
        let loaded = parse(
            r#"
            [[profile]]
            id = "wide"
            min_size = { cols = 100, rows = 30 }
            "#,
        )
        .expect("valid");
        assert_eq!(
            loaded.config.profile("wide").expect("present").min_size,
            Size::new(100, 30)
        );
    }

    #[test]
    fn a_recommendation_below_the_layout_floor_drops_the_profile() {
        // A recommendation a split could never honor would silently mean
        // nothing, so it is refused rather than clamped.
        let loaded = parse(&format!(
            "[[profile]]\nid = \"tiny\"\nmin_size = {{ cols = {}, rows = 1 }}\n",
            MIN_PANE_SIZE.cols - 1
        ))
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::Rejected {
                id: "tiny".to_owned(),
                reason: MetadataError::MinSizeTooSmall {
                    recommended: Size::new(MIN_PANE_SIZE.cols - 1, 1),
                    floor: MIN_PANE_SIZE,
                },
            }]
        );
        assert!(loaded.config.profile("tiny").is_none());
    }

    #[test]
    fn a_configured_profile_can_express_a_built_in_exactly() {
        // The rule that keeps built-ins from being privileged: the document can
        // rebuild `codex` field for field.
        let loaded = parse(&format!(
            "[[profile]]\nid = \"codex-clone\"\ncommand = [\"codex\"]\ndefault_name = \"codex\"\nmin_size = {{ cols = {}, rows = {} }}\n",
            HARNESS_MIN_SIZE.cols, HARNESS_MIN_SIZE.rows
        ))
        .expect("valid");
        let clone = loaded.config.profile("codex-clone").expect("present");
        let built_in = Profile::codex();
        assert_eq!(clone.command, built_in.command);
        assert_eq!(clone.default_name, built_in.default_name);
        assert_eq!(clone.min_size, built_in.min_size);
        assert_eq!(clone.adapter, built_in.adapter);
    }

    // -- Adapters and names -------------------------------------------------

    #[test]
    fn an_adapter_is_carried_through_and_validated() {
        let loaded = parse(
            r#"
            [[profile]]
            id = "watched"
            adapter = "my-adapter"
            "#,
        )
        .expect("valid");
        assert_eq!(
            loaded
                .config
                .profile("watched")
                .expect("present")
                .adapter
                .as_ref()
                .map(AdapterId::as_str),
            Some("my-adapter")
        );

        let bad = parse(
            r#"
            [[profile]]
            id = "watched"
            adapter = "My Adapter"
            "#,
        )
        .expect("well-formed");
        assert!(matches!(
            bad.warnings.as_slice(),
            [ConfigWarning::Rejected {
                reason: MetadataError::BadChar { .. },
                ..
            }]
        ));
    }

    #[test]
    fn a_bad_id_is_reported_under_the_id_as_written() {
        // The ID is what failed, so the warning cannot name a `ProfileId` — it
        // has to keep the raw string or the user cannot find the entry.
        let loaded = parse(
            r#"
            [[profile]]
            id = "My Profile"
            "#,
        )
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::Rejected {
                id: "My Profile".to_owned(),
                reason: MetadataError::BadChar {
                    field: "profile id",
                    ch: 'M',
                },
            }]
        );
    }

    // -- Failure modes ------------------------------------------------------

    #[test]
    fn one_bad_profile_does_not_cost_the_good_ones() {
        let loaded = parse(
            r#"
            [[profile]]
            id = "good"
            command = ["hx"]

            [[profile]]
            id = "Bad Id"

            [[profile]]
            id = "also-good"
            command = ["less"]
            "#,
        )
        .expect("well-formed");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.config.profile("good").is_some());
        assert!(loaded.config.profile("also-good").is_some());
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_an_ignored_line() {
        // A silently ignored typo is a setting the user believes is applied.
        let err = parse(
            r#"
            [[profile]]
            id = "notes"
            comand = ["hx"]
            "#,
        )
        .expect_err("unknown key");
        assert!(err.message().contains("comand"), "{err}");

        assert!(parse("[[porfile]]\nid = \"notes\"\n").is_err());
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse("[[profile]\nid = \"notes\"").is_err());
        assert!(parse("[[profile]]\nid = 7\n").is_err());
    }

    #[test]
    fn a_missing_id_is_an_error_because_nothing_can_name_the_entry() {
        assert!(parse("[[profile]]\ncommand = [\"hx\"]\n").is_err());
    }

    // -- Keys ---------------------------------------------------------------

    fn key(text: &str) -> Key {
        Key::parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    #[test]
    fn a_document_with_no_keys_table_is_the_c_b_defaults() {
        let loaded = parse("").expect("valid");
        assert_eq!(loaded.config.keys(), &Keymap::defaults());
        assert_eq!(loaded.config.keys().prefix(), key("C-b"));
    }

    #[test]
    fn a_rebound_prefix_keeps_every_default_binding() {
        let loaded = parse(
            r#"
            [keys]
            prefix = "C-a"
            "#,
        )
        .expect("valid");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.keys().prefix(), key("C-a"));
        assert_eq!(
            loaded.config.keys().bindings(),
            Keymap::defaults().bindings(),
            "rebinding the prefix is not a reason to forget the table"
        );
    }

    #[test]
    fn a_binding_overrides_a_default_and_leaves_the_others_alone() {
        let loaded = parse(
            r#"
            [keys.bindings]
            "|" = "split-vertical"
            "x" = "toggle-zoom"
            "#,
        )
        .expect("valid");
        assert!(loaded.warnings.is_empty());
        let keys = loaded.config.keys();
        assert_eq!(keys.action(key("|")), Some(&Action::SplitVertical));
        assert_eq!(keys.action(key("x")), Some(&Action::ToggleZoom));
        // The overridden key's old action is still reachable where it was also
        // bound, and every untouched default is untouched.
        assert_eq!(keys.action(key("%")), Some(&Action::SplitVertical));
        assert_eq!(keys.action(key("c")), Some(&Action::NewTab));
        assert_eq!(keys.action(key("d")), Some(&Action::DetachClient));
    }

    #[test]
    fn none_removes_a_default_binding() {
        let loaded = parse(
            r#"
            [keys.bindings]
            "x" = "none"
            "#,
        )
        .expect("valid");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.config.keys().action(key("x")), None);
        assert_eq!(
            loaded.config.keys().action(key("z")),
            Some(&Action::ToggleZoom)
        );
    }

    #[test]
    fn a_chord_written_twice_is_a_document_error() {
        // TOML refuses the repeated key itself, which is what makes "last one
        // wins" impossible to reach by accident.
        assert!(
            parse("[keys.bindings]\n\"x\" = \"close-pane\"\n\"x\" = \"toggle-zoom\"\n").is_err()
        );
    }

    #[test]
    fn an_unspellable_prefix_keeps_c_b() {
        let loaded = parse(
            r#"
            [keys]
            prefix = "C-"
            "#,
        )
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::BadPrefix {
                key: "C-".to_owned(),
                reason: KeyError::MissingKey,
            }]
        );
        assert_eq!(
            loaded.config.keys().prefix(),
            key("C-b"),
            "a prefix nobody can press is a session with no way out"
        );
    }

    #[test]
    fn an_invalid_binding_is_dropped_alone_and_the_default_survives() {
        let loaded = parse(
            r#"
            [keys.bindings]
            "S-a" = "close-pane"
            "nope" = "new-tab"
            "v" = "split-vertical"
            "#,
        )
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [
                ConfigWarning::BadBinding {
                    key: "S-a".to_owned(),
                    reason: KeyError::ShiftedChar('a'),
                },
                ConfigWarning::BadBinding {
                    key: "nope".to_owned(),
                    reason: KeyError::UnknownKey("nope".to_owned()),
                },
            ],
            "warnings are ordered by chord so they do not depend on map order"
        );
        assert_eq!(
            loaded.config.keys().action(key("v")),
            Some(&Action::SplitVertical),
            "one bad line must not cost the good ones"
        );
        assert_eq!(
            loaded.config.keys().action(key("x")),
            Some(&Action::ClosePane)
        );
    }

    #[test]
    fn a_binding_naming_an_action_that_needs_typed_text_is_refused() {
        // `rename-tab` has no spelling because a chord carries no name to rename
        // the tab to; it is reached from a surface that can ask.
        let loaded = parse(
            r#"
            [keys.bindings]
            "r" = "rename-tab"
            "#,
        )
        .expect("well-formed");
        assert_eq!(
            loaded.warnings,
            [ConfigWarning::UnknownAction {
                key: "r".to_owned(),
                action: "rename-tab".to_owned(),
            }]
        );
        assert_eq!(loaded.config.keys().action(key("r")), None);
    }

    #[test]
    fn an_unknown_keys_field_is_a_document_error() {
        assert!(parse("[keys]\nprefx = \"C-a\"\n").is_err());
        assert!(parse("[keys]\nprefix = 7\n").is_err());
    }

    #[test]
    fn a_key_warning_names_the_chord_and_the_default_it_kept() {
        let message = ConfigWarning::BadPrefix {
            key: "C-".to_owned(),
            reason: KeyError::MissingKey,
        }
        .to_string();
        assert!(message.contains("C-b"), "{message}");
    }

    #[test]
    fn an_unreadable_document_falls_back_to_the_built_ins() {
        let (config, warnings) = parse_or_defaults("[[profile]\n");
        assert_eq!(config, Config::defaults());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with("invalid configuration"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_partially_valid_document_keeps_what_loaded() {
        let (config, warnings) = parse_or_defaults(
            r#"
            [[profile]]
            id = "notes"

            [[profile]]
            id = "Bad"
            "#,
        );
        assert!(config.profile("notes").is_some());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Bad"), "{warnings:?}");
    }
}
