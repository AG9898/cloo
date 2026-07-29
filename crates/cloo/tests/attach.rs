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

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, io};

use cloo_client::attach::{AttachError, Attached, attach};
use cloo_client::copy_mode::{apply_copy, highlight_spans};
use cloo_client::effects::{EffectPolicy, apply_effect};
use cloo_client::input::{
    ChromeMouse, MouseReport, MouseRoute, ScreenLayout, WHEEL_LINES, route_mouse,
};
use cloo_client::renderer::Grid;
use cloo_client::theme::Theme;
use cloo_core::pane::{PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{AdapterId, Profile, ProfileCommand};
use cloo_proto::{
    Action, AdapterMessage, AdapterRejection, AdapterReply, AdapterState, AttentionSource,
    AttentionState, ClientMessage, ClipboardTarget, CopyMotion, FrameStream, MouseButton,
    MouseEvent, MouseKind, MouseMods, MouseTracking, OuterTerminalEffect, PROTOCOL_VERSION, PaneId,
    PaneModes, Point, RowUpdate, SearchDirection, ServerMessage, Size, TermCaps, WorkspaceStatus,
};
use cloo_server::daemon::Daemon;
use cloo_server::launch::Launch;
use cloo_server::pty::PtyConfig;
use cloo_server::socket::{Listener, control_path_for};
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

/// A pseudoterminal pair standing in for the outer terminal of `cloo attach`.
struct Tty {
    master: OwnedFd,
    slave: OwnedFd,
}

fn open_tty() -> Tty {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let winsize = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes one descriptor into each valid out parameter.
    // The termios pointer is null (use defaults) and `winsize` is live.
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    assert_ne!(
        rc,
        -1,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `openpty` succeeded, so both unowned descriptors are open.
    unsafe {
        Tty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    }
}

/// Reads a pty until text appears, without blocking past [`PATIENCE`].
fn read_until(fd: &OwnedFd, needle: &str) -> Result<String, String> {
    let mut file = unsafe {
        // SAFETY: the descriptor is owned by the caller and outlives this
        // `ManuallyDrop`, which intentionally never closes it.
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(fd.as_raw_fd()))
    };
    let deadline = Instant::now() + PATIENCE;
    let mut seen = Vec::new();
    let mut buf = [0_u8; 4096];
    while Instant::now() < deadline {
        if !readable_before(fd, deadline) {
            break;
        }
        match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                seen.extend_from_slice(&buf[..read]);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return Ok(String::from_utf8_lossy(&seen).into_owned());
                }
            }
        }
    }
    Err(String::from_utf8_lossy(&seen).into_owned())
}

/// Waits until the pty has data or the deadline expires.
fn readable_before(fd: &OwnedFd, deadline: Instant) -> bool {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis(),
    )
    .unwrap_or(i32::MAX);
    // SAFETY: `poll` receives one live `pollfd`, matching the count.
    unsafe { libc::poll(&raw mut poll, 1, millis) != 0 }
}

/// Waits for a command to exit without allowing a broken client loop to hang
/// the entire test process.
fn wait_for_exit(client: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(status) = client
            .try_wait()
            .expect("the attached client remains waitable")
        {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    client
        .kill()
        .expect("the wedged attached client can be stopped");
    let _ = client.wait();
    panic!("the attached client did not exit after its detach command");
}

/// Whether the slave terminal is currently in raw mode.
fn is_raw(fd: &OwnedFd) -> bool {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` writes exactly one termios into the live pointer.
    let rc = unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) };
    assert_ne!(rc, -1, "tcgetattr failed");
    // SAFETY: the successful call initialized the value.
    let termios = unsafe { termios.assume_init() };
    termios.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG) == 0
}

/// Runs the binary attached to `socket` with all its stdio on `tty`.
fn spawn_attached_on(tty: &Tty, socket: &Path) -> std::process::Child {
    let stdio = || {
        Stdio::from(
            tty.slave
                .try_clone()
                .expect("the slave descriptor can be duplicated"),
        )
    };
    Command::new(env!("CARGO_BIN_EXE_cloo"))
        .arg("attach")
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio())
        .env("CLOO_SOCKET", socket)
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM")
        .spawn()
        .expect("the cloo binary is built before its integration tests")
}

/// A daemon running in the background for the synchronous command test.
///
/// Its stop channel is important: a daemon remains available after its child
/// has exited so a later client can inspect the session, and therefore does
/// not terminate on its own merely because this fixture has finished.
struct ThreadDaemon {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ThreadDaemon {
    /// Stops the daemon and waits for its runtime to release the socket.
    fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the daemon thread does not panic");
        }
    }
}

impl Drop for ThreadDaemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs a daemon in its own runtime so a blocking pty read in this synchronous
/// binary test cannot prevent it from accepting or publishing damage.
fn spawn_daemon_thread(socket: PathBuf, script: &'static str) -> ThreadDaemon {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("the daemon runtime builds");
        runtime.block_on(async move {
            let listener = Listener::bind(&socket).expect("a fresh socket must bind");
            let mut daemon =
                Daemon::new(listener, &base(), scripted(script)).expect("the daemon starts");
            ready_tx.send(()).expect("the test waits for the daemon");
            tokio::select! {
                result = daemon.run() => {
                    result.expect("the daemon stays healthy");
                }
                _ = stop_rx => {}
            }
        });
    });
    ready_rx
        .recv_timeout(PATIENCE)
        .expect("the daemon must bind before the client starts");
    ThreadDaemon {
        stop: Some(stop_tx),
        thread: Some(thread),
    }
}

/// The session's half of a config at 80x24: geometry and `TERM`.
fn base() -> PtyConfig {
    PtyConfig::session(Size::new(80, 24))
        .expect("80x24 is a valid size")
        .env("TERM", "xterm-256color")
}

/// A launch running `script` under `sh`, named the way a user would name it.
fn scripted(script: &str) -> Launch {
    let mut profile = Profile::generic();
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
    };
    Launch::new(
        profile,
        Some(PaneName::new("api").expect("a valid name")),
        Some(TaskLabel::new("fix the flaky test").expect("a valid label")),
        WorkingDir::new("/").expect("absolute"),
    )
    .expect("the generic profile validates")
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
    let mut daemon =
        Daemon::new(listener, &base(), scripted(script)).expect("the daemon must start");
    let pid = daemon.child_id();
    let handle = tokio::spawn(async move { daemon.run().await });
    (pid, handle)
}

/// Binds a daemon whose logical name is independent of its fixture socket.
fn spawn_named_daemon(
    socket: &Path,
    name: &str,
    script: &str,
) -> tokio::task::JoinHandle<Result<std::process::ExitStatus, cloo_server::DaemonError>> {
    let listener = Listener::bind(socket).expect("a fresh socket path must bind");
    let mut daemon = Daemon::new(listener, &base(), scripted(script))
        .expect("the daemon must start")
        .with_session_name(name);
    tokio::spawn(async move { daemon.run().await })
}

/// The same, for a daemon whose profile table came from a `config.toml`.
///
/// Parsed rather than assembled, because that is the only way a profile reaches
/// a running server: what a typed launch request can name is exactly what the
/// user's document defined, and this fixture is that document.
fn spawn_daemon_with_config(
    socket: &Path,
    script: &str,
    document: &str,
) -> tokio::task::JoinHandle<Result<std::process::ExitStatus, cloo_server::DaemonError>> {
    let config = cloo_core::config::parse(document)
        .expect("the test document is valid")
        .config;
    let listener = Listener::bind(socket).expect("a fresh socket path must bind");
    let mut daemon = Daemon::new(listener, &base(), scripted(script))
        .expect("the daemon must start")
        .with_config(config);
    tokio::spawn(async move { daemon.run().await })
}

/// The same, for a pane whose profile opted into `adapter`.
///
/// The opt-in is the profile's, so it has to be set before the pane exists —
/// which is exactly how a user's `config.toml` sets it.
fn spawn_daemon_with_adapter(
    socket: &Path,
    script: &str,
    adapter: &str,
) -> tokio::task::JoinHandle<Result<std::process::ExitStatus, cloo_server::DaemonError>> {
    let mut profile = Profile::generic().adapter(AdapterId::new(adapter).expect("a valid id"));
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
    };
    let launch = Launch::new(
        profile,
        Some(PaneName::new("api").expect("a valid name")),
        None,
        WorkingDir::new("/").expect("absolute"),
    )
    .expect("the profile validates");

    let listener = Listener::bind(socket).expect("a fresh socket path must bind");
    let mut daemon = Daemon::new(listener, &base(), launch).expect("the daemon must start");
    tokio::spawn(async move { daemon.run().await })
}

/// Connects to a daemon's adapter control socket and announces `adapter`.
async fn control(socket: &Path, adapter: &str) -> FrameStream<UnixStream> {
    let path = control_path_for(socket);
    let stream = tokio::time::timeout(PATIENCE, async {
        loop {
            match UnixStream::connect(&path).await {
                Ok(stream) => return stream,
                // The daemon binds both sockets before it accepts on either, so
                // this only spins while the task is being scheduled.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("the control socket must appear");

    let mut conn = FrameStream::new(stream);
    conn.send(&AdapterMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        adapter: adapter.to_owned(),
    })
    .await
    .expect("the hello must send");
    let reply: Option<AdapterReply> = conn.recv().await.expect("the control socket must answer");
    assert!(
        matches!(reply, Some(AdapterReply::Ready { .. })),
        "expected a ready, got {reply:?}"
    );
    conn
}

/// Reads frames until the server reports `pane`'s attention as `state`.
async fn await_attention(
    attached: &mut Attached<UnixStream>,
    pane: PaneId,
    state: AttentionState,
) -> cloo_proto::PaneAttention {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Attention(states)) => {
                    if let Some(found) = states
                        .into_iter()
                        .find(|att| att.pane == pane && att.state == state)
                    {
                        return found;
                    }
                }
                Some(_) => {}
                None => panic!("the server closed before reporting {state:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("pane {pane:?} was never reported {state:?}"))
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
    client_with_caps(socket, TermCaps::default()).await
}

/// Attaches with exactly the capabilities a client policy will later use.
async fn client_with_caps(socket: &Path, caps: TermCaps) -> Attached<UnixStream> {
    tokio::time::timeout(PATIENCE, attach(socket, Size::new(80, 24), caps, None))
        .await
        .expect("the attach must not hang")
        .expect("the attach must succeed")
}

/// Attaches at a specific outer-terminal size, so a test can drive the session's
/// minimum-size negotiation across several clients.
async fn client_sized(socket: &Path, size: Size) -> Attached<UnixStream> {
    tokio::time::timeout(PATIENCE, attach(socket, size, TermCaps::default(), None))
        .await
        .expect("the attach must not hang")
        .expect("the attach must succeed")
}

/// Reads until one matching typed outer-terminal request arrives.
async fn await_effect(
    attached: &mut Attached<UnixStream>,
    want: &OuterTerminalEffect,
) -> OuterTerminalEffect {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Effect { effect, .. }) if &effect == want => return effect,
                Some(_) => {}
                None => panic!("the server closed before sending {want:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw typed effect {want:?}"))
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

/// Reads until the server describes who the panes are.
async fn await_panes(attached: &mut Attached<UnixStream>) -> Vec<cloo_proto::PaneInfo> {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Panes(panes)) => return panes,
                Some(_) => {}
                None => panic!("the server closed before describing its panes"),
            }
        }
    })
    .await
    .expect("pane identity must reach an attached client")
}

/// Reads until the daemon publishes exactly the expected attached status.
async fn await_workspace_status(
    attached: &mut Attached<UnixStream>,
    expected: WorkspaceStatus,
) -> WorkspaceStatus {
    let status = tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::WorkspaceStatus(status)) if status == expected => {
                    return status;
                }
                Some(_) => {}
                None => panic!("the server closed before reporting {expected:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the daemon never reported {expected:?}"));
    assert_eq!(
        attached.status(),
        Some(&status),
        "the attached client must replace its cached projection"
    );
    assert_eq!(
        attached.size(),
        status.effective_size,
        "effective size comes from workspace status"
    );
    status
}

#[test]
fn cli_attach_composes_the_frame_and_detaches_without_losing_the_session() {
    let dir = TempDir::new("cli-live-loop");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "printf attached-client-ok; read _; exit 0");
    let tty = open_tty();
    let mut client = spawn_attached_on(&tty, &socket);

    let seen = read_until(&tty.master, "attached-client-ok")
        .unwrap_or_else(|seen| panic!("the attached frame never rendered; saw:\n{seen}"));
    assert!(
        seen.contains("\x1b[?25l"),
        "the client did not render a frame; saw:\n{seen}"
    );
    assert!(
        seen.contains("api"),
        "the pane header was not composed; saw:\n{seen}"
    );
    assert!(is_raw(&tty.slave), "the attach loop enters raw mode");

    let mut master = unsafe {
        // SAFETY: `tty.master` outlives this wrapper, which never closes it.
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(tty.master.as_raw_fd()))
    };
    master
        .write_all(b"\x02d")
        .expect("the prefix detach reaches cloo");
    let status = wait_for_exit(&mut client);
    assert!(status.success(), "cloo attach exited with {status}");
    assert!(
        !is_raw(&tty.slave),
        "cloo attach left the terminal raw after detach"
    );

    // Detach is only a client event. A fresh attachment can still reach the
    // child and unblock it, proving the command's loop did not own the PTY.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("the reattach runtime builds");
    runtime.block_on(async {
        let mut reattached = attach(&socket, Size::new(80, 24), TermCaps::default(), None)
            .await
            .expect("the detached session accepts another client");
        reattached
            .send_input(b"\n".to_vec())
            .await
            .expect("the reattached client reaches the child");
    });
    daemon.stop();
}

#[tokio::test]
async fn a_clients_resync_says_who_every_pane_is() {
    // A client caches the visible grid and nothing else, so the identity it
    // draws in a pane header has to arrive over the wire. All of it is explicit:
    // the profile the pane was launched from, and what the user called it.
    let dir = TempDir::new("panes");
    let socket = dir.socket();
    let (_, daemon) = spawn_daemon(&socket, "read _; exit 0");

    let mut attached = client(&socket).await;
    let panes = await_panes(&mut attached).await;
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].profile, "generic");
    assert_eq!(panes[0].name, "api");
    assert_eq!(panes[0].task.as_deref(), Some("fix the flaky test"));
    assert_eq!(panes[0].cwd, "/");

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    let _ = tokio::time::timeout(PATIENCE, daemon).await;
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
async fn workspace_status_tracks_attach_resize_and_detach_without_duplicate_projections() {
    let dir = TempDir::new("workspace-status");
    let socket = dir.socket();
    let daemon = spawn_named_daemon(&socket, "agents", "read _; exit 0");

    let mut first = client_sized(&socket, Size::new(100, 30)).await;
    await_workspace_status(
        &mut first,
        WorkspaceStatus {
            name: "agents".into(),
            clients: 1,
            effective_size: Size::new(100, 30),
        },
    )
    .await;

    let mut second = client_sized(&socket, Size::new(120, 40)).await;
    let joined = WorkspaceStatus {
        name: "agents".into(),
        clients: 2,
        effective_size: Size::new(100, 30),
    };
    await_workspace_status(&mut first, joined.clone()).await;
    await_workspace_status(&mut second, joined).await;

    // This client changed, but neither the minimum nor any other projected
    // field did. No status frame should be invented for a private size.
    second
        .send_resize(Size::new(140, 50))
        .await
        .expect("the larger resize reaches the daemon");
    let duplicate = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            match second.recv().await.expect("the connection must hold") {
                Some(ServerMessage::WorkspaceStatus(status)) => return Some(status),
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await;
    assert!(
        duplicate.is_err(),
        "an unchanged projection must publish no status, got {duplicate:?}"
    );

    // Sending a resize is only a report. The client keeps the daemon's
    // effective answer until the next status arrives.
    second
        .send_resize(Size::new(80, 20))
        .await
        .expect("the narrower resize reaches the daemon");
    assert_eq!(second.size(), Size::new(100, 30));
    let narrowed = WorkspaceStatus {
        name: "agents".into(),
        clients: 2,
        effective_size: Size::new(80, 20),
    };
    await_workspace_status(&mut first, narrowed.clone()).await;
    await_workspace_status(&mut second, narrowed).await;

    second.detach().await.expect("the second client detaches");
    await_workspace_status(
        &mut first,
        WorkspaceStatus {
            name: "agents".into(),
            clients: 1,
            effective_size: Size::new(100, 30),
        },
    )
    .await;

    first
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
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
async fn a_typed_clipboard_effect_reaches_a_capable_client_once() {
    let dir = TempDir::new("clipboard-effect");
    let socket = dir.socket();
    // The first read lets the client attach before the effect is emitted. OSC
    // 52 carries base64 `hi`; `cloo-term` decodes it into the typed text the
    // server then fans out, never into raw terminal bytes.
    let (_pid, daemon) = spawn_daemon(
        &socket,
        "read _; printf '\\033]52;c;aGk=\\007'; read _; exit 0",
    );
    let caps = TermCaps {
        clipboard_osc52: true,
        ..TermCaps::default()
    };
    let mut attached = client_with_caps(&socket, caps).await;

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must start the effect fixture");
    let expected = OuterTerminalEffect::ClipboardStore {
        target: ClipboardTarget::Clipboard,
        text: "hi".into(),
    };
    let effect = await_effect(&mut attached, &expected).await;
    assert_eq!(effect, expected);

    // Client policy is the final gate. This byte-exact assertion proves the
    // typed request reaches a capable, permitted client exactly once rather
    // than a raw OSC payload bypassing the renderer.
    let mut terminal = Vec::new();
    assert!(
        apply_effect(
            &mut terminal,
            caps,
            EffectPolicy::allow_supported(),
            &effect,
        )
        .expect("the in-memory terminal accepts one effect")
    );
    assert_eq!(terminal, b"\x1b]52;c;aGk=\x1b\\");

    // The script's second read keeps the daemon alive until the assertion is
    // complete, then lets it reap normally.
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
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
async fn composed_frame_resize_reaches_the_interior_grid_and_child() {
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

    // Half one: the session task reflowed the child grid to the framed
    // interior, so the two side edges never become application columns.
    await_row_width(&mut attached, 98).await;

    // Half two: the child's own view of its terminal changed, which only
    // `TIOCSWINSZ` on the pty master can have done. `stty size` prints
    // "rows cols".
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    await_text(&mut attached, "38 98").await;

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
    await_text(&mut attached, "22 78").await;

    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// The reconnect/resize race: a narrower client joins and then leaves, and the
/// survivor's grid must track the negotiated minimum both down and back up.
///
/// If a departing client's resize did not reach the survivor, a later full-width
/// row would be applied against a cache still stuck at the narrow width — the
/// exact geometry disagreement that corrupts a client grid. Asserting the
/// survivor sees rows first 40 then 80 cells wide is asserting that never happens.
#[tokio::test]
async fn a_shrinking_client_that_leaves_redraws_the_survivor_at_full_width() {
    let dir = TempDir::new("resize-race");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, "printf 'ready\\n'; read _; exit 0");

    let mut wide = client_sized(&socket, Size::new(80, 24)).await;
    assert_eq!(wide.size(), Size::new(80, 24));
    await_text(&mut wide, "ready").await;

    // A narrower client drags the session down to the component-wise minimum.
    // The survivor's grid reflows to 40 columns with it.
    let narrow = client_sized(&socket, Size::new(40, 24)).await;
    await_row_width(&mut wide, 38).await;

    // The narrow client detaches. With only the wide client left the session
    // grows back to 80, and that resize must reach the survivor as a full redraw
    // rather than leaving its cache at the stale narrow width.
    narrow
        .detach()
        .await
        .expect("the narrow client detaches cleanly");
    await_row_width(&mut wide, 78).await;

    // Let the child exit so the daemon can reap and finish.
    wide.send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// Two clients at different sizes render the same session, so both must converge
/// on the negotiated minimum rather than each drawing its own width.
///
/// Per-client independent sizing is explicitly out of scope; the guarantee is
/// that two attached clients stay visually consistent. A row exactly 50 cells
/// wide reaching *both* is that consistency made assertable.
#[tokio::test]
async fn two_clients_at_different_sizes_share_the_negotiated_minimum() {
    let dir = TempDir::new("shared-min");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, "printf 'ready\\n'; read _; exit 0");

    let mut wide = client_sized(&socket, Size::new(80, 24)).await;
    await_text(&mut wide, "ready").await;

    // The smaller client sets the minimum for the whole session, including the
    // client that was already attached at a larger size.
    let mut narrow = client_sized(&socket, Size::new(50, 24)).await;
    await_row_width(&mut narrow, 48).await;
    await_row_width(&mut wide, 48).await;

    wide.send_input(b"\n".to_vec())
        .await
        .expect("input must reach the child");
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

#[tokio::test]
async fn copy_mode_highlights_reach_a_client_and_its_copy_is_policy_gated() {
    // The whole loop in one test: server-owned copy state crosses the wire,
    // the client turns it into highlights over its own cached grid without
    // changing a cell, and the explicit copy comes back as one typed clipboard
    // effect that local policy can still refuse.
    let dir = TempDir::new("copy-mode");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(
        &socket,
        "printf 'first\\nneedle one\\nneedle two\\nlast\\n'; read _; exit 0",
    );
    let caps = TermCaps {
        clipboard_osc52: true,
        ..TermCaps::default()
    };
    let mut attached = client_with_caps(&socket, caps).await;

    // A client caches the visible grid and nothing else, so it applies damage
    // exactly as the real render loop does — starting with the attach snapshot,
    // which is why the cache is filled before any copy command is sent.
    let mut grid = Grid::new(Size::new(78, 22));
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Damage { rows, .. }) => {
                    for row in &rows {
                        grid.apply(row).expect("the server's rows fit its geometry");
                    }
                    if rows.iter().any(|row| row_text(row) == "last") {
                        return;
                    }
                }
                Some(_) => {}
                None => panic!("the server closed before the fixture printed"),
            }
        }
    })
    .await
    .expect("the fixture's output must reach the client");

    for action in [
        Action::EnterCopyMode,
        Action::CopySearch {
            query: "needle one".into(),
            direction: SearchDirection::Forward,
        },
        Action::BeginCopySelection,
        Action::CopyMotion(CopyMotion::LineEnd),
        Action::CopySelection(ClipboardTarget::Clipboard),
    ] {
        attached
            .send_command(action)
            .await
            .expect("a copy-mode command must reach the daemon");
    }

    let mut copy_state: Option<cloo_proto::CopyModeState> = None;
    let mut copied: Option<OuterTerminalEffect> = None;
    tokio::time::timeout(PATIENCE, async {
        while copy_state.is_none() || copied.is_none() {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Damage { rows, .. }) => {
                    for row in &rows {
                        grid.apply(row).expect("the server's rows fit its geometry");
                    }
                }
                Some(ServerMessage::CopyMode(Some(state))) => {
                    if state.selection.is_some() {
                        copy_state = Some(state);
                    }
                }
                Some(ServerMessage::Effect { effect, .. }) => copied = Some(effect),
                Some(_) => {}
                None => panic!("the server closed during copy mode"),
            }
        }
    })
    .await
    .expect("copy state and the copied text must both reach the client");

    let state = copy_state.expect("the loop only exits with copy state");
    assert_eq!(state.query.as_deref(), Some("needle one"));
    assert_eq!(state.matches.len(), 1);

    // The client renders the selection as positioned spans and leaves its cache
    // exactly as the server described it.
    let before = grid.clone();
    let spans = highlight_spans(Point::new(0, 0), &grid, &state, Theme::storm());
    let selected: String = spans
        .iter()
        .flat_map(|span| span.cells.iter().map(|cell| cell.ch))
        .collect();
    assert!(
        selected.contains("needle one"),
        "the highlight must cover the selected text, got {selected:?}"
    );
    assert_eq!(
        grid, before,
        "rendering a selection must not mutate the grid"
    );

    // The copy itself: one typed effect, refused by the default policy and
    // written byte for byte by a permitting one.
    let effect = copied.expect("the loop only exits with a copied effect");
    assert_eq!(
        effect,
        OuterTerminalEffect::ClipboardStore {
            target: ClipboardTarget::Clipboard,
            text: "needle one".into(),
        }
    );
    let mut terminal = b"rendered frame".to_vec();
    let before = terminal.clone();
    assert!(
        !apply_copy(&mut terminal, caps, EffectPolicy::default(), &effect)
            .expect("a denied copy does not write")
    );
    assert_eq!(terminal, before, "a denied copy is a no-op");

    let mut terminal = Vec::new();
    assert!(
        apply_copy(
            &mut terminal,
            caps,
            EffectPolicy::allow_supported(),
            &effect,
        )
        .expect("the in-memory terminal accepts one store")
    );
    assert_eq!(terminal, b"\x1b]52;c;bmVlZGxlIG9uZQ==\x1b\\");

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    drop(attached);
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// Reads frames until the focused pane's copy cursor sits on `line`, or until
/// any copy state arrives when `line` is `None`.
async fn await_copy_cursor(
    attached: &mut Attached<UnixStream>,
    line: Option<u32>,
) -> cloo_proto::CopyModeState {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::CopyMode(Some(state)))
                    if line.is_none_or(|want| state.cursor.line == want) =>
                {
                    return state;
                }
                Some(_) => {}
                None => panic!("the server closed before answering with copy state"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no copy state ever put the cursor on {line:?}"))
}

/// The wheel's whole loop, over a real socket. A report the client's own screen
/// says is chrome's becomes a `ChromeAction`, that becomes the copy-mode commands
/// the *keyboard* already sends, and the server answers with copy state whose
/// cursor moved — which is the point of the equivalence, not a detail of it.
#[tokio::test]
async fn a_wheel_over_a_pane_walks_scrollback_through_the_copy_mode_commands() {
    let dir = TempDir::new("wheel");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(
        &socket,
        "printf 'one\\ntwo\\nthree\\nfour\\nlast\\n'; read _; exit 0",
    );
    let mut attached = client(&socket).await;
    let pane = PaneId::new(1);
    await_text(&mut attached, "last").await;

    // The client describes the screen it drew and hit-tests the report against
    // it. A shell that never asked for the mouse owns nothing, so this is
    // chrome's — and a chrome route has no wire event at all.
    let screen = ScreenLayout::single(Size::new(80, 24), pane);
    let report = MouseReport {
        kind: MouseKind::ScrollUp,
        mods: MouseMods::NONE,
        col: 10,
        row: 4,
    };
    let route = route_mouse(&screen, PaneModes::default(), &report);
    assert_eq!(route.wire_event(), None, "a wheel here is not the child's");
    let MouseRoute::Chrome(target) = route else {
        panic!("expected a chrome route, got {route:?}");
    };

    let mut chrome = ChromeMouse::new();
    let action = chrome
        .feed(&screen, target, &report)
        .expect("a wheel over a pane scrolls it");
    let commands = action.commands(None);
    assert_eq!(
        commands
            .iter()
            .filter(|action| matches!(action, Action::CopyMotion(CopyMotion::Up)))
            .count(),
        usize::from(WHEEL_LINES),
        "the wheel is copy-mode motions and nothing else: {commands:?}"
    );
    // Sent in two halves only so the test has a baseline to measure against: a
    // real client sends the whole list, and the server coalesces the frames it
    // answers with, which would leave nothing to compare the final cursor to.
    // Copy mode starts on the newest retained line, so the notch is proved by
    // the cursor landing exactly `WHEEL_LINES` above it rather than by copy mode
    // merely being on.
    let split = commands
        .iter()
        .position(|action| matches!(action, Action::EnterCopyMode))
        .expect("a pane not in copy mode is asked to enter it")
        + 1;
    for command in &commands[..split] {
        attached
            .send_command(command.clone())
            .await
            .expect("a wheel command must reach the daemon");
    }
    let entered = await_copy_cursor(&mut attached, None).await.cursor.line;
    for command in &commands[split..] {
        attached
            .send_command(command.clone())
            .await
            .expect("a wheel command must reach the daemon");
    }
    let state = await_copy_cursor(
        &mut attached,
        Some(entered.saturating_sub(u32::from(WHEEL_LINES))),
    )
    .await;

    assert_eq!(state.pane, pane);
    assert!(
        state.selection.is_none(),
        "scrolling selects nothing; it only moves the view"
    );
    assert_eq!(state.cursor.line + u32::from(WHEEL_LINES), entered);

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    drop(attached);
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

/// Reads frames until a layout snapshot matching `want` arrives.
///
/// The tracker sends a [`ServerMessage::Layout`] only when the geometry
/// changed, so a predicate — not a fixed frame count — is what pins a test to
/// the layout it means.
async fn await_layout(
    attached: &mut Attached<UnixStream>,
    want: impl Fn(&cloo_proto::LayoutSnapshot) -> bool,
) -> cloo_proto::LayoutSnapshot {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Layout(layout)) if want(&layout) => return layout,
                Some(_) => {}
                None => panic!("the server closed before the layout matched"),
            }
        }
    })
    .await
    .expect("the layout must reach the expected shape")
}

#[tokio::test]
async fn split_zoom_and_close_from_a_client_move_the_reported_layout() {
    // These four commands used to fall into the daemon's catch-all and be
    // dropped. Each of split, zoom, and close is sent over the wire and proved
    // to change the layout the session actor reports — the geometry a client
    // draws from, on its own wire clock.
    let dir = TempDir::new("layout-commands");
    let socket = dir.socket();
    let (_pid, daemon) = spawn_daemon(&socket, "read _; exit 0");

    let mut attached = client(&socket).await;
    let first = PaneId::new(1);

    // One pane at the full area, nothing zoomed.
    let start = await_layout(&mut attached, |l| l.panes.len() == 1).await;
    assert_eq!(start.zoomed, None);
    assert_eq!(start.focused, Some(first));

    // A vertical divider stands two panes side by side. Focus follows the split,
    // which is what makes splitting and then typing do what a user means.
    attached
        .send_command(Action::SplitVertical)
        .await
        .expect("a split must reach the daemon");
    let split = await_layout(&mut attached, |l| l.panes.len() == 2).await;
    let second = split.focused.expect("a split focuses its new pane");
    assert_ne!(second, first, "the new pane is focused, not the old one");

    // Zoom shows the focused pane alone at the full area; the tree behind it is
    // unchanged, so this is a view flag rather than a reshaping.
    attached
        .send_command(Action::ToggleZoom)
        .await
        .expect("zoom must reach the daemon");
    let zoomed = await_layout(&mut attached, |l| l.zoomed.is_some()).await;
    assert_eq!(zoomed.zoomed, Some(second));
    assert_eq!(
        zoomed.panes.len(),
        1,
        "a zoomed layout resolves to one pane"
    );
    assert_eq!(zoomed.panes[0].pane, second);

    // Unzoom brings the second pane back into the geometry.
    attached
        .send_command(Action::ToggleZoom)
        .await
        .expect("unzoom must reach the daemon");
    let unzoomed = await_layout(&mut attached, |l| l.zoomed.is_none() && l.panes.len() == 2).await;
    assert_eq!(unzoomed.zoomed, None);

    // Close names no pane: the session resolves it from its own focus, so the
    // new pane goes and focus falls back to the survivor.
    attached
        .send_command(Action::ClosePane)
        .await
        .expect("close must reach the daemon");
    let closed = await_layout(&mut attached, |l| l.panes.len() == 1).await;
    assert_eq!(
        closed.focused,
        Some(first),
        "focus falls back to the survivor"
    );
    assert_eq!(closed.panes[0].pane, first);

    // Let the surviving child exit so the daemon can finish.
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    drop(attached);
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn a_typed_launch_request_creates_a_pane_from_the_servers_own_profile_table() {
    // The whole loop for M8-05: a client sends an *identifier*, the daemon
    // resolves it against the configuration it loaded, and the pane that
    // appears carries that profile's identity. Nothing on the wire could have
    // named a command — `Action::LaunchProfile` has no field for one.
    let dir = TempDir::new("launch-profile");
    let socket = dir.socket();
    let daemon = spawn_daemon_with_config(
        &socket,
        "read _; exit 0",
        r#"
        [[profile]]
        id = "harness"
        command = ["sh", "-c", "read _; exit 0"]
        "#,
    );

    let mut attached = client(&socket).await;
    let start = await_panes(&mut attached).await;
    assert_eq!(start.len(), 1);

    // An identifier the server's table does not name is refused, and so is a
    // command line — which reaches the daemon only ever as an ID to look up.
    // Sent *first*, so a pane it wrongly created would still be counted below.
    for refused in ["no-such-profile", "", "sh -c 'echo hi'"] {
        attached
            .send_command(Action::LaunchProfile(refused.to_owned()))
            .await
            .expect("the request must reach the daemon");
    }
    attached
        .send_command(Action::LaunchProfile("harness".to_owned()))
        .await
        .expect("the request must reach the daemon");

    let panes = tokio::time::timeout(PATIENCE, async {
        loop {
            match attached.recv().await.expect("the connection must hold") {
                Some(ServerMessage::Panes(panes))
                    if panes.iter().any(|info| info.profile == "harness") =>
                {
                    return panes;
                }
                Some(_) => {}
                None => panic!("the server closed before launching the profile"),
            }
        }
    })
    .await
    .expect("a configured profile must reach the session");

    assert_eq!(
        panes.len(),
        2,
        "only the configured identifier created a pane; got {panes:?}"
    );
    let launched = panes
        .iter()
        .find(|info| info.profile == "harness")
        .expect("the loop above waited for it");
    assert_eq!(
        launched.name, "harness",
        "a request names a profile and nothing else, so the pane takes the profile's own name"
    );
    assert_eq!(launched.task, None, "cloo never invents a task label");
    assert_eq!(
        launched.cwd, "/",
        "a launched pane starts in the workspace's own directory, not one a client chose"
    );

    // Close the launched pane — the split focused it — and let the survivor
    // exit so the daemon can finish.
    attached
        .send_command(Action::ClosePane)
        .await
        .expect("close must reach the daemon");
    let closed = await_layout(&mut attached, |layout| layout.panes.len() == 1).await;
    assert_eq!(closed.panes.len(), 1);
    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    drop(attached);
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}

#[tokio::test]
async fn an_opt_in_adapter_reports_advisory_state_that_reaches_a_client_attributed() {
    // The whole loop for the one advisory source: a separate local process
    // connects to the control socket, claims a state for a pane whose profile
    // opted into it, and an attached client is told both the state *and* who
    // said so. The pane is the daemon's first pane, which is always id 1.
    let dir = TempDir::new("adapter");
    let socket = dir.socket();
    let daemon = spawn_daemon_with_adapter(&socket, "read _; exit 0", "my-adapter");
    let pane = PaneId::new(1);

    let mut attached = client(&socket).await;
    let mut adapter = control(&socket, "my-adapter").await;

    adapter
        .send(&AdapterMessage::Report {
            pane,
            state: AdapterState::NeedsInput,
        })
        .await
        .expect("the report must send");
    let reply: Option<AdapterReply> = adapter.recv().await.expect("every report is answered");
    assert_eq!(
        reply,
        Some(AdapterReply::Applied { pane }),
        "an opted-in adapter's report must be applied"
    );

    let att = await_attention(&mut attached, pane, AttentionState::NeedsInput).await;
    assert_eq!(
        att.source,
        AttentionSource::Adapter("my-adapter".to_owned()),
        "the claim must reach the chrome attributed, never as an observed fact"
    );
    assert!(!att.acknowledged);

    // An impostor gets a refusal it can print, and changes nothing: the profile
    // named one adapter, and that name is the user's whole consent.
    let mut impostor = control(&socket, "someone-else").await;
    impostor
        .send(&AdapterMessage::Report {
            pane,
            state: AdapterState::Ready,
        })
        .await
        .expect("the report must send");
    let reply: Option<AdapterReply> = impostor.recv().await.expect("every report is answered");
    assert_eq!(
        reply,
        Some(AdapterReply::Rejected {
            pane,
            reason: AdapterRejection::NotPermitted,
        })
    );

    // Prove the refusal was a no-op rather than a race: a state the permitted
    // adapter reports afterwards is the next one the client sees.
    adapter
        .send(&AdapterMessage::Report {
            pane,
            state: AdapterState::Failed,
        })
        .await
        .expect("the report must send");
    let att = await_attention(&mut attached, pane, AttentionState::Failed).await;
    assert_eq!(
        att.source,
        AttentionSource::Adapter("my-adapter".to_owned())
    );

    attached
        .send_input(b"\n".to_vec())
        .await
        .expect("input must let the fixture exit");
    drop(attached);
    tokio::time::timeout(PATIENCE, daemon)
        .await
        .expect("the daemon must exit")
        .expect("the daemon task must not panic")
        .expect("the daemon must not fail");
}
