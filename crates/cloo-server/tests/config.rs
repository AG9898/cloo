//! Integration coverage for server-owned configuration reloads.
//!
//! These tests read and replace real files, so they live under `tests/` rather
//! than beside the pure path resolver. Each fixture owns a unique directory;
//! no process environment is changed and no test can observe another's file.
//!
//! The daemon tests below drive a real socket and a real child, and speak the
//! wire protocol directly rather than through `cloo-client`: `cloo-server` may
//! never name that crate, not even as a dev-dependency. What they need is only
//! the server's own half — an attach, then the frames it publishes — so nothing
//! here requires the composition root.
//!
//! `SIGHUP` is process-wide, and a running daemon owns a listener for it, so
//! every test that raises one holds [`SIGHUP_TESTS`] for as long as its daemon
//! is alive. Without that, one test's reload would land in another's frames.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use cloo_core::keymap::{DEFAULT_PREFIX, Key};
use cloo_core::pane::{PaneName, WorkingDir};
use cloo_core::profile::{Profile, ProfileCommand};
use cloo_core::theme::ThemeChoice;
use cloo_proto::{
    Action, ClientMessage, FrameStream, PROTOCOL_VERSION, PaneInfo, ServerMessage, Size, TermCaps,
};
use cloo_server::config::{ConfigFile, ConfigManager, Reload, ReloadWatch};
use cloo_server::daemon::Daemon;
use cloo_server::launch::Launch;
use cloo_server::pty::PtyConfig;
use cloo_server::socket::Listener;
use tokio::net::UnixStream;

/// One isolated configuration file, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cloo-config-test-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the test configuration directory is creatable");
        Self(path)
    }

    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run").join("session.sock")
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn profile(id: &str) -> String {
    format!("[[profile]]\nid = {id:?}\ncommand = [\"sh\"]\n")
}

/// How long any single wire expectation may take before a test fails.
///
/// A deadline, not a delay: every wait returns as soon as its frame arrives.
const PATIENCE: Duration = Duration::from_secs(20);

/// Serializes every test that delivers a real `SIGHUP`.
///
/// A daemon installs a process-wide reload listener for as long as it runs, so
/// a signal raised by one test would otherwise reach another test's daemon and
/// publish a revision nobody asked for.
static SIGHUP_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A child that echoes each line back inside angle brackets.
///
/// The brackets matter: a pseudoterminal echoes typed input on its own, so a
/// bare `ping` on the grid would prove nothing about the child. `<ping>` can
/// only have come back through the child, the session actor, and the daemon's
/// damage publication — which is the property a reload must not disturb.
const ECHO_BRACKETED: &str = "while read line; do echo \"<$line>\"; done";

/// Requests one reload of every live configuration watcher in this process.
fn raise_sighup() {
    // SAFETY: every caller holds `SIGHUP_TESTS` and has a Tokio `SIGHUP`
    // listener installed, so the signal requests a reload rather than
    // terminating the test process, and no other test can be delivering one.
    let result = unsafe { libc::raise(libc::SIGHUP) };
    assert_eq!(
        result,
        0,
        "could not deliver SIGHUP: {}",
        std::io::Error::last_os_error()
    );
}

/// A launch running `script` under `sh`, as the built-in generic profile.
fn scripted(script: &str) -> Launch {
    let mut profile = Profile::generic();
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
    };
    Launch::new(
        profile,
        Some(PaneName::new("api").expect("a valid pane name")),
        None,
        WorkingDir::new("/").expect("an absolute directory"),
    )
    .expect("the generic profile validates")
}

/// Binds a daemon on `socket` with `manager` as its reloadable configuration.
fn spawn_daemon(
    socket: &Path,
    manager: ConfigManager,
    report: impl Fn(&str) + Send + Sync + 'static,
) -> tokio::task::JoinHandle<Result<std::process::ExitStatus, cloo_server::DaemonError>> {
    let base = PtyConfig::session(Size::new(80, 24))
        .expect("80x24 is a valid size")
        .env("TERM", "xterm-256color");
    let listener = Listener::bind(socket).expect("a fresh socket path must bind");
    let mut daemon = Daemon::new(listener, &base, scripted(ECHO_BRACKETED))
        .expect("the daemon must start")
        .with_config_manager(manager)
        .with_diagnostics(report);
    tokio::spawn(async move { daemon.run().await })
}

/// Attaches to `socket` and reads through the daemon's opening snapshot.
async fn attach(socket: &Path) -> FrameStream<UnixStream> {
    let stream = tokio::time::timeout(PATIENCE, async {
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => return stream,
                // The daemon binds before it accepts, so this only spins while
                // its task is being scheduled.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("the session socket must appear");

    let mut conn = FrameStream::new(stream);
    conn.send(&ClientMessage::Attach {
        protocol_version: PROTOCOL_VERSION,
        size: Size::new(80, 24),
        term_caps: TermCaps::default(),
        session: None,
    })
    .await
    .expect("the attach must send");
    let hello = until(&mut conn, |message| match message {
        ServerMessage::Hello { size, .. } => Some(*size),
        _ => None,
    })
    .await;
    assert_eq!(hello, Size::new(80, 24));
    conn
}

/// Reads frames until `want` answers, bounded by the deadline.
async fn until<T>(
    conn: &mut FrameStream<UnixStream>,
    mut want: impl FnMut(&ServerMessage) -> Option<T>,
) -> T {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match conn.recv::<ServerMessage>().await {
                Ok(Some(message)) => {
                    if let Some(found) = want(&message) {
                        return found;
                    }
                }
                Ok(None) => panic!("the daemon closed the connection"),
                Err(err) => panic!("the connection failed: {err}"),
            }
        }
    })
    .await
    .expect("the expected frame must arrive")
}

/// Reads until the pane table names `profile`, then returns it.
///
/// A revision arriving while this waits is a failure by itself: the only
/// reloads these tests request are the ones they assert about, so an
/// unannounced one means a refused document published a revision.
async fn until_launched(conn: &mut FrameStream<UnixStream>, profile: &str) -> Vec<PaneInfo> {
    until(conn, |message| match message {
        ServerMessage::ConfigReloaded { revision } => {
            panic!("an unrequested configuration revision {revision} was published")
        }
        ServerMessage::Panes(panes) if panes.iter().any(|info| info.profile == profile) => {
            Some(panes.clone())
        }
        _ => None,
    })
    .await
}

#[test]
fn a_valid_reload_replaces_the_active_configuration_without_a_restart() {
    let dir = TempDir::new("valid");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));

    let first = manager.reload();
    assert!(first.applied(), "the initial document must load: {first:?}");
    assert!(manager.config().profile("notes").is_some());

    fs::write(&path, profile("journal")).expect("the replacement config is writable");
    let second = manager.reload();
    assert!(
        second.applied(),
        "the replacement document must load: {second:?}"
    );
    assert!(manager.config().profile("journal").is_some());
    assert!(
        manager.config().profile("notes").is_none(),
        "the live value was not replaced"
    );
    assert_eq!(
        manager.file().map(ConfigFile::path),
        Some(path.as_path()),
        "the manager still reloads the same file"
    );
}

#[test]
fn a_reload_replaces_the_key_and_visual_tables_beside_the_profiles() {
    // One assignment replaces the whole `Config`, so a reload that dropped a
    // table — or applied one table from each document — would show up here.
    let dir = TempDir::new("tables");
    let path = dir.config();
    fs::write(
        &path,
        "[keys]\nprefix = \"C-a\"\n\n[visual]\ntheme = \"terminal\"\n",
    )
    .expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());
    assert_eq!(manager.config().keys().prefix(), Key::ctrl('a'));
    assert_eq!(manager.config().visual().theme, ThemeChoice::Terminal);

    // A document naming none of them is not a partial update: every table the
    // replacement leaves out returns to its documented default.
    fs::write(&path, profile("notes")).expect("the replacement config is writable");
    assert!(manager.reload().applied());
    assert!(manager.config().profile("notes").is_some());
    assert_eq!(manager.config().keys().prefix(), DEFAULT_PREFIX);
    assert_eq!(manager.config().visual().theme, ThemeChoice::default());
}

#[test]
fn a_detached_configuration_has_no_file_and_is_never_reset_by_a_reload() {
    // A caller-supplied configuration is not a configuration whose file went
    // missing: re-reading nothing must keep it rather than fall back to the
    // built-ins, and must publish no revision to anyone.
    let loaded = cloo_core::config::parse(&profile("notes")).expect("the document is valid");
    let mut manager = ConfigManager::detached(loaded.config);
    assert!(manager.file().is_none());

    let reload = manager.reload();
    assert!(
        matches!(reload, Reload::Detached),
        "a manager with no file has nothing to re-read: {reload:?}"
    );
    assert!(!reload.applied());
    assert!(reload.diagnostics().is_empty());
    assert!(manager.config().profile("notes").is_some());
}

#[test]
fn a_rejected_reload_renders_one_diagnostic_naming_its_file() {
    let dir = TempDir::new("diagnostic");
    let path = dir.config();
    fs::write(&path, "[[profile]\nid = \"broken\"\n").expect("the invalid config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));

    let diagnostics = manager.reload().diagnostics();
    assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
    assert!(
        diagnostics[0].contains(&path.display().to_string()),
        "a diagnostic must name the file the user has to fix: {diagnostics:?}"
    );
}

#[test]
fn an_invalid_reload_keeps_the_last_valid_configuration() {
    let dir = TempDir::new("invalid");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());
    let before = manager.config().clone();

    fs::write(&path, "[[profile]\nid = \"broken\"\n").expect("the invalid replacement is writable");
    let reload = manager.reload();
    assert!(
        matches!(reload, Reload::Rejected { .. }),
        "an invalid document must be refused: {reload:?}"
    );
    assert_eq!(
        manager.config(),
        &before,
        "a failed reload changed the live value"
    );
}

#[test]
fn removing_the_file_is_a_valid_reset_to_the_built_ins() {
    let dir = TempDir::new("missing");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());
    assert!(manager.config().profile("notes").is_some());

    fs::remove_file(&path).expect("the test config exists");
    assert!(manager.reload().applied());
    assert!(manager.config().profile("notes").is_none());
    assert!(manager.config().profile("generic").is_some());
    assert!(dir.path().exists(), "only the fixture file was removed");
}

#[test]
fn an_invalid_profile_warns_but_applies_its_valid_neighbours() {
    let dir = TempDir::new("warning");
    let path = dir.config();
    fs::write(
        &path,
        "[[profile]]\nid = \"notes\"\n\n[[profile]]\nid = \"Bad Id\"\n",
    )
    .expect("the mixed config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));

    let reload = manager.reload();
    let Reload::Applied { warnings } = reload else {
        panic!("a semantically mixed document is still valid")
    };
    assert_eq!(warnings.len(), 1);
    assert!(manager.config().profile("notes").is_some());
    assert!(manager.config().profile("Bad Id").is_none());
}

#[tokio::test]
async fn a_sighup_reloads_the_same_live_manager() {
    let _serialized = SIGHUP_TESTS.lock().await;
    let dir = TempDir::new("sighup");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());
    fs::write(&path, profile("journal")).expect("the replacement config is writable");

    let mut watch = ReloadWatch::new().expect("SIGHUP is available on this Unix test host");
    raise_sighup();

    let reload = tokio::time::timeout(
        Duration::from_secs(1),
        manager.reload_when_signalled(&mut watch),
    )
    .await
    .expect("the SIGHUP watcher must receive the signal");
    assert!(
        reload.applied(),
        "the valid SIGHUP reload was refused: {reload:?}"
    );
    assert!(manager.config().profile("journal").is_some());
}

#[tokio::test]
async fn a_daemon_sighup_applies_the_new_document_and_publishes_one_revision() {
    // The whole loop for M9-04: the daemon re-reads its own file, replaces the
    // table a typed launch request resolves against, and tells attached
    // clients the revision — which is all it tells them, because every
    // preference in that document is read by each client for itself.
    let _serialized = SIGHUP_TESTS.lock().await;
    let dir = TempDir::new("daemon-valid");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());

    let socket = dir.socket();
    let daemon = spawn_daemon(&socket, manager, |_| {});
    let mut client = attach(&socket).await;

    // A document naming a profile the running daemon has never heard of.
    fs::write(&path, profile("journal")).expect("the replacement config is writable");
    raise_sighup();

    let revision = until(&mut client, |message| match message {
        ServerMessage::ConfigReloaded { revision } => Some(*revision),
        _ => None,
    })
    .await;
    assert_eq!(revision, 1, "the first applied reload is revision 1");

    // Nothing about a reload stops the daemon serving: the child still answers
    // through the session actor and its output still reaches this client.
    client
        .send(&ClientMessage::Input(b"ping\n".to_vec()))
        .await
        .expect("the input must reach the daemon");
    until(&mut client, |message| match message {
        ServerMessage::Damage { rows, .. } => rows
            .iter()
            .any(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .contains("<ping>")
            })
            .then_some(()),
        _ => None,
    })
    .await;

    // The replacement table is the live one: an identifier only the new
    // document names resolves to a pane.
    client
        .send(&ClientMessage::Command(Action::LaunchProfile(
            "journal".to_owned(),
        )))
        .await
        .expect("the launch request must reach the daemon");
    let panes = until_launched(&mut client, "journal").await;
    assert_eq!(
        panes.len(),
        2,
        "the launched pane joined the original one; got {panes:?}"
    );

    daemon.abort();
}

#[tokio::test]
async fn a_rejected_daemon_reload_publishes_nothing_and_keeps_the_previous_table() {
    let _serialized = SIGHUP_TESTS.lock().await;
    let dir = TempDir::new("daemon-invalid");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());

    let reported = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reported);
    let socket = dir.socket();
    let daemon = spawn_daemon(&socket, manager, move |diagnostic| {
        sink.lock()
            .expect("the diagnostic sink is uncontended")
            .push(diagnostic.to_owned());
    });
    let mut client = attach(&socket).await;

    fs::write(&path, "[[profile]\nid = \"broken\"\n").expect("the invalid config is writable");
    raise_sighup();

    // The refused document changed nothing: the identifier the *previous* one
    // named still launches, and no revision may arrive while it does.
    client
        .send(&ClientMessage::Command(Action::LaunchProfile(
            "notes".to_owned(),
        )))
        .await
        .expect("the launch request must reach the daemon");
    let panes = until_launched(&mut client, "notes").await;
    assert_eq!(panes.len(), 2, "got {panes:?}");

    // A valid document now, so the daemon publishes its first revision. That it
    // is revision 1 is the proof the refused reload consumed none.
    fs::write(&path, profile("journal")).expect("the replacement config is writable");
    raise_sighup();
    let revision = until(&mut client, |message| match message {
        ServerMessage::ConfigReloaded { revision } => Some(*revision),
        _ => None,
    })
    .await;
    assert_eq!(
        revision, 1,
        "a refused reload must not consume a revision number"
    );

    // The refusal was reported rather than swallowed: a user who has to fix a
    // file is told which one and why.
    let diagnostics = reported
        .lock()
        .expect("the diagnostic sink is uncontended")
        .clone();
    assert_eq!(diagnostics.len(), 1, "got {diagnostics:?}");
    assert!(
        diagnostics[0].contains(&path.display().to_string()),
        "the diagnostic must name the refused file: {diagnostics:?}"
    );

    daemon.abort();
}
