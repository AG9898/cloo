//! The command line, and the launch it resolves to.
//!
//! Parsing is split in two on purpose. [`parse`] is pure: it turns an argument
//! vector into a [`Request`], which is nothing but the strings the user typed.
//! [`Request::resolve`] is where those strings become a validated
//! [`Launch`] — a profile looked up by ID, a name and a task label checked
//! against `cloo-core`'s rules, and a working directory made absolute. Only the
//! second half needs a process to run in, which is what makes the first half
//! testable without one.
//!
//! The rule the whole module exists to hold: **a pane's identity is what the
//! user said it is.** `--task` has no default, no inference, and no fallback to
//! something scraped out of the child's output. A pane whose task nobody gave
//! has no task, and the chrome says so by leaving it out.

use std::path::{Path, PathBuf};

use cloo_core::config::Config;
use cloo_core::pane::{PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{Profile, ProfileCommand};
use cloo_server::launch::Launch;

/// What the command line asked cloo to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Print the version and exit.
    Version,
    /// Print the usage and exit.
    Help,
    /// Run a pane.
    Run(Request),
}

/// The launch options as typed, before any of them are validated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// `--profile`, if given.
    pub profile: Option<String>,
    /// `--name`, if given.
    pub name: Option<String>,
    /// `--task`, if given.
    pub task: Option<String>,
    /// `--cwd`, if given.
    pub cwd: Option<String>,
    /// A program and its arguments, if given positionally.
    pub argv: Vec<String>,
}

/// Everything the command line can be wrong about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// A flag cloo does not know.
    UnknownFlag(String),
    /// A flag that takes a value was given none.
    MissingValue(&'static str),
    /// A flag was given twice.
    Repeated(&'static str),
    /// Both `--profile` and a program were given.
    ProfileAndProgram,
    /// No profile with that ID is defined.
    UnknownProfile(String),
    /// A name, task label, working directory, or profile was unusable.
    Invalid(cloo_core::MetadataError),
    /// The process's own directory could not be read, so a relative `--cwd`
    /// could not be resolved.
    NoCurrentDir(String),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFlag(flag) => write!(f, "unrecognized option '{flag}'"),
            Self::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Self::Repeated(flag) => write!(f, "{flag} was given more than once"),
            Self::ProfileAndProgram => f.write_str(
                "--profile and a program cannot both be given: a profile is already a command",
            ),
            Self::UnknownProfile(id) => {
                write!(
                    f,
                    "no profile named {id:?}; known profiles: {}",
                    profile_ids()
                )
            }
            Self::Invalid(err) => write!(f, "{err}"),
            Self::NoCurrentDir(err) => {
                write!(f, "could not read the current directory: {err}")
            }
        }
    }
}

impl std::error::Error for CliError {}

impl From<cloo_core::MetadataError> for CliError {
    fn from(value: cloo_core::MetadataError) -> Self {
        Self::Invalid(value)
    }
}

/// The IDs a `--profile` may name, for the usage text and for an error.
#[must_use]
pub fn profile_ids() -> String {
    Config::defaults()
        .profiles()
        .iter()
        .map(|profile| profile.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses an argument vector.
///
/// Everything up to the first non-flag argument is an option; that argument and
/// everything after it is the program and its arguments, so `cloo sh -c 'x'`
/// passes `-c` to `sh` rather than to cloo. An explicit `--` ends the options
/// too, which is how a program whose name starts with a dash is run.
///
/// # Errors
///
/// Returns the [`CliError`] describing the first thing that was wrong.
pub fn parse(args: &[String]) -> Result<Invocation, CliError> {
    let mut request = Request::default();
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-V" | "--version" => return Ok(Invocation::Version),
            "-h" | "--help" => return Ok(Invocation::Help),
            "--" => {
                request.argv = rest.cloned().collect();
                break;
            }
            "-p" | "--profile" => set(&mut request.profile, "--profile", rest.next())?,
            "-n" | "--name" => set(&mut request.name, "--name", rest.next())?,
            "-t" | "--task" => set(&mut request.task, "--task", rest.next())?,
            "-c" | "--cwd" => set(&mut request.cwd, "--cwd", rest.next())?,
            // An unrecognized flag is a mistake, not a program name. Treating
            // it as one would try to execute `--colour` and then blame the
            // user's PATH.
            flag if flag.starts_with('-') => {
                return Err(CliError::UnknownFlag(flag.to_owned()));
            }
            program => {
                request.argv = core::iter::once(program.to_owned())
                    .chain(rest.cloned())
                    .collect();
                break;
            }
        }
    }

    if request.profile.is_some() && !request.argv.is_empty() {
        return Err(CliError::ProfileAndProgram);
    }
    Ok(Invocation::Run(request))
}

/// Fills one option, refusing a repeat rather than silently keeping the last.
fn set(
    slot: &mut Option<String>,
    flag: &'static str,
    value: Option<&String>,
) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError::Repeated(flag));
    }
    *slot = Some(value.ok_or(CliError::MissingValue(flag))?.clone());
    Ok(())
}

impl Request {
    /// Resolves this request against the running process.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] naming the option that was unusable.
    pub fn into_launch(self) -> Result<Launch, CliError> {
        // File I/O lives in `cloo-server`; the core parser receives only text.
        // Startup falls back safely on a bad optional file, while preserving a
        // visible diagnostic rather than silently pretending it applied.
        let loaded = cloo_server::config::load_from_environment();
        for diagnostic in loaded.diagnostics {
            eprintln!("cloo: warning: {diagnostic}");
        }
        let here =
            std::env::current_dir().map_err(|err| CliError::NoCurrentDir(err.to_string()))?;
        self.resolve(&loaded.config, &here)
    }

    /// The pure form of [`into_launch`](Self::into_launch): `here` is what a
    /// relative `--cwd` is resolved against.
    ///
    /// # Errors
    ///
    /// As [`into_launch`](Self::into_launch).
    pub fn resolve(self, config: &Config, here: &Path) -> Result<Launch, CliError> {
        let profile = match &self.profile {
            Some(id) => config
                .profile(id)
                .cloned()
                .ok_or_else(|| CliError::UnknownProfile(id.clone()))?,
            None if self.argv.is_empty() => Profile::generic(),
            None => ad_hoc(&self.argv),
        };

        let name = self.name.map(PaneName::new).transpose()?;
        let task = self.task.map(TaskLabel::new).transpose()?;
        // Resolution happens here rather than in the model: a relative path
        // means whatever the *user's* shell was in, and the daemon's own
        // directory is not that and is not stable across restarts.
        let cwd = WorkingDir::new(absolute(self.cwd.as_deref(), here))?;

        Ok(Launch::new(profile, name, task, cwd)?)
    }
}

/// The profile a bare `cloo <program>` runs.
///
/// The `generic` profile with its command replaced: an explicitly named program
/// *is* an ordinary pane, and saying so keeps the launcher from growing a
/// nameless fourth entry. The default name is the program's file name, which is
/// what a user would have called it anyway.
fn ad_hoc(argv: &[String]) -> Profile {
    let mut profile = Profile::generic();
    let (program, args) = argv.split_first().map_or_else(
        || (String::new(), Vec::new()),
        |(first, rest)| (first.clone(), rest.to_vec()),
    );
    profile.default_name = Path::new(&program).file_name().map_or_else(
        || program.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    profile.command = ProfileCommand::Program { program, args };
    profile
}

/// Makes `cwd` absolute against `here`, without touching the filesystem.
///
/// Lexical on purpose. `canonicalize` would resolve symlinks, and a user who
/// launched a pane in a symlinked worktree wants to see the path they typed, not
/// the one it happens to point at today.
fn absolute(cwd: Option<&str>, here: &Path) -> PathBuf {
    match cwd {
        None => here.to_path_buf(),
        Some(path) if Path::new(path).is_absolute() => PathBuf::from(path),
        // `~` is left alone: it is the shell's, and `WorkingDir` refuses it
        // rather than joining it onto the current directory as a literal.
        Some(path) if path.starts_with('~') => PathBuf::from(path),
        Some(path) => here.join(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }

    fn request(raw: &[&str]) -> Request {
        match parse(&args(raw)) {
            Ok(Invocation::Run(request)) => request,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn here() -> PathBuf {
        PathBuf::from("/home/dev")
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn a_bare_invocation_asks_for_nothing_in_particular() {
        assert_eq!(request(&[]), Request::default());
    }

    #[test]
    fn version_and_help_win_over_everything() {
        assert_eq!(parse(&args(&["--version"])), Ok(Invocation::Version));
        assert_eq!(parse(&args(&["-V"])), Ok(Invocation::Version));
        assert_eq!(parse(&args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(parse(&args(&["-h"])), Ok(Invocation::Help));
    }

    #[test]
    fn every_pane_option_is_read() {
        let request = request(&[
            "--profile",
            "codex",
            "--name",
            "api",
            "--task",
            "fix the flaky test",
            "--cwd",
            "/srv/api",
        ]);
        assert_eq!(request.profile.as_deref(), Some("codex"));
        assert_eq!(request.name.as_deref(), Some("api"));
        assert_eq!(request.task.as_deref(), Some("fix the flaky test"));
        assert_eq!(request.cwd.as_deref(), Some("/srv/api"));
        assert!(request.argv.is_empty());
    }

    #[test]
    fn options_stop_at_the_program() {
        // The whole reason the split is positional: `-c` here is `sh`'s, and a
        // parser that kept scanning would reject the user's own command line.
        let request = request(&["--name", "build", "sh", "-c", "make", "--task", "no"]);
        assert_eq!(request.name.as_deref(), Some("build"));
        assert_eq!(request.argv, ["sh", "-c", "make", "--task", "no"]);
        assert_eq!(request.task, None);
    }

    #[test]
    fn a_double_dash_runs_a_program_whose_name_looks_like_a_flag() {
        assert_eq!(
            request(&["--", "--weird-program"]).argv,
            ["--weird-program"]
        );
    }

    #[test]
    fn an_unknown_flag_is_a_mistake_rather_than_a_program() {
        assert_eq!(
            parse(&args(&["--colour"])),
            Err(CliError::UnknownFlag("--colour".to_owned()))
        );
    }

    #[test]
    fn a_flag_needs_its_value_and_may_not_repeat() {
        assert_eq!(
            parse(&args(&["--name"])),
            Err(CliError::MissingValue("--name"))
        );
        assert_eq!(
            parse(&args(&["--task", "a", "--task", "b"])),
            Err(CliError::Repeated("--task")),
            "keeping the last silently would be a setting the user cannot see"
        );
    }

    #[test]
    fn a_profile_and_a_program_are_two_answers_to_one_question() {
        assert_eq!(
            parse(&args(&["--profile", "codex", "sh"])),
            Err(CliError::ProfileAndProgram)
        );
    }

    // -- resolution ---------------------------------------------------------

    #[test]
    fn a_named_profile_is_launched_with_its_own_defaults() {
        let launch = request(&["--profile", "claude"])
            .resolve(&Config::defaults(), &here())
            .expect("a built-in profile resolves");
        assert_eq!(launch.profile().id.as_str(), "claude");
        assert_eq!(launch.meta().name.as_str(), "claude");
        assert_eq!(launch.meta().task, None, "a task is never invented");
        assert_eq!(launch.meta().cwd.as_path(), Path::new("/home/dev"));
    }

    #[test]
    fn the_user_name_task_and_directory_win() {
        let launch = request(&[
            "--profile",
            "codex",
            "--name",
            "api",
            "--task",
            "fix the flaky test",
            "--cwd",
            "/srv/api",
        ])
        .resolve(&Config::defaults(), &here())
        .expect("resolves");
        assert_eq!(launch.meta().name.as_str(), "api");
        assert_eq!(
            launch.meta().task.as_ref().map(TaskLabel::as_str),
            Some("fix the flaky test")
        );
        assert_eq!(launch.meta().cwd.as_path(), Path::new("/srv/api"));
    }

    #[test]
    fn a_local_profile_from_configuration_launches_the_same_way() {
        // The property that keeps the built-ins from being special: a profile
        // the user defined reaches the same launch path with no extra code.
        let loaded = cloo_core::config::parse(
            r#"
            [[profile]]
            id = "notes"
            command = ["hx", "notes.md"]
            "#,
        )
        .expect("valid document");
        let launch = request(&["--profile", "notes"])
            .resolve(&loaded.config, &here())
            .expect("resolves");
        assert_eq!(launch.profile().id.as_str(), "notes");
        assert_eq!(launch.meta().name.as_str(), "notes");
    }

    #[test]
    fn an_unknown_profile_names_the_ones_that_exist() {
        let err = request(&["--profile", "codx"])
            .resolve(&Config::defaults(), &here())
            .expect_err("an unknown profile must be refused");
        let message = err.to_string();
        assert!(message.contains("codx"), "got: {message}");
        assert!(message.contains("codex"), "got: {message}");
    }

    #[test]
    fn a_program_runs_as_a_generic_pane_named_for_itself() {
        let launch = request(&["/usr/bin/htop"])
            .resolve(&Config::defaults(), &here())
            .expect("resolves");
        assert_eq!(launch.profile().id.as_str(), "generic");
        assert_eq!(launch.meta().name.as_str(), "htop");
        assert_eq!(
            launch.profile().command,
            ProfileCommand::program("/usr/bin/htop")
        );
    }

    #[test]
    fn a_bare_invocation_is_the_generic_profile_and_its_login_shell() {
        let launch = request(&[])
            .resolve(&Config::defaults(), &here())
            .expect("resolves");
        assert_eq!(launch.profile().id.as_str(), "generic");
        assert_eq!(launch.profile().command, ProfileCommand::LoginShell);
    }

    #[test]
    fn a_relative_directory_is_resolved_against_the_users_own() {
        let launch = request(&["--cwd", "api"])
            .resolve(&Config::defaults(), &here())
            .expect("resolves");
        assert_eq!(launch.meta().cwd.as_path(), Path::new("/home/dev/api"));
    }

    #[test]
    fn a_tilde_is_the_shells_and_is_refused_rather_than_joined() {
        // Joining it would produce `/home/dev/~/api`, a directory that almost
        // certainly does not exist and that nobody asked for.
        let err = request(&["--cwd", "~/api"])
            .resolve(&Config::defaults(), &here())
            .expect_err("an unexpanded tilde must be refused");
        assert!(matches!(err, CliError::Invalid(_)), "got {err}");
    }

    #[test]
    fn a_name_or_task_that_could_repaint_the_chrome_is_refused() {
        for bad in [
            request(&["--name", "esc\u{1b}[31m"]),
            request(&["--task", "esc\u{1b}[31m"]),
        ] {
            assert!(
                matches!(
                    bad.resolve(&Config::defaults(), &here()),
                    Err(CliError::Invalid(_))
                ),
                "control characters must not reach a pane header"
            );
        }
    }
}
