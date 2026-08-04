//! The eight handoff states, driven through the real attached client.
//!
//! `crates/cloo-client/tests/visual.rs` asserts what the frame composer *would*
//! draw for a scene assembled in memory. This file asserts the other half of the
//! M9 acceptance contract: that the shipped `cloo attach` binary, given a real
//! daemon and a real pseudoterminal, actually reaches those states. Nothing here
//! constructs a `Scene`, calls a chrome helper, or reads a client cache — every
//! assertion is made against the bytes the binary wrote to an outer terminal
//! after keys were typed into it.
//!
//! That is deliberately the slow, coarse half. A byte stream tells you the
//! composition arrived; it cannot tell you a role resolved to the right colour,
//! and it should not try to — the cell goldens own that, and duplicating them
//! here would only produce a second, weaker expectation to keep in step. What
//! these fixtures prove is the part a pure fixture structurally cannot: that the
//! chord is bound, the surface opens over the live frame, the daemon answered,
//! and the terminal is handed back unmodified afterwards.
//!
//! Every fixture ends the same way — detach with the prefix, wait for the
//! process, and assert the outer terminal is no longer raw — because a surface
//! that renders beautifully and then strands a shell in raw mode has not passed.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, io};

use cloo_core::pane::{PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{AdapterId, Profile, ProfileCommand};
use cloo_proto::{
    AdapterMessage, AdapterReply, AdapterState, FrameStream, PROTOCOL_VERSION, PaneId, Size,
};
use cloo_server::daemon::Daemon;
use cloo_server::launch::Launch;
use cloo_server::pty::PtyConfig;
use cloo_server::socket::{Listener, control_path_for};
use tokio::net::UnixStream;

/// The longest any fixture waits for a frame, a process, or a daemon.
const PATIENCE: Duration = Duration::from_secs(10);

/// The default prefix chord, as a byte.
const PREFIX: u8 = 0x02;

// ---------------------------------------------------------------------------
// Fixture scaffolding
// ---------------------------------------------------------------------------

/// A unique temporary directory for one fixture, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cloo-visual-test-{}-{tag}-{n}", std::process::id()));
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

/// The outer terminal of `cloo attach`, and the handle a fixture types into.
struct Tty {
    master: OwnedFd,
    slave: OwnedFd,
}

impl Tty {
    /// Opens a pseudoterminal at the reference geometry of these fixtures.
    fn open() -> Self {
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
        assert_ne!(rc, -1, "openpty failed: {}", io::Error::last_os_error());
        // SAFETY: `openpty` succeeded, so both unowned descriptors are open.
        unsafe {
            Self {
                master: OwnedFd::from_raw_fd(master),
                slave: OwnedFd::from_raw_fd(slave),
            }
        }
    }

    /// Types raw bytes at the client, exactly as a user's terminal would.
    fn type_bytes(&self, bytes: &[u8]) {
        let mut file = unsafe {
            // SAFETY: `self.master` outlives this wrapper, which never closes it.
            std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(self.master.as_raw_fd()))
        };
        file.write_all(bytes)
            .expect("the outer terminal accepts input");
    }

    /// Types a prefixed chord: the prefix, then one key.
    fn chord(&self, key: u8) {
        self.type_bytes(&[PREFIX, key]);
    }

    /// Reads rendered output until every needle has appeared on screen.
    ///
    /// Matching is against the *visible* text, with the positioning and styling
    /// sequences removed, for two reasons. A composed frame interleaves an SGR
    /// run with almost every field, so a byte-adjacency match would assert about
    /// the renderer's run-splitting rather than about the picture. And colour is
    /// not this file's claim at all — the cell goldens own role resolution, and a
    /// second, weaker expectation over the same bytes would only be one more
    /// thing to keep in step.
    ///
    /// Waiting for *all* the needles is what makes a multi-part assertion sound:
    /// a surface arrives over several writes, and stopping at the first one would
    /// make every later check a race.
    fn expect_all(&self, needles: &[&str], what: &str) -> String {
        read_until(&self.master, needles)
            .unwrap_or_else(|seen| panic!("{what} never rendered; saw:\n{seen}"))
    }

    /// The single-needle form.
    fn expect(&self, needle: &str, what: &str) -> String {
        self.expect_all(&[needle], what)
    }

    /// Dismisses an open overlay and waits for the frame beneath to repaint.
    ///
    /// The wait is the point. A lone Escape is flushed on a frame tick rather
    /// than immediately, so a prefix chord typed straight after it can be
    /// swallowed as the tail of an escape sequence — which would leave the
    /// client attached and the fixture blaming detach for an input race.
    fn close_overlay(&self, repainted: &str) {
        self.type_bytes(b"\x1b");
        self.expect(repainted, "the frame beneath a dismissed overlay");
    }

    /// Whether the client currently owns the terminal.
    fn is_raw(&self) -> bool {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `tcgetattr` writes exactly one termios into the live pointer.
        let rc = unsafe { libc::tcgetattr(self.slave.as_raw_fd(), termios.as_mut_ptr()) };
        assert_ne!(rc, -1, "tcgetattr failed");
        // SAFETY: the successful call initialized the value.
        let termios = unsafe { termios.assume_init() };
        termios.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG) == 0
    }

    /// One stdio handle onto the slave side.
    fn stdio(&self) -> Stdio {
        Stdio::from(
            self.slave
                .try_clone()
                .expect("the slave descriptor can be duplicated"),
        )
    }
}

/// Reads a pty until every needle is visible, without blocking past
/// [`PATIENCE`]. Returns the visible text either way.
fn read_until(fd: &OwnedFd, needles: &[&str]) -> Result<String, String> {
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
                let visible = visible_text(&seen);
                if needles.iter().all(|needle| visible.contains(needle)) {
                    return Ok(visible);
                }
            }
        }
    }
    Err(visible_text(&seen))
}

/// The characters a terminal would have shown, with control sequences dropped.
///
/// Only enough of a parser to separate text from addressing and styling: CSI and
/// OSC introducers and their terminators, plus the two-byte escapes a renderer
/// emits. Anything it does not recognise stays in the output, so an unexpected
/// sequence shows up in a failure message rather than disappearing from it.
fn visible_text(bytes: &[u8]) -> String {
    let raw = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                // Parameter and intermediate bytes, then one final byte.
                for ch in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC runs to BEL or to the two-character ST.
                while let Some(ch) = chars.next() {
                    if ch == '\u{7}' {
                        break;
                    }
                    if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
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

/// The attached client process under test.
struct AttachedClient {
    child: std::process::Child,
}

impl AttachedClient {
    /// Runs the shipped binary against `socket`, with its stdio on `tty`.
    fn spawn(tty: &Tty, socket: &Path) -> Self {
        Self::spawn_with_term(tty, socket, "xterm-256color")
    }

    /// The same, with an explicit outer-terminal capability baseline.
    fn spawn_with_term(tty: &Tty, socket: &Path, term: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_cloo"))
            .arg("attach")
            .stdin(tty.stdio())
            .stdout(tty.stdio())
            .stderr(tty.stdio())
            .env("CLOO_SOCKET", socket)
            .env("TERM", term)
            .env_remove("COLORTERM")
            .spawn()
            .expect("the cloo binary is built before its integration tests");
        Self { child }
    }

    /// Runs the binary against ordinary named-session discovery.
    fn spawn_named(tty: &Tty, runtime: &Path, session: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_cloo"))
            .arg("attach")
            .arg(session)
            .stdin(tty.stdio())
            .stdout(tty.stdio())
            .stderr(tty.stdio())
            .env("XDG_RUNTIME_DIR", runtime)
            .env_remove("CLOO_SOCKET")
            .env("TERM", "xterm-256color")
            .env_remove("COLORTERM")
            .spawn()
            .expect("the cloo binary is built before its integration tests");
        Self { child }
    }

    /// Detaches with the prefix and asserts the terminal was handed back.
    ///
    /// Every fixture ends here rather than by killing the process, because
    /// "the surface rendered" is only half the claim: a client that leaves a
    /// reporting mode or raw mode behind has broken the shell it was run from.
    fn detach_and_restore(mut self, tty: &Tty) {
        tty.chord(b'd');
        let deadline = Instant::now() + PATIENCE;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("the client stays waitable") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("a wedged client can be stopped");
                let _ = self.child.wait();
                panic!("the attached client did not exit after its detach command");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "cloo attach exited with {status}");
        assert!(
            !tty.is_raw(),
            "the client left the outer terminal in raw mode"
        );
    }
}

/// A daemon on its own runtime thread.
///
/// These fixtures block the test thread on a pty read, so a daemon sharing that
/// thread could not accept a connection or publish damage while one is waiting.
struct ThreadDaemon {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ThreadDaemon {
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

/// The session's half of a config at the fixtures' reference geometry.
fn base() -> PtyConfig {
    PtyConfig::session(Size::new(80, 24))
        .expect("80x24 is a valid size")
        .env("TERM", "xterm-256color")
}

/// A launch running `script` under `sh`, named the way a user would name it.
fn scripted(script: &str, adapter: Option<&str>) -> Launch {
    let mut profile = Profile::generic();
    if let Some(adapter) = adapter {
        profile = profile.adapter(AdapterId::new(adapter).expect("a valid adapter id"));
    }
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

/// Binds a daemon on `socket` and runs it on its own thread until stopped.
fn spawn_daemon_thread(
    socket: PathBuf,
    name: &'static str,
    script: &'static str,
    adapter: Option<&'static str>,
) -> ThreadDaemon {
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
            let mut daemon = Daemon::new(listener, &base(), scripted(script, adapter))
                .expect("the daemon starts")
                .with_session_name(name);
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

/// Reports one advisory adapter state for `pane` on `socket`'s control channel.
///
/// Attention is explicit harness state, never a screen scrape, so the only way
/// to reach the notification card honestly is to be an adapter the pane's
/// profile named — which is what this does, over the real control socket.
fn report_adapter_state(socket: &Path, adapter: &str, pane: PaneId, state: AdapterState) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("the adapter runtime builds");
    runtime.block_on(async {
        let path = control_path_for(socket);
        let stream = tokio::time::timeout(PATIENCE, async {
            loop {
                match UnixStream::connect(&path).await {
                    Ok(stream) => return stream,
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
        let reply: Option<AdapterReply> = conn.recv().await.expect("the control socket answers");
        assert!(
            matches!(reply, Some(AdapterReply::Ready { .. })),
            "expected a ready, got {reply:?}"
        );

        conn.send(&AdapterMessage::Report { pane, state })
            .await
            .expect("the report must send");
        let reply: Option<AdapterReply> = conn.recv().await.expect("every report is answered");
        assert_eq!(
            reply,
            Some(AdapterReply::Applied { pane }),
            "the pane's own profile named this adapter"
        );
    });
}

// ---------------------------------------------------------------------------
// Card 01 — the daily one-pane workspace
// ---------------------------------------------------------------------------

#[test]
fn card_01_one_pane_composes_tab_frame_and_status_rows_on_a_real_terminal() {
    let dir = TempDir::new("card01");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "dev", "printf one-pane-ok; read _", None);
    let tty = Tty::open();
    let client = AttachedClient::spawn(&tty, &socket);

    // The three things card 01 is: a session-aware tab row, a completely framed
    // focused pane whose header names it, and the always-on status row.
    let seen = tty.expect_all(
        &[
            "one-pane-ok",
            " dev ",
            ">1 api",
            "\u{250c}> 1 api",
            "\u{2514}\u{2500}",
            "C-b split % stack \" help ?",
        ],
        "the composed one-pane frame",
    );
    assert!(
        seen.contains("? unknown"),
        "the header states the pane's activity signal; saw:\n{seen}"
    );
    assert!(tty.is_raw(), "the attach loop owns the terminal");

    client.detach_and_restore(&tty);
    daemon.stop();
}

// ---------------------------------------------------------------------------
// Cards 02 and 03 — splits and the nested workspace
// ---------------------------------------------------------------------------

#[test]
fn cards_02_and_03_split_and_nest_through_the_live_keymap() {
    let dir = TempDir::new("cards0203");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "dev", "printf nested-ready; read _", None);
    let tty = Tty::open();
    let client = AttachedClient::spawn(&tty, &socket);
    tty.expect("nested-ready", "the initial workspace");

    // Card 02: one vertical split, driven by the bound chord rather than by a
    // command this fixture sent on the wire itself.
    tty.chord(b'%');
    let split = tty.expect_all(
        &["2 panes", "\u{250c}  1 api", "\u{250c}> 2 api"],
        "the vertical split",
    );
    assert!(
        split.contains("\u{2514}\u{2500}"),
        "both allocations keep a complete frame; saw:\n{split}"
    );

    // Card 03: a horizontal split of the focused pane nests the geometry, and
    // the third pane is headed by its own index rather than sharing one.
    tty.chord(b'"');
    tty.expect_all(
        &[
            "3 panes",
            "\u{250c}  1 api",
            "\u{250c}  2 api",
            "\u{250c}> 3 api",
        ],
        "the nested workspace",
    );

    client.detach_and_restore(&tty);
    daemon.stop();
}

// ---------------------------------------------------------------------------
// Card 04 — the searchable prefix palette
// ---------------------------------------------------------------------------

#[test]
fn card_04_prefix_palette_opens_over_the_frame_and_filters_live_bindings() {
    let dir = TempDir::new("card04");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "dev", "printf palette-ready; read _", None);
    let tty = Tty::open();
    let client = AttachedClient::spawn(&tty, &socket);
    tty.expect("palette-ready", "the initial workspace");

    tty.chord(b'?');
    tty.expect_all(
        &[
            "commands - prefix C-b",
            "  / _",
            "esc close",
            "up/down move",
        ],
        "the command palette",
    );

    // Typing is the palette's own departure from the shared overlay vocabulary:
    // a printable key is query text, not navigation, and no byte of it reaches
    // the pane. The `sh` child echoes nothing back, so the filtered surface
    // arriving is the evidence that the client consumed the keys.
    tty.type_bytes(b"spl");
    tty.expect_all(
        &["  / spl_", "split right", "split-vertical", "split down"],
        "the filtered result list",
    );

    tty.close_overlay("palette-ready");
    client.detach_and_restore(&tty);
    daemon.stop();
}

// ---------------------------------------------------------------------------
// Card 05 — the real session switcher
// ---------------------------------------------------------------------------

#[test]
fn card_05_session_switcher_lists_the_verified_daemon_catalog() {
    let dir = TempDir::new("card05");
    let runtime = dir.0.join("runtime");
    let socket_dir = runtime.join("cloo");
    let main = spawn_daemon_thread(
        socket_dir.join("main.sock"),
        "main",
        "printf main-session; read _",
        None,
    );
    let review = spawn_daemon_thread(
        socket_dir.join("review.sock"),
        "review",
        "printf review-session; read _",
        None,
    );
    let tty = Tty::open();
    let client = AttachedClient::spawn_named(&tty, &runtime, "main");
    tty.expect("main-session", "the initial session");

    // The catalog is the daemons' own answers, not a synthesized current row:
    // the socket this client is viewing is the one labelled attached, and the
    // other daemon appears only because it was verified over its own socket.
    tty.chord(b's');
    tty.expect_all(
        &[
            "sessions",
            "main attached",
            "review",
            "esc close",
            "enter switch",
        ],
        "the verified session catalog",
    );

    tty.close_overlay("main-session");
    client.detach_and_restore(&tty);
    main.stop();
    review.stop();
}

// ---------------------------------------------------------------------------
// Card 06 — the runtime configuration and theme preview
// ---------------------------------------------------------------------------

#[test]
fn card_06_configuration_preview_reports_the_settings_this_client_resolved() {
    let dir = TempDir::new("card06");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "dev", "printf config-ready; read _", None);
    let tty = Tty::open();
    let client = AttachedClient::spawn(&tty, &socket);
    tty.expect("config-ready", "the initial workspace");

    // It reports; it never claims to edit. `read only` occupies the slot the
    // other overlays spend on navigation, and that is the whole contract.
    tty.chord(b',');
    tty.expect_all(
        &[
            "configuration",
            "theme  storm",
            "focus  dim unfocused",
            "status minimal",
            "keys   C-b",
            "themes",
            "gruvbox",
            "nord",
            "preview",
            "1 focused",
            "2 unfocused",
            "esc close read only",
        ],
        "the configuration surface",
    );

    tty.close_overlay("config-ready");
    client.detach_and_restore(&tty);
    daemon.stop();
}

// ---------------------------------------------------------------------------
// Card 07 — the attention notification
// ---------------------------------------------------------------------------

#[test]
fn card_07_an_adapter_report_raises_a_live_notice_and_an_openable_queue() {
    let dir = TempDir::new("card07");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(
        socket.clone(),
        "dev",
        "printf attention-ready; read _",
        Some("my-adapter"),
    );
    let tty = Tty::open();
    let client = AttachedClient::spawn(&tty, &socket);
    tty.expect("attention-ready", "the initial workspace");

    // The daemon's first pane is always id 1, and its profile named the adapter
    // now reporting about it.
    report_adapter_state(
        &socket,
        "my-adapter",
        PaneId::new(1),
        AdapterState::NeedsInput,
    );
    // A notice floats over the frame, the status row's tally becomes explicit,
    // and the pane's own header says the same thing. The state is a glyph as
    // well as a label everywhere it appears.
    tty.expect_all(
        &["api ! needs input", "1!"],
        "the attention notice and its status tally",
    );

    // The same projection is what the queue surface lists, so the notice and the
    // overlay cannot disagree about what is waiting.
    tty.chord(b'!');
    tty.expect_all(
        &[
            "attention",
            "1 api ! needs input",
            "esc close",
            "enter focus",
            "a ack",
        ],
        "the attention queue overlay",
    );

    tty.close_overlay("attention-ready");
    client.detach_and_restore(&tty);
    daemon.stop();
}

// ---------------------------------------------------------------------------
// Card 08 — the active pane resize
// ---------------------------------------------------------------------------

#[test]
fn card_08_a_keyboard_resize_lights_the_divider_without_mouse_reporting() {
    let dir = TempDir::new("card08");
    let socket = dir.socket();
    let daemon = spawn_daemon_thread(socket.clone(), "dev", "printf resize-ready; read _", None);
    let tty = Tty::open();
    // vt100 negotiates no SGR mouse reporting: card 08 must be reachable from
    // the keyboard alone, or a terminal without a pointer loses an operation
    // rather than a convenience.
    let client = AttachedClient::spawn_with_term(&tty, &socket, "vt100");
    tty.expect("resize-ready", "the initial workspace");

    tty.chord(b'%');
    tty.expect_all(&["2 panes", "\u{250c}> 2 api"], "the split workspace");

    tty.type_bytes(&[PREFIX, 0x1b, b'[', b'D']);
    let resizing = tty.expect("resize \u{b7} ratio 0.", "the active resize affordance");
    assert!(
        resizing.contains("resize \u{b7} ratio 0.4"),
        "the label reports the ratio the framed allocations reconstruct; saw:\n{resizing}"
    );

    client.detach_and_restore(&tty);
    daemon.stop();
}
