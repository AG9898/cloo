//! Integration coverage for server-owned configuration reloads.
//!
//! These tests read and replace real files, so they live under `tests/` rather
//! than beside the pure path resolver. Each fixture owns a unique directory;
//! no process environment is changed and no test can observe another's file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use std::time::Duration;

use cloo_server::config::{ConfigFile, ConfigManager, Reload, ReloadWatch};

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
    assert_eq!(manager.file().path(), path);
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
    let dir = TempDir::new("sighup");
    let path = dir.config();
    fs::write(&path, profile("notes")).expect("the first config is writable");
    let mut manager = ConfigManager::new(ConfigFile::new(&path));
    assert!(manager.reload().applied());
    fs::write(&path, profile("journal")).expect("the replacement config is writable");

    let mut watch = ReloadWatch::new().expect("SIGHUP is available on this Unix test host");
    // SAFETY: this test process installed a Tokio SIGHUP listener immediately
    // above; delivering the signal requests one reload instead of terminating
    // the process, and no shared test fixture relies on SIGHUP's default action.
    let result = unsafe { libc::raise(libc::SIGHUP) };
    assert_eq!(
        result,
        0,
        "could not deliver SIGHUP: {}",
        std::io::Error::last_os_error()
    );

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
