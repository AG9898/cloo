//! Launch profiles: the data a pane is created from.
//!
//! A profile is a **command template plus presentation defaults**, and nothing
//! else. It supplies what to run, what to call the pane by default, how small
//! the pane may usefully get, and whether an opt-in local adapter is expected to
//! report state for it.
//!
//! Three rules from `docs/AGENT_WORKFLOWS.md` shape this module:
//!
//! - **The built-ins are data.** [`Profile::generic`], [`Profile::codex`], and
//!   [`Profile::claude`] are three values of the same struct. There is no
//!   per-vendor branch, no per-vendor trait, and nothing a fourth profile from a
//!   user's configuration could not also express — which is the whole point:
//!   adding a harness must never mean adding code.
//! - **No vendor dependency.** `codex` and `claude` name an executable the user
//!   already has, the way a shell alias would. cloo never links a vendor SDK,
//!   never calls a cloud API, and never requires an account.
//! - **Validation is pure.** [`Profile::validate`] checks *shape*: a usable ID,
//!   a non-empty program, printable defaults, a recommendation cloo could
//!   actually honor. It never asks the filesystem whether the program exists —
//!   `cloo-core` performs no I/O, and a missing executable is a launch-time
//!   failure the server reports ([`crate::pane`] holds the same line for a
//!   working directory).
//!
//! ```
//! use cloo_core::profile::{Profile, ProfileCommand};
//!
//! let claude = Profile::claude();
//! assert_eq!(claude.id.as_str(), "claude");
//! assert!(matches!(claude.command, ProfileCommand::Program { .. }));
//! assert!(claude.validate().is_ok());
//! ```

use cloo_proto::Size;

use crate::error::MetadataError;
use crate::layout::MIN_PANE_SIZE;

/// The longest a profile ID may be. Long enough for a descriptive local profile,
/// short enough to sit in a launcher list beside its command.
pub const MAX_PROFILE_ID: usize = 32;

/// The longest a profile's default pane name may be.
pub const MAX_DEFAULT_NAME: usize = 64;

/// The longest an adapter ID may be.
pub const MAX_ADAPTER_ID: usize = 32;

// ---------------------------------------------------------------------------
// Profile ID
// ---------------------------------------------------------------------------

/// A profile's stable identifier: what a user types to launch it.
///
/// Deliberately narrow — lowercase ASCII letters, digits, `-`, and `_` — so an
/// ID is unambiguous on a command line, in a config file, and in a keybinding,
/// and so two profiles cannot differ only by case or by an invisible character.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId(String);

impl ProfileId {
    /// Validates and wraps a profile ID.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`], [`MetadataError::TooLong`], or
    /// [`MetadataError::BadChar`] when the ID is not in the accepted alphabet.
    pub fn new(id: impl Into<String>) -> Result<Self, MetadataError> {
        let id = id.into();
        if id.is_empty() {
            return Err(MetadataError::Empty("profile id"));
        }
        if id.chars().count() > MAX_PROFILE_ID {
            return Err(MetadataError::TooLong {
                field: "profile id",
                len: id.chars().count(),
                max: MAX_PROFILE_ID,
            });
        }
        if let Some(ch) = id
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
        {
            return Err(MetadataError::BadChar {
                field: "profile id",
                ch,
            });
        }
        Ok(Self(id))
    }

    /// The ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Adapter ID
// ---------------------------------------------------------------------------

/// The name of an opt-in local state adapter.
///
/// An adapter is advisory: it may report a pane's attention state over the
/// local control interface, and everything it reports stays visibly attributed
/// to it ([`crate::pane::AttentionSource::Adapter`]). Naming one in a profile
/// does not make cloo depend on it — a profile whose adapter never connects
/// simply leaves the pane [`Unknown`](crate::pane::AttentionState::Unknown).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterId(String);

impl AdapterId {
    /// Validates and wraps an adapter ID. Same alphabet as [`ProfileId`].
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`], [`MetadataError::TooLong`], or
    /// [`MetadataError::BadChar`].
    pub fn new(id: impl Into<String>) -> Result<Self, MetadataError> {
        let id = id.into();
        if id.is_empty() {
            return Err(MetadataError::Empty("adapter id"));
        }
        if id.chars().count() > MAX_ADAPTER_ID {
            return Err(MetadataError::TooLong {
                field: "adapter id",
                len: id.chars().count(),
                max: MAX_ADAPTER_ID,
            });
        }
        if let Some(ch) = id
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
        {
            return Err(MetadataError::BadChar {
                field: "adapter id",
                ch,
            });
        }
        Ok(Self(id))
    }

    /// The ID as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for AdapterId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Command template
// ---------------------------------------------------------------------------

/// What a profile launches.
///
/// There is no shell-string variant on purpose: a template is an argv, so a pane
/// name, a task label, or a working directory can never be word-split or
/// re-interpreted by a shell on the way to `execvp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileCommand {
    /// The user's login shell. Resolving it means reading `$SHELL` and falling
    /// back to `/bin/sh`, which is I/O and therefore the server's job — this
    /// variant is the *request*, not the answer.
    LoginShell,
    /// An explicit program and its arguments, run without a shell.
    Program {
        /// The executable, resolved on `PATH` at launch.
        program: String,
        /// Arguments passed verbatim.
        args: Vec<String>,
    },
}

impl ProfileCommand {
    /// Builds a program template with no arguments.
    #[must_use]
    pub fn program(program: impl Into<String>) -> Self {
        Self::Program {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Checks the template's shape.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`] for a blank program and
    /// [`MetadataError::BadChar`] for a control character or a NUL in the
    /// program or any argument — a NUL cannot survive the C string `execvp`
    /// wants, and a control character in an argument is never intentional.
    pub fn validate(&self) -> Result<(), MetadataError> {
        let Self::Program { program, args } = self else {
            return Ok(());
        };
        if program.is_empty() {
            return Err(MetadataError::Empty("profile command"));
        }
        for part in core::iter::once(program).chain(args) {
            if let Some(ch) = part.chars().find(|c| c.is_control()) {
                return Err(MetadataError::BadChar {
                    field: "profile command",
                    ch,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// A launch profile.
///
/// Every field is data. A local profile from configuration is built with the
/// same constructor as a built-in and validated by the same function, so there
/// is no privileged shape a user cannot reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// What the user types to launch it.
    pub id: ProfileId,
    /// What to run.
    pub command: ProfileCommand,
    /// The pane name used when the user supplies none.
    pub default_name: String,
    /// The smallest geometry at which this profile is usable.
    ///
    /// Advisory in exactly one direction: it can stop a split that would make a
    /// pane useless, and it can never make an already-small client safe, since
    /// the session is the minimum of every attached client's size.
    pub min_size: Size,
    /// The opt-in local adapter expected to report this pane's state, if any.
    pub adapter: Option<AdapterId>,
}

impl Profile {
    /// Builds a profile from an already-validated ID.
    #[must_use]
    pub fn new(id: ProfileId, command: ProfileCommand, default_name: impl Into<String>) -> Self {
        Self {
            id,
            command,
            default_name: default_name.into(),
            min_size: MIN_PANE_SIZE,
            adapter: None,
        }
    }

    /// Sets the recommended minimum geometry.
    #[must_use]
    pub const fn min_size(mut self, size: Size) -> Self {
        self.min_size = size;
        self
    }

    /// Names the opt-in adapter expected to report this pane's state.
    #[must_use]
    pub fn adapter(mut self, adapter: AdapterId) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// Checks every field's shape.
    ///
    /// # Errors
    ///
    /// A [`MetadataError`] naming the first field that is unusable. The ID and
    /// the adapter ID were validated when they were constructed, so what is left
    /// is the command template, the default name, and the recommendation.
    pub fn validate(&self) -> Result<(), MetadataError> {
        self.command.validate()?;
        if self.default_name.is_empty() {
            return Err(MetadataError::Empty("profile default name"));
        }
        if self.default_name.chars().count() > MAX_DEFAULT_NAME {
            return Err(MetadataError::TooLong {
                field: "profile default name",
                len: self.default_name.chars().count(),
                max: MAX_DEFAULT_NAME,
            });
        }
        if let Some(ch) = self.default_name.chars().find(|c| c.is_control()) {
            return Err(MetadataError::BadChar {
                field: "profile default name",
                ch,
            });
        }
        if self.min_size.cols < MIN_PANE_SIZE.cols || self.min_size.rows < MIN_PANE_SIZE.rows {
            return Err(MetadataError::MinSizeTooSmall {
                recommended: self.min_size,
                floor: MIN_PANE_SIZE,
            });
        }
        Ok(())
    }

    // -- Built-ins ----------------------------------------------------------

    /// The `generic` profile: an ordinary shell pane.
    #[must_use]
    pub fn generic() -> Self {
        Self::new(
            ProfileId("generic".to_owned()),
            ProfileCommand::LoginShell,
            "shell",
        )
    }

    /// The `codex` profile.
    ///
    /// The recommendation is wider than the layout floor because a harness that
    /// draws a full-width transcript is unreadable in a 20-column pane, not
    /// because cloo knows anything about the program.
    #[must_use]
    pub fn codex() -> Self {
        Self::new(
            ProfileId("codex".to_owned()),
            ProfileCommand::program("codex"),
            "codex",
        )
        .min_size(HARNESS_MIN_SIZE)
    }

    /// The `claude` profile.
    #[must_use]
    pub fn claude() -> Self {
        Self::new(
            ProfileId("claude".to_owned()),
            ProfileCommand::program("claude"),
            "claude",
        )
        .min_size(HARNESS_MIN_SIZE)
    }

    /// The three built-in profiles, in launcher order.
    #[must_use]
    pub fn built_ins() -> Vec<Self> {
        vec![Self::generic(), Self::codex(), Self::claude()]
    }
}

/// The recommended minimum for a full-screen coding harness.
pub const HARNESS_MIN_SIZE: Size = Size::new(60, 15);

#[cfg(test)]
mod tests {
    use super::*;

    // -- IDs ----------------------------------------------------------------

    #[test]
    fn a_profile_id_accepts_the_documented_alphabet() {
        for id in ["generic", "codex", "claude", "my-profile", "agent_2"] {
            assert!(ProfileId::new(id).is_ok(), "{id} should be accepted");
        }
    }

    #[test]
    fn a_profile_id_rejects_everything_else() {
        // Uppercase and whitespace would let two profiles differ invisibly; the
        // escape byte is what a name could otherwise use to repaint the chrome
        // drawing it.
        for id in ["Codex", "my profile", "cod\u{1b}ex", "café", "dot.ted"] {
            assert!(ProfileId::new(id).is_err(), "{id:?} should be rejected");
        }
        assert_eq!(ProfileId::new(""), Err(MetadataError::Empty("profile id")));
    }

    #[test]
    fn a_profile_id_is_bounded() {
        let long = "a".repeat(MAX_PROFILE_ID + 1);
        assert!(matches!(
            ProfileId::new(long),
            Err(MetadataError::TooLong { .. })
        ));
        assert!(ProfileId::new("a".repeat(MAX_PROFILE_ID)).is_ok());
    }

    #[test]
    fn an_adapter_id_uses_the_same_alphabet() {
        assert!(AdapterId::new("claude-adapter").is_ok());
        assert!(AdapterId::new("Claude").is_err());
        assert!(AdapterId::new("").is_err());
    }

    // -- Command templates --------------------------------------------------

    #[test]
    fn a_login_shell_template_carries_no_program_to_validate() {
        // Resolving `$SHELL` is I/O, so the variant is the request rather than
        // the answer and there is nothing here to check.
        assert!(ProfileCommand::LoginShell.validate().is_ok());
    }

    #[test]
    fn a_command_template_rejects_a_nul_or_control_character() {
        let with_nul = ProfileCommand::Program {
            program: "codex".to_owned(),
            args: vec!["--flag=a\0b".to_owned()],
        };
        assert!(matches!(
            with_nul.validate(),
            Err(MetadataError::BadChar { .. })
        ));
        assert!(matches!(
            ProfileCommand::program("").validate(),
            Err(MetadataError::Empty(_))
        ));
    }

    #[test]
    fn a_command_template_keeps_its_arguments_verbatim() {
        // An argv, never a shell string: a space in an argument is one argument.
        let cmd = ProfileCommand::Program {
            program: "claude".to_owned(),
            args: vec!["--append-system-prompt".to_owned(), "be brief".to_owned()],
        };
        assert!(cmd.validate().is_ok());
        let ProfileCommand::Program { args, .. } = &cmd else {
            unreachable!()
        };
        assert_eq!(args.len(), 2);
    }

    // -- Built-ins ----------------------------------------------------------

    #[test]
    fn the_built_ins_are_the_three_documented_profiles() {
        let ids: Vec<String> = Profile::built_ins()
            .iter()
            .map(|p| p.id.to_string())
            .collect();
        assert_eq!(ids, ["generic", "codex", "claude"]);
    }

    #[test]
    fn every_built_in_validates() {
        for profile in Profile::built_ins() {
            assert!(profile.validate().is_ok(), "{} is invalid", profile.id);
        }
    }

    #[test]
    fn the_built_ins_differ_only_in_data() {
        // The test that would fail if a vendor ever earned a special case: the
        // harness profiles are the generic one with different field values.
        let codex = Profile::codex();
        let claude = Profile::claude();
        assert_eq!(codex.min_size, claude.min_size);
        assert_ne!(codex.command, claude.command);
        assert_eq!(
            codex,
            Profile::new(
                ProfileId::new("codex").expect("valid id"),
                ProfileCommand::program("codex"),
                "codex",
            )
            .min_size(HARNESS_MIN_SIZE)
        );
    }

    #[test]
    fn a_built_in_carries_no_adapter() {
        // An adapter is opt-in local configuration. Shipping one wired up by
        // default would make an advisory signal look authoritative.
        for profile in Profile::built_ins() {
            assert_eq!(profile.adapter, None);
        }
    }

    #[test]
    fn a_harness_profile_recommends_more_than_the_layout_floor() {
        assert!(Profile::codex().min_size.cols > MIN_PANE_SIZE.cols);
        assert!(Profile::codex().min_size.rows > MIN_PANE_SIZE.rows);
    }

    // -- User profiles ------------------------------------------------------

    #[test]
    fn a_user_profile_is_built_the_same_way() {
        let mine = Profile::new(
            ProfileId::new("notes").expect("valid id"),
            ProfileCommand::Program {
                program: "hx".to_owned(),
                args: vec!["notes.md".to_owned()],
            },
            "notes",
        )
        .adapter(AdapterId::new("my-adapter").expect("valid id"));
        assert!(mine.validate().is_ok());
        assert_eq!(mine.min_size, MIN_PANE_SIZE);
    }

    #[test]
    fn a_recommendation_below_the_layout_floor_is_rejected() {
        // Otherwise a profile could ask for a size a split would never produce,
        // and the recommendation would silently mean nothing.
        let bad = Profile::generic().min_size(Size::new(10, 2));
        assert!(matches!(
            bad.validate(),
            Err(MetadataError::MinSizeTooSmall { .. })
        ));
    }

    #[test]
    fn a_default_name_is_bounded_and_printable() {
        let mut profile = Profile::generic();
        profile.default_name = "a".repeat(MAX_DEFAULT_NAME + 1);
        assert!(matches!(
            profile.validate(),
            Err(MetadataError::TooLong { .. })
        ));

        profile.default_name = "sh\u{7}ell".to_owned();
        assert!(matches!(
            profile.validate(),
            Err(MetadataError::BadChar { .. })
        ));

        profile.default_name = String::new();
        assert!(matches!(profile.validate(), Err(MetadataError::Empty(_))));
    }
}
