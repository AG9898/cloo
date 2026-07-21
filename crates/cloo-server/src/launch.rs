//! Turning a profile plus what the user typed into a running pane.
//!
//! A [`Launch`] is the request: *this profile, under this name, for this task,
//! in this directory*. It is the only way a pane is created, and it is where the
//! two halves of `cloo-core`'s pure model meet the server's I/O — resolving
//! `$SHELL` for a [`ProfileCommand::LoginShell`] and handing a working directory
//! to `chdir` are both things `cloo-core` may not do.
//!
//! Three rules shape it:
//!
//! - **Everything is explicit.** The profile, the name, the task label, and the
//!   directory all come from the user or from the profile's own defaults.
//!   Nothing is ever derived from what a child prints — see
//!   `docs/AGENT_WORKFLOWS.md`. There is deliberately no way to construct a
//!   `Launch` from a grid, a process name, or a transcript.
//! - **Validation happens before a process exists.** [`Launch::new`] validates
//!   the profile and builds the pane's [`PaneMeta`] up front, so every later
//!   step is infallible except the spawn itself. The same ordering as split and
//!   close: ask the half that can refuse first, and a refusal never costs a
//!   child.
//! - **A missing executable is a launch-time failure.** `cloo-core` never asks
//!   the filesystem whether a program exists, and neither does this: the answer
//!   comes from `execvp`, and it surfaces as [`PtyError::Spawn`] naming the
//!   program.
//!
//! ```
//! use cloo_core::pane::WorkingDir;
//! use cloo_core::profile::Profile;
//! use cloo_server::launch::Launch;
//!
//! let launch = Launch::new(
//!     Profile::claude(),
//!     None,
//!     None,
//!     WorkingDir::new("/home/dev/api").expect("absolute"),
//! )
//! .expect("a built-in profile validates");
//! // The profile's default name, because the user supplied none.
//! assert_eq!(launch.meta().name.as_str(), "claude");
//! ```

use std::ffi::OsStr;

use cloo_core::error::MetadataError;
use cloo_core::pane::{PaneMeta, PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{Profile, ProfileCommand};

use crate::pty::PtyConfig;

/// The program run for a [`ProfileCommand::LoginShell`] when `$SHELL` says
/// nothing. POSIX guarantees it exists.
pub const FALLBACK_SHELL: &str = "/bin/sh";

/// A validated request to create one pane.
///
/// Holds the profile it launches and the metadata the pane will carry. The two
/// are built together so they cannot disagree: a pane's reported profile is the
/// profile whose command actually ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    profile: Profile,
    meta: PaneMeta,
}

impl Launch {
    /// Validates a profile and the user's overrides into a launch request.
    ///
    /// `name` and `task` are the user's; an absent name takes the profile's
    /// default, and an absent task stays absent rather than becoming an invented
    /// one.
    ///
    /// # Errors
    ///
    /// Returns the [`MetadataError`] naming the first unusable field. Nothing
    /// has been spawned and nothing has been changed when this fails.
    pub fn new(
        profile: Profile,
        name: Option<PaneName>,
        task: Option<TaskLabel>,
        cwd: WorkingDir,
    ) -> Result<Self, MetadataError> {
        profile.validate()?;
        let meta = PaneMeta::from_profile(&profile, name, task, cwd)?;
        Ok(Self { profile, meta })
    }

    /// The profile this launches.
    #[must_use]
    pub const fn profile(&self) -> &Profile {
        &self.profile
    }

    /// The metadata the pane will carry.
    #[must_use]
    pub const fn meta(&self) -> &PaneMeta {
        &self.meta
    }

    /// Applies this launch to a session's base configuration.
    ///
    /// The base carries what belongs to the *session* — the environment every
    /// pane inherits and the geometry the caller is about to correct — and this
    /// overwrites what belongs to the *profile*: the argv and the working
    /// directory. Splitting it that way is what lets a split spawn a different
    /// profile without losing the session's `TERM`.
    ///
    /// `$SHELL` is read here rather than in `cloo-core`, which performs no I/O.
    #[must_use]
    pub fn configure(&self, base: &PtyConfig) -> PtyConfig {
        let (program, args) = match &self.profile.command {
            ProfileCommand::LoginShell => (login_shell(), Vec::new()),
            ProfileCommand::Program { program, args } => (program.clone(), args.clone()),
        };
        base.clone()
            .command(&program, &args)
            .cwd(self.meta.cwd.as_path())
    }
}

/// The user's login shell, or the POSIX fallback.
#[must_use]
pub fn login_shell() -> String {
    shell_from(std::env::var_os("SHELL").as_deref())
}

/// The pure form of [`login_shell`].
fn shell_from(shell: Option<&OsStr>) -> String {
    match shell.map(|shell| shell.to_string_lossy().into_owned()) {
        Some(shell) if !shell.is_empty() => shell,
        _ => FALLBACK_SHELL.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_core::profile::{ProfileCommand, ProfileId};

    fn cwd() -> WorkingDir {
        WorkingDir::new("/home/dev/api").expect("absolute path")
    }

    #[test]
    fn a_launch_takes_the_profile_default_name_and_keeps_the_users() {
        let default = Launch::new(Profile::codex(), None, None, cwd()).expect("valid");
        assert_eq!(default.meta().name.as_str(), "codex");
        assert_eq!(default.meta().task, None);

        let explicit = Launch::new(
            Profile::codex(),
            Some(PaneName::new("api").expect("valid name")),
            Some(TaskLabel::new("fix the flaky test").expect("valid label")),
            cwd(),
        )
        .expect("valid");
        assert_eq!(explicit.meta().name.as_str(), "api");
        assert_eq!(
            explicit.meta().task.as_ref().map(TaskLabel::as_str),
            Some("fix the flaky test")
        );
    }

    #[test]
    fn an_invalid_profile_is_refused_before_anything_is_spawned() {
        // The failure path a configured profile reaches. It must be an error
        // rather than a panic or a silently corrected value, and it must happen
        // here rather than at `execvp`.
        let bad = Profile::new(
            ProfileId::new("broken").expect("valid id"),
            ProfileCommand::program(""),
            "broken",
        );
        assert!(matches!(
            Launch::new(bad, None, None, cwd()),
            Err(MetadataError::Empty(_))
        ));
    }

    #[test]
    fn a_program_profile_keeps_its_argv_verbatim() {
        let profile = Profile::new(
            ProfileId::new("notes").expect("valid id"),
            ProfileCommand::Program {
                program: "hx".to_owned(),
                args: vec!["my notes.md".to_owned()],
            },
            "notes",
        );
        let launch = Launch::new(profile, None, None, cwd()).expect("valid");
        let config = launch.configure(&PtyConfig::new("placeholder"));
        assert_eq!(config.program(), OsStr::new("hx"));
        assert_eq!(
            config.args(),
            [OsStr::new("my notes.md")],
            "an argument with a space is one argument, never word-split"
        );
        assert_eq!(
            config.working_dir(),
            Some(std::path::Path::new("/home/dev/api"))
        );
    }

    #[test]
    fn the_session_environment_survives_the_profiles_argv() {
        // A split must not lose the session's `TERM` just because it launched a
        // different profile.
        let base = PtyConfig::new("placeholder").env("TERM", "xterm-256color");
        let launch = Launch::new(Profile::codex(), None, None, cwd()).expect("valid");
        let config = launch.configure(&base);
        assert_eq!(
            config.env_overrides(),
            [(
                std::ffi::OsString::from("TERM"),
                std::ffi::OsString::from("xterm-256color")
            )]
        );
    }

    #[test]
    fn a_login_shell_profile_resolves_at_launch_time() {
        let launch = Launch::new(Profile::generic(), None, None, cwd()).expect("valid");
        let config = launch.configure(&PtyConfig::new("placeholder"));
        assert_eq!(config.program(), OsStr::new(&login_shell()));
        assert!(config.args().is_empty());
    }

    #[test]
    fn an_absent_or_empty_shell_falls_back_to_sh() {
        assert_eq!(
            shell_from(Some(OsStr::new("/usr/bin/fish"))),
            "/usr/bin/fish"
        );
        assert_eq!(shell_from(None), FALLBACK_SHELL);
        assert_eq!(shell_from(Some(OsStr::new(""))), FALLBACK_SHELL);
    }
}
