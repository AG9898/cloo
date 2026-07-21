//! Integration coverage for the daemon socket lifecycle.
//!
//! These tests touch the real filesystem, which is why they live in `tests/`
//! and not in a `#[cfg(test)]` module — path resolution is pure and is covered
//! by unit tests in `src/socket.rs` instead. Every test binds inside its own
//! temporary directory under `$TMPDIR`, so nothing here depends on
//! `XDG_RUNTIME_DIR`, no two tests collide, and no process environment is
//! mutated (which would race, since Rust runs tests in threads).

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{fs, io};

use cloo_server::socket::{Listener, SocketError, lock_path_for};

/// A unique temporary directory for one test, removed by [`TempDir::drop`].
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cloo-socket-test-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a temp dir must be creatable");
        Self(path)
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

#[test]
fn binding_creates_the_socket_and_its_directory() {
    let dir = TempDir::new("create");
    let socket = dir.socket();

    let listener = Listener::bind(&socket).expect("a fresh path must bind");

    assert_eq!(listener.path(), socket);
    assert!(
        fs::symlink_metadata(&socket)
            .expect("the socket must exist")
            .file_type()
            .is_socket()
    );
    assert!(UnixStream::connect(&socket).is_ok(), "nothing is listening");
}

#[test]
fn the_socket_directory_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("perms");
    let socket = dir.socket();
    let _listener = Listener::bind(&socket).expect("a fresh path must bind");

    let mode = fs::metadata(socket.parent().expect("the socket has a parent"))
        .expect("the directory must exist")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "socket directory is too permissive");
}

#[test]
fn a_second_daemon_is_refused_while_the_first_holds_the_socket() {
    let dir = TempDir::new("exclusive");
    let socket = dir.socket();

    let first = Listener::bind(&socket).expect("the first daemon must bind");

    let err = Listener::bind(&socket).expect_err("a second daemon must be refused");
    assert!(
        matches!(err, SocketError::AlreadyRunning(ref p) if *p == socket),
        "expected AlreadyRunning, got {err}"
    );

    // The refusal must not have disturbed the running daemon.
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "the first daemon's socket was damaged by the refused bind"
    );
    drop(first);
}

#[test]
fn dropping_a_listener_unlinks_its_socket_and_frees_the_name() {
    let dir = TempDir::new("unlink");
    let socket = dir.socket();

    drop(Listener::bind(&socket).expect("the first daemon must bind"));

    assert!(
        !socket.exists(),
        "a clean exit must not leave a socket behind"
    );
    // The lock file stays: unlinking it would race a daemon that has already
    // opened it and is about to lock it.
    assert!(lock_path_for(&socket).exists(), "the lock file was removed");

    let second = Listener::bind(&socket).expect("the name must be reusable");
    assert!(UnixStream::connect(&socket).is_ok());
    drop(second);
}

#[test]
fn a_stale_socket_from_a_dead_daemon_is_replaced() {
    let dir = TempDir::new("stale");
    let socket = dir.socket();

    // Simulate a daemon killed with SIGKILL: the socket file survives, but
    // nothing is listening on it and no lock is held.
    fs::create_dir_all(socket.parent().expect("the socket has a parent"))
        .expect("the run dir must be creatable");
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("a bare socket must bind"));
    fs::write(lock_path_for(&socket), b"").expect("a leftover lock file must be writable");
    assert!(socket.exists(), "the fixture did not leave a socket");
    assert_eq!(
        UnixStream::connect(&socket)
            .expect_err("nothing must be listening")
            .kind(),
        io::ErrorKind::ConnectionRefused
    );

    let listener = Listener::bind(&socket).expect("a stale socket must be cleared");

    assert!(
        UnixStream::connect(&socket).is_ok(),
        "the replacement socket is not accepting"
    );
    drop(listener);
}

#[test]
fn cleanup_never_removes_a_file_that_is_not_a_socket() {
    let dir = TempDir::new("notasocket");
    let socket = dir.socket();
    fs::create_dir_all(socket.parent().expect("the socket has a parent"))
        .expect("the run dir must be creatable");
    fs::write(&socket, b"precious").expect("the decoy must be writable");

    let err = Listener::bind(&socket).expect_err("a regular file must not be bound over");
    assert!(
        matches!(err, SocketError::NotASocket(ref p) if *p == socket),
        "expected NotASocket, got {err}"
    );
    assert_eq!(
        fs::read(&socket).expect("the file must still be there"),
        b"precious",
        "cloo deleted a file it did not create"
    );
}

#[test]
fn cleanup_never_follows_a_symlink_out_of_the_socket_directory() {
    let dir = TempDir::new("symlink");
    let socket = dir.socket();
    fs::create_dir_all(socket.parent().expect("the socket has a parent"))
        .expect("the run dir must be creatable");

    // A symlink pointing at a live socket elsewhere. Following it would report
    // "socket" and the unlink would then remove the link — and a naive
    // implementation that followed further could take out the target.
    let elsewhere = dir.path().join("other.sock");
    let other = std::os::unix::net::UnixListener::bind(&elsewhere).expect("a target must bind");
    std::os::unix::fs::symlink(&elsewhere, &socket).expect("a symlink must be creatable");

    let err = Listener::bind(&socket).expect_err("a symlink must not be bound over");
    assert!(
        matches!(err, SocketError::NotASocket(_)),
        "expected NotASocket, got {err}"
    );
    assert!(elsewhere.exists(), "the symlink target was removed");
    drop(other);
}

#[test]
fn dropping_a_listener_leaves_a_successor_socket_alone() {
    let dir = TempDir::new("successor");
    let socket = dir.socket();

    let first = Listener::bind(&socket).expect("the first daemon must bind");
    // Stand in for a successor that bound the same path: the identity check in
    // `Drop` must see a different inode and leave it alone. Unlinking first is
    // what a real successor's stale cleanup would do.
    fs::remove_file(&socket).expect("the socket must be removable");
    let successor =
        std::os::unix::net::UnixListener::bind(&socket).expect("the successor must bind");

    drop(first);

    assert!(
        socket.exists(),
        "a departing daemon removed a successor's socket"
    );
    assert!(UnixStream::connect(&socket).is_ok());
    drop(successor);
}

#[test]
fn a_path_with_no_parent_directory_is_refused() {
    let err = Listener::bind(Path::new("session.sock"))
        .expect_err("a bare relative name has no directory to create");
    assert!(
        matches!(err, SocketError::NoParentDir(_)),
        "expected NoParentDir, got {err}"
    );
}
