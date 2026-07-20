//! Integration coverage for the single-pane PTY reactor.
//!
//! Every test here drives a scripted `sh -c` child rather than an interactive
//! shell, so each one terminates on its own and the assertions are
//! deterministic. Synchronization is by awaiting EOF — never by sleeping.
//!
//! These tests spawn real processes and real pseudoterminals, which is why they
//! live in `tests/` and not in a `#[cfg(test)]` module. They create no sockets
//! and leave nothing behind: the child is reaped by `wait`, and the master
//! descriptor is closed when the reactor drops.

use cloo_server::pty::{Pty, PtyConfig, PtyError, PtyReactor, Pump};
use cloo_term::TermSize;

/// A config running `script` under `sh`, at `cols` x `rows`.
fn scripted(script: &str, cols: u16, rows: u16) -> PtyConfig {
    let size = TermSize::new(cols, rows).expect("test sizes are non-zero");
    PtyConfig::new("sh")
        .arg("-c")
        .arg(script)
        .env("TERM", "xterm-256color")
        .size(size)
}

#[tokio::test]
async fn scripted_shell_output_reaches_the_grid() {
    let mut reactor = PtyReactor::spawn(&scripted("echo hello", 80, 24))
        .expect("a pty and an sh child must be available");

    let total = reactor.run_to_eof().await.expect("reads must succeed");
    let status = reactor.wait().expect("the child must be reapable");

    assert!(total > 0, "the child produced no output at all");
    assert!(status.success(), "sh exited with {status}");
    assert_eq!(reactor.emulator().row_text(0).as_deref(), Some("hello"));
    assert_eq!(reactor.emulator().row_text(1).as_deref(), Some(""));
}

#[tokio::test]
async fn output_split_across_reads_still_lands_correctly() {
    // Three separate writes with an escape sequence straddling them; the
    // reactor has no control over where a read boundary falls, so the grid must
    // come out the same either way.
    let script = r"printf 'a\033[1'; printf 'mB\033[0m'; printf 'c\n'";
    let mut reactor = PtyReactor::spawn(&scripted(script, 80, 24)).expect("pty spawn must succeed");

    reactor.run_to_eof().await.expect("reads must succeed");
    reactor.wait().expect("the child must be reapable");

    assert_eq!(reactor.emulator().row_text(0).as_deref(), Some("aBc"));
}

#[tokio::test]
async fn the_child_sees_the_configured_winsize() {
    // `stty size` only works with a controlling terminal, so this asserts both
    // that openpty carried the winsize through and that TIOCSCTTY worked.
    let mut reactor =
        PtyReactor::spawn(&scripted("stty size", 100, 30)).expect("pty spawn must succeed");

    reactor.run_to_eof().await.expect("reads must succeed");
    reactor.wait().expect("the child must be reapable");

    assert_eq!(reactor.emulator().row_text(0).as_deref(), Some("30 100"));
}

#[tokio::test]
async fn input_written_to_the_pty_reaches_the_child() {
    let mut reactor = PtyReactor::spawn(&scripted("read line; echo \"got:$line\"", 80, 24))
        .expect("pty spawn must succeed");

    reactor
        .write_all(b"ping\n")
        .expect("writing to the pty must succeed");
    reactor.run_to_eof().await.expect("reads must succeed");
    reactor.wait().expect("the child must be reapable");

    // The pty echoes input, so the typed line lands on row 0 and the reply on
    // row 1.
    assert_eq!(reactor.emulator().row_text(0).as_deref(), Some("ping"));
    assert_eq!(reactor.emulator().row_text(1).as_deref(), Some("got:ping"));
}

#[tokio::test]
async fn resize_updates_the_grid_and_the_child() {
    // The child reports its size only after reading a line, so the resize is
    // guaranteed to have happened before `stty` runs.
    let mut reactor =
        PtyReactor::spawn(&scripted("read _; stty size", 80, 24)).expect("pty spawn must succeed");

    let resized = TermSize::new(132, 43).expect("132x43 is a valid size");
    reactor.resize(resized).expect("resize must succeed");
    assert_eq!(reactor.emulator().size(), resized);

    reactor.write_all(b"\n").expect("writing must succeed");
    reactor.run_to_eof().await.expect("reads must succeed");
    reactor.wait().expect("the child must be reapable");

    assert_eq!(reactor.emulator().row_text(1).as_deref(), Some("43 132"));
}

#[tokio::test]
async fn eof_is_reported_once_the_child_is_gone_and_stays_reported() {
    let mut reactor =
        PtyReactor::spawn(&scripted("exit 3", 80, 24)).expect("pty spawn must succeed");

    reactor.run_to_eof().await.expect("reads must succeed");
    assert_eq!(
        reactor
            .pump()
            .await
            .expect("a post-eof pump must not error"),
        Pump::Eof
    );

    let status = reactor.wait().expect("the child must be reapable");
    assert_eq!(status.code(), Some(3));
}

#[tokio::test]
async fn a_missing_program_fails_to_spawn_with_the_program_named() {
    let config = PtyConfig::new("cloo-no-such-program-exists");
    // `PtyReactor` is not `Debug`, so unwrap the error by hand rather than with
    // `expect_err`.
    let Err(err) = PtyReactor::spawn(&config) else {
        panic!("a nonexistent program must not spawn");
    };

    assert!(matches!(err, PtyError::Spawn { .. }));
    assert!(
        err.to_string().contains("cloo-no-such-program-exists"),
        "the error must name the program, got: {err}"
    );
}

#[test]
fn dropping_a_pty_reaps_the_child() {
    // No runtime here on purpose: `Pty` is usable without one, and this asserts
    // the restoration path rather than the read path. A child that survived its
    // `Pty` would leave a zombie for the whole test binary.
    let config = PtyConfig::new("sh").arg("-c").arg("cat");
    let pty = Pty::spawn(&config).expect("pty spawn must succeed");
    let pid = pty.child_id();
    drop(pty);

    // The pid has been reaped, so signalling it must fail with ESRCH. If the
    // child were merely a zombie, signal 0 would still succeed.
    // SAFETY: `kill` with signal 0 performs an existence and permission check
    // only; it delivers nothing and touches no memory.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) };
    assert_eq!(alive, -1, "child {pid} outlived its Pty");
}
