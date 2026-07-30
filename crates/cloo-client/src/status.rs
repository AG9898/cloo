//! Client-local status data that must never stall frame composition.

use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Maximum time spent asking Git about one focused working directory.
pub const REPOSITORY_DEADLINE: Duration = Duration::from_millis(250);
/// How often a focused repository is checked for a changed branch or count.
pub const REPOSITORY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// One local wall-clock reading, independent of its eventual presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    hour: u8,
    minute: u8,
}

impl LocalTime {
    /// Constructs a valid 24-hour local time.
    #[must_use]
    pub const fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self { hour, minute })
        } else {
            None
        }
    }

    fn text(self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// Injectable source for the clock segment.
pub trait LocalClock {
    /// Returns the current local hour and minute, or nothing when unavailable.
    fn now(&self) -> Option<LocalTime>;
}

/// The process host's local wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl LocalClock for SystemClock {
    fn now(&self) -> Option<LocalTime> {
        let seconds: libc::time_t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .try_into()
            .ok()?;
        let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: `seconds` and `local` are valid for reads/writes for the
        // duration of the call. `localtime_r` initializes `local` on success.
        let result = unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) };
        if result.is_null() {
            return None;
        }
        // SAFETY: a non-null `localtime_r` result points at the initialized
        // output object supplied above.
        let local = unsafe { local.assume_init() };
        LocalTime::new(
            local.tm_hour.try_into().ok()?,
            local.tm_min.try_into().ok()?,
        )
    }
}

/// Repository facts derived from the focused pane's reported directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryStatus {
    /// The checked-out branch, absent for a detached head.
    pub branch: Option<String>,
    /// Changed tracked and untracked entries reported by Git.
    pub changes: u32,
}

/// Status values owned by one client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientStatus {
    clock: Option<String>,
    repository: Option<RepositoryStatus>,
}

impl ClientStatus {
    /// Refreshes the clock from an injected source.
    pub fn refresh_clock(&mut self, clock: &impl LocalClock) -> bool {
        let next = clock.now().map(LocalTime::text);
        if self.clock == next {
            return false;
        }
        self.clock = next;
        true
    }

    /// The latest clock text, when the local source answered.
    #[must_use]
    pub fn clock(&self) -> Option<&str> {
        self.clock.as_deref()
    }

    /// Replaces the repository answer for the current focused directory.
    pub fn set_repository(&mut self, repository: Option<RepositoryStatus>) -> bool {
        if self.repository == repository {
            return false;
        }
        self.repository = repository;
        true
    }

    /// The latest bounded repository answer.
    #[must_use]
    pub const fn repository(&self) -> Option<&RepositoryStatus> {
        self.repository.as_ref()
    }
}

/// Resolves repository data within [`REPOSITORY_DEADLINE`].
///
/// This performs blocking process work and must be called from a blocking task,
/// never from the render loop itself. Failure, a non-repository directory, and
/// a timeout all intentionally produce the same omitted value.
#[must_use]
pub fn repository_status(cwd: &Path) -> Option<RepositoryStatus> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = run_bounded(&mut command, REPOSITORY_DEADLINE).ok()??;
    output
        .status
        .success()
        .then(|| parse_repository(&output.stdout))
}

fn run_bounded(command: &mut Command, deadline: Duration) -> io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep((deadline - elapsed).min(Duration::from_millis(10)));
    }
}

fn parse_repository(stdout: &[u8]) -> RepositoryStatus {
    let mut branch = None;
    let mut changes = 0_u32;
    for line in String::from_utf8_lossy(stdout).lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            if !matches!(head, "(detached)" | "(unknown)") {
                branch = Some(head.to_owned());
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            changes = changes.saturating_add(1);
        }
    }
    RepositoryStatus { branch, changes }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedClock(Option<LocalTime>);

    impl LocalClock for FixedClock {
        fn now(&self) -> Option<LocalTime> {
            self.0
        }
    }

    #[test]
    fn status_clock_uses_the_injected_local_time_source() {
        let mut status = ClientStatus::default();
        assert!(status.refresh_clock(&FixedClock(LocalTime::new(7, 5))));
        assert_eq!(status.clock(), Some("07:05"));
        assert!(!status.refresh_clock(&FixedClock(LocalTime::new(7, 5))));
        assert!(status.refresh_clock(&FixedClock(None)));
        assert_eq!(status.clock(), None, "an unavailable clock is omitted");
    }

    #[test]
    fn repository_status_parses_branch_and_change_count_without_transcript_data() {
        let status = parse_repository(
            b"# branch.oid deadbeef\n# branch.head feature/status\n1 .M N... 100644 100644 100644 a b file\n? new-file\n",
        );
        assert_eq!(status.branch.as_deref(), Some("feature/status"));
        assert_eq!(status.changes, 2);

        let detached = parse_repository(b"# branch.head (detached)\n");
        assert_eq!(detached.branch, None);
        assert_eq!(detached.changes, 0);
    }

    #[test]
    fn repository_status_command_has_a_hard_deadline() {
        let mut command = Command::new("sleep");
        command.arg("1");
        let started = Instant::now();
        assert_eq!(
            run_bounded(&mut command, Duration::from_millis(25)).expect("sleep starts"),
            None
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a slow lookup must be killed rather than block a frame"
        );
    }

    #[test]
    fn repository_status_omits_a_directory_that_is_not_a_repository() {
        assert_eq!(repository_status(Path::new("/")), None);
    }
}
