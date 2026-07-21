//! Integration coverage for attach, hello, and detach.
//!
//! These drive a real daemon over a real Unix socket with a real child on a
//! real pseudoterminal. They live in the **binary** crate, not in
//! `cloo-server`, because they need both halves of the wire and `cloo-server`
//! may never name `cloo-client` — not even as a dev-dependency, which is still
//! the forbidden sideways edge. `crates/cloo` is the composition root and
//! already depends on both.
//!
//! Each binds inside its own temporary directory under `$TMPDIR`, so nothing
//! depends on `XDG_RUNTIME_DIR` and no two tests collide.
//!
//! Synchronization is by reading the wire until the expected frame arrives,
//! bounded by a timeout — never by sleeping for a fixed interval and hoping.
//! The scripted children block on `read` rather than exiting, because the
//! property under test is that they are still there afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::{fs, io};

use cloo_client::attach::{AttachError, Attached, attach};
use cloo_proto::{
    ClientMessage, FrameStream, MouseButton, MouseEvent, MouseKind, MouseMods, MouseTracking,
    PROTOCOL_VERSION, PaneId, PaneModes, Point, RowUpdate, ServerMessage, Size, TermCaps,
};
use cloo_server::daemon::Daemon;
use cloo_server::pty::PtyConfig;
use cloo_server::socket::Listener;
use tokio::net::UnixStream;

/// How long any single wire expectation may take before the test fails.
///
/// Generous, because a loaded CI box is slow and a flaky test is worse than a
/// slow one. Nothing here is expected to come anywhere near it.
const PATIENCE: Duration = Duration::from_secs(10);

/// A unique temporary directory for one test, removed by [`TempDir::drop`].
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cloo-attach-test-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a temp dir must be creatable");
        Self(path)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run").join("session.sock")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A config running `script` under `sh` at 80x24.
fn scripted(script: &str) -> PtyConfig {
    PtyConfig::new("sh")
        .arg("-c")
        .arg(script)
        .env("TERM", "xterm-256color")
        .wire_size(Size::new(80, 24))
        .expect("80x24 is a valid size")
}

/// Binds a daemon on `socket` and runs it in the background.
///
/// Returns the child's pid and the join handle, so a test can prove the child
/// outlived a detach and then wait for the daemon to finish.
fn spawn_daemon(
    socket: &Path,
    script: &str,
) -> (
    u32,
    tokio::task::JoinHandle<Result<std::process::ExitStatus, cloo_server::DaemonError>>,
) {
    let listener = Listener::bind(socket).expect("a fresh socket path must bind");
    let mut daemon = Daemon::new(listener, &scripted(script)).expect("the daemon must start");
    let pid = daemon.child_id();
    let handle = tokio::spawn(async move { daemon.run().await });
    (pid, handle)
}

/// Whether `pid` still exists.
fn alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs an existence and permission check
    // only; it delivers nothing and touches no memory.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The visible text of a row, without its trailing blanks.
fn row_text(row: &RowUpdate) -> String {
    row.cells
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Attaches to `socket`, failing the test with the daemon's own reason.
async fn client(socket: &Path) -> Attached<UnixStream> {
    tokio::time::timeout(
        PATIENCE,
        attach(socket, Size::new(80, 24), TermCaps::default(), None),
    )
    .await
    .expect("the attach must not hang")
    .expect("the attach must succeed")
}

/// Reads frames until a damage update puts `want` on some row of the pane.
///
/// Any row, not row 0: the pty echoes the newline that unblocks a scripted
/// `read`, so where a line lands depends on how much has already been typed.
async fn await_text(attached: &mut Attached<UnixStream>, want: &str) {
    let found = tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Damage { rows, .. }) => {
                    if rows.iter().any(|row| row_text(row) == want) {
                        return true;
                    }
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {want:?} in the pane"));
    assert!(found, "the server closed before sending {want:?}");
}

/// Like [`await_text`], but returns how many bounded damage frames arrived
/// after the caller began waiting. A burst may span a few frame ticks on a
/// loaded machine; it must never become one frame per PTY read.
async fn await_text_counting_damage(attached: &mut Attached<UnixStream>, want: &str) -> usize {
    tokio::time::timeout(PATIENCE, async {
        let mut damage_frames = 0;
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Damage { rows, .. }) => {
                    damage_frames += 1;
                    if rows.iter().any(|row| row_text(row) == want) {
                        return damage_frames;
                    }
                }
                Some(_) => {}
                None => panic!("the server closed before sending {want:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {want:?} in the pane"))
}

#[tokio::test]
async fn attaching_delivers_a_hello_and_a_session_snapshot() {
    let dir = TempDir::new("snapshot");
    let socket = dir.socket();
    let (pid, daemon) = spawn_daemon(&socket, "printf 'hello\\n'; read _; exit 0");

    let mut attached = client(&socket).await;
    assert_eq!(attached.size(), Size::new(80, 24));
    assert_eq!(attached.tabs().len(), 1, "a session always has one tab");
    assert!(attached.tabs()[0].active);

    await_text(&mut attached, "hello").await;

    // Unblock the child so the daemon can finish.
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    let status = tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
    assert!(status.success(), "the child exited with {status}");
    assert!(!alive(pid), "the child outlived its daemon");
}

#[tokio::test]
async fn detaching_leaves_the_child_running_and_its_state_intact() {
    let dir = TempDir::new("detach");
    let socket = dir.socket();
    let (pid, daemon) = spawn_daemon(&socket, "printf 'hello\\n'; read _; printf 'bye\\n'");

    let mut first = client(&socket).await;
    await_text(&mut first, "hello").await;
    first.detach().await.expect("detach must succeed");

    assert!(alive(pid), "detaching killed the child");

    // The session is still there, and still knows what the child wrote before
    // anyone was watching.
    let mut second = client(&socket).await;
    await_text(&mut second, "hello").await;

    second
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    await_text(&mut second, "bye").await;

    let status = tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
    assert!(status.success(), "the child exited with {status}");
    assert!(!alive(pid), "the child outlived its daemon");
}

#[tokio::test]
async fn a_client_that_vanishes_does_not_take_the_session_with_it() {
    let dir = TempDir::new("vanish");
    let socket = dir.socket();
    let (pid, daemon) = spawn_daemon(&socket, "read _; printf 'still here\\n'");

    let attached = client(&socket).await;
    // No detach, no goodbye — the client process died.
    drop(attached);

    let mut second = client(&socket).await;
    assert!(alive(pid), "a dropped connection killed the child");
    second
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    await_text(&mut second, "still here").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn two_clients_receive_one_coalesced_damage_stream() {
    let dir = TempDir::new("fanout");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, "printf 'ready\\n'; read _; printf 'shared\\n'");

    let mut first = client(&socket).await;
    await_text(&mut first, "ready").await;
    let mut second = client(&socket).await;
    await_text(&mut second, "ready").await;

    first
        .send_input(b"\n".to_vec())
        .await
        .expect("input from one client must reach the session");
    await_text(&mut first, "shared").await;
    await_text(&mut second, "shared").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn burst_output_is_frame_bounded_and_a_lagging_client_recovers() {
    let dir = TempDir::new("damage-burst");
    let socket = dir.socket();
    // `yes | head` produces far more than one PTY read without relying on
    // sleeps. The first client deliberately does not read while the burst is
    // in flight; the second must still see the final marker promptly.
    let (_pid, daemon) = spawn_daemon(
        &socket,
        "printf 'ready\\n'; read _; yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | head -n 80000; printf 'burst-complete\\n'; read _; exit 0",
    );

    let mut lagging = client(&socket).await;
    let mut active = client(&socket).await;
    await_text(&mut active, "ready").await;

    active
        .send_input(b"\n".to_vec())
        .await
        .expect("input must start the burst");
    let frames = await_text_counting_damage(&mut active, "burst-complete").await;
    assert!(
        frames <= 12,
        "a burst must be coalesced to frame-bounded updates, got {frames}"
    );

    // The lagging socket task dropped its bounded backlog and requested a new
    // snapshot. If it had been allowed to stall the daemon, the active client
    // above would not have reached the marker; if it did not resync here, this
    // client would never converge on the same final grid.
    await_text(&mut lagging, "burst-complete").await;

    active
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the child exit");
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// Reads frames until a damage update carries a row exactly `cols` cells wide.
///
/// This is the *grid* half of a resize: the emulator reflowed, so the rows the
/// server puts on the wire are the new width.
async fn await_row_width(attached: &mut Attached<UnixStream>, cols: usize) {
    let found = tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Damage { rows, .. }) => {
                    if rows.iter().any(|row| row.cells.len() == cols) {
                        return true;
                    }
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no row ever came back {cols} cells wide"));
    assert!(found, "the server closed before resizing the grid");
}

#[tokio::test]
async fn a_resize_reaches_both_the_grid_and_the_child() {
    let dir = TempDir::new("resize");
    let socket = dir.socket();
    // `stty size` asks the *pty* what shape it is, which is the only thing that
    // can answer for `TIOCSWINSZ` having been issued. It runs after the read so
    // the test controls when the answer is produced.
    let (_pid, daemon) = spawn_daemon(&socket, "printf 'ready\\n'; read _; stty size");

    let mut attached = client(&socket).await;
    assert_eq!(attached.size(), Size::new(80, 24));
    await_text(&mut attached, "ready").await;

    attached
        .send_resize(Size::new(100, 40))
        .await
        .expect("the resize must reach the daemon");

    // Half one: the session task reflowed the grid, so rows are 100 wide.
    await_row_width(&mut attached, 100).await;

    // Half two: the child's own view of its terminal changed, which only
    // `TIOCSWINSZ` on the pty master can have done. `stty size` prints
    // "rows cols".
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    await_text(&mut attached, "40 100").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_degenerate_resize_leaves_the_session_alone() {
    let dir = TempDir::new("degenerate");
    let socket = dir.socket();
    let (pid, daemon) = spawn_daemon(&socket, "printf 'ready\\n'; read _; stty size");

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;

    // A terminal reporting zero rows mid-drag must not become a zero-height pty
    // and a correspondingly confused shell.
    attached
        .send_resize(Size::new(100, 0))
        .await
        .expect("the resize must reach the daemon");
    assert!(alive(pid), "a degenerate resize killed the child");

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    await_text(&mut attached, "24 80").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// Reads frames until the server reports pane modes satisfying `want`.
async fn await_modes(attached: &mut Attached<UnixStream>, want: fn(PaneModes) -> bool) {
    let found = tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Modes { modes, .. }) if want(modes) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .expect("the expected modes never arrived");
    assert!(found, "the server closed before reporting the modes");
}

/// A child that echoes what it is sent, with escape bytes stripped so the result
/// is readable on the grid.
///
/// `-echo` keeps the pty's own echo out of the rows and `-icanon` is what lets a
/// report with no newline in it be read at all; `tr` is what makes an escape
/// sequence assertable as text.
fn echoing(enable: &str, bytes: usize) -> String {
    format!(
        "stty -echo -icanon; printf '{enable}'; printf 'ready\\n'; \
         head -c {bytes} | tr -d '\\033'"
    )
}

#[tokio::test]
async fn a_paste_is_bracketed_exactly_when_the_child_asked_for_it() {
    let dir = TempDir::new("paste-bracketed");
    let socket = dir.socket();
    // `\x1b[200~hello\x1b[201~` is 17 bytes; the child prints it back without
    // the escape byte, so the brackets themselves are what the test sees.
    let (_pid, daemon) = spawn_daemon(&socket, &echoing("\\033[?2004h", 17));

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;
    await_modes(&mut attached, |modes| modes.bracketed_paste).await;

    attached
        .send_paste(b"hello".to_vec())
        .await
        .expect("the paste must reach the daemon");
    await_text(&mut attached, "[200~hello[201~").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_paste_to_a_child_that_did_not_ask_arrives_as_typed_input() {
    let dir = TempDir::new("paste-plain");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, &echoing("", 5));

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;

    attached
        .send_paste(b"hello".to_vec())
        .await
        .expect("the paste must reach the daemon");
    await_text(&mut attached, "hello").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn focus_reaches_a_child_that_asked_for_it() {
    let dir = TempDir::new("focus");
    let socket = dir.socket();
    // `\x1b[I` is three bytes, two of them printable.
    let (_pid, daemon) = spawn_daemon(&socket, &echoing("\\033[?1004h", 3));

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;
    await_modes(&mut attached, |modes| modes.focus_events).await;

    attached
        .send_focus(true)
        .await
        .expect("the focus report must reach the daemon");
    await_text(&mut attached, "[I").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_child_that_asked_for_neither_focus_nor_the_mouse_hears_neither() {
    let dir = TempDir::new("silent");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, &echoing("", 4));

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;

    // Neither of these may put a single byte into the child's input. The typed
    // "done" that follows is what proves it: if either had been forwarded, the
    // four bytes the child reads would start with a report instead.
    attached.send_focus(true).await.expect("focus must send");
    attached
        .send_mouse(MouseEvent {
            pane: PaneId::new(1),
            at: Point::new(10, 5),
            kind: MouseKind::Press(MouseButton::Left),
            mods: MouseMods::NONE,
        })
        .await
        .expect("the mouse event must send");
    attached
        .send_input(b"done".to_vec())
        .await
        .expect("input must reach the child");

    await_text(&mut attached, "done").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_mouse_event_reaches_a_tracking_child_in_the_sgr_encoding() {
    let dir = TempDir::new("mouse");
    let socket = dir.socket();
    // `\x1b[<0;11;6M` is ten bytes: the SGR report for a left press on the cell
    // at column 10, row 5, one-based on the wire out.
    let (_pid, daemon) = spawn_daemon(&socket, &echoing("\\033[?1000h\\033[?1006h", 10));

    let mut attached = client(&socket).await;
    await_text(&mut attached, "ready").await;
    await_modes(&mut attached, |modes| {
        modes.mouse != MouseTracking::Off && modes.sgr_mouse
    })
    .await;

    attached
        .send_mouse(MouseEvent {
            pane: PaneId::new(1),
            at: Point::new(10, 5),
            kind: MouseKind::Press(MouseButton::Left),
            mods: MouseMods::NONE,
        })
        .await
        .expect("the mouse event must reach the daemon");
    await_text(&mut attached, "[<0;11;6M").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_stale_client_is_refused_with_an_actionable_reason() {
    let dir = TempDir::new("mismatch");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, "read _; exit 0");

    // A client built against a different protocol version, hand-rolled because
    // `cloo-client` can only ever send the version it was built with.
    let stream = UnixStream::connect(&socket)
        .await
        .expect("the daemon must be listening");
    let mut conn = FrameStream::new(stream);
    conn.send(&ClientMessage::Attach {
        protocol_version: PROTOCOL_VERSION.wrapping_add(1),
        size: Size::new(80, 24),
        term_caps: TermCaps::default(),
        session: None,
    })
    .await
    .expect("the attach must send");

    let reply = tokio::time::timeout(PATIENCE, conn.recv::<ServerMessage>())
        .await
        .expect("a refusal must not hang")
        .expect("the refusal must decode");
    let Some(ServerMessage::Refused { reason }) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert!(reason.contains("version mismatch"), "got: {reason}");
    assert!(reason.contains("reattach"), "got: {reason}");

    // The refusal cost the session nothing: a matching client still attaches.
    let mut good = client(&socket).await;
    good.send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn attaching_where_no_daemon_is_listening_says_so() {
    let dir = TempDir::new("nodaemon");
    let socket = dir.socket();

    let err = attach(&socket, Size::new(80, 24), TermCaps::default(), None)
        .await
        .expect_err("there is nothing to attach to");

    assert!(matches!(err, AttachError::NoDaemon(_)), "got {err}");
    assert!(
        err.to_string().contains("no cloo daemon is listening"),
        "got: {err}"
    );

    // And a socket file with nothing behind it reads the same way, because to a
    // user it is the same situation.
    fs::create_dir_all(socket.parent().expect("the socket has a parent"))
        .expect("the run dir must be creatable");
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("a bare socket must bind"));
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&socket)
            .expect_err("nothing must be listening")
            .kind(),
        io::ErrorKind::ConnectionRefused
    );
    let err = attach(&socket, Size::new(80, 24), TermCaps::default(), None)
        .await
        .expect_err("a stale socket is not a daemon");
    assert!(matches!(err, AttachError::NoDaemon(_)), "got {err}");
}
