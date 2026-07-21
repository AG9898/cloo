//! The binary's command line and the one-pane smoke path, end to end.
//!
//! The smoke test runs the real binary with its stdio on a pseudoterminal
//! slave, which is the only way to exercise the path at all: cloo refuses to
//! start without a terminal, on purpose. The master side then stands in for the
//! user's screen, and asserting on the bytes that arrive there is asserting on
//! what the user would actually see.
//!
//! Nothing here asserts an exact frame — the renderer's own tests do that
//! byte for byte. These assert the wiring: a child's output reaches the
//! screen, typed input reaches the child, and the terminal is handed back.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a smoke test waits for expected output before failing.
const TIMEOUT: Duration = Duration::from_secs(20);

fn cloo() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cloo"))
}

/// A pseudoterminal pair standing in for the user's terminal.
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
    // The termios pointer is null ("use the defaults") and the winsize pointer
    // refers to a live local that outlives the call.
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
    // SAFETY: `openpty` succeeded, so both descriptors are open and unowned.
    unsafe {
        Tty {
            master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    }
}

/// Reads from `fd` until `needle` appears or [`TIMEOUT`] elapses.
///
/// Returns everything read, so a failing assertion can show what did arrive.
///
/// Readiness is polled with the time actually remaining rather than read
/// blindly: a terminal that simply goes quiet — which is precisely what a
/// broken resize or a dropped frame looks like — would otherwise block a
/// blocking read forever and turn a clean failure into a hung suite.
fn read_until(fd: &OwnedFd, needle: &str) -> Result<String, String> {
    let mut file = unsafe {
        // SAFETY: the descriptor is owned by the caller and outlives the
        // `ManuallyDrop` wrapper, which never closes it.
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(fd.as_raw_fd()))
    };
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    let mut buf = [0_u8; 4096];
    while Instant::now() < deadline {
        if !readable_before(fd, deadline) {
            break;
        }
        match file.read(&mut buf) {
            // EOF, or the child closed the pty: nothing more will arrive.
            Ok(0) => break,
            Ok(read) => {
                seen.extend_from_slice(&buf[..read]);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return Ok(String::from_utf8_lossy(&seen).into_owned());
                }
            }
            // The slave side is gone. On Linux that is reported as EIO.
            Err(_) => break,
        }
    }
    Err(String::from_utf8_lossy(&seen).into_owned())
}

/// Waits for `fd` to have something to read, giving up at `deadline`.
///
/// An error is reported as readable so the caller's `read` produces the real
/// reason — an `EIO` from a closed slave is an ordinary end of output here.
fn readable_before(fd: &OwnedFd, deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: the pointer refers to one live `pollfd`, and the count matches.
    let rc = unsafe { libc::poll(&raw mut poll, 1, millis) };
    rc != 0
}

/// Changes `fd`'s terminal geometry, as a window manager would.
fn set_winsize(fd: &OwnedFd, cols: u16, rows: u16) {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is a live descriptor and `TIOCSWINSZ` reads exactly one
    // `winsize` through the pointer, which refers to a live local.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize) };
    assert_ne!(rc, -1, "TIOCSWINSZ failed");
}

/// Is `fd`'s terminal in raw mode?
fn is_raw(fd: &OwnedFd) -> bool {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is a live descriptor and `tcgetattr` writes exactly one
    // `termios` into the valid pointer.
    let rc = unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) };
    assert_ne!(rc, -1, "tcgetattr failed");
    // SAFETY: the call succeeded, so the value is initialized.
    let termios = unsafe { termios.assume_init() };
    termios.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG) == 0
}

/// Runs the binary with its stdio on `tty`'s slave side.
fn spawn_on(tty: &Tty, args: &[&str]) -> std::process::Child {
    let stdio = || {
        Stdio::from(
            tty.slave
                .try_clone()
                .expect("the slave descriptor can be duplicated"),
        )
    };
    cloo()
        .args(args)
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio())
        // A fixed, modest capability set keeps the frame independent of
        // whatever terminal the test suite happens to run under.
        .env("TERM", "xterm-256color")
        .env_remove("COLORTERM")
        .spawn()
        .expect("the cloo binary is built before its integration tests")
}

// -- The command line ------------------------------------------------------

#[test]
fn version_prints_the_crate_version_and_succeeds() {
    let out = cloo().arg("--version").output().expect("cloo runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("cloo {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_documents_running_a_program() {
    let out = cloo().arg("--help").output().expect("cloo runs");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"));
    assert!(stdout.contains("<program>"), "got:\n{stdout}");
}

#[test]
fn an_unknown_flag_is_a_usage_error_and_is_never_executed() {
    let out = cloo().arg("--nonesuch").output().expect("cloo runs");
    assert_eq!(out.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unrecognized option"), "got:\n{stderr}");
}

#[test]
fn help_documents_the_launch_options_and_the_built_in_profiles() {
    let out = cloo().arg("--help").output().expect("cloo runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "--profile",
        "--name",
        "--task",
        "--cwd",
        "generic",
        "codex",
        "claude",
    ] {
        assert!(
            stdout.contains(expected),
            "{expected} missing from:\n{stdout}"
        );
    }
}

#[test]
fn an_unknown_profile_is_refused_before_the_terminal_is_touched() {
    // A usage error, not a launch failure: nothing was spawned, and the message
    // names the profiles that do exist.
    let out = cloo()
        .args(["--profile", "codx"])
        .stdin(Stdio::piped())
        .output()
        .expect("cloo runs");
    assert_eq!(out.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("codx"), "got:\n{stderr}");
    assert!(stderr.contains("codex"), "got:\n{stderr}");
}

#[test]
fn a_task_label_that_could_repaint_the_chrome_is_refused() {
    let out = cloo()
        .args(["--task", "esc\u{1b}[31m", "true"])
        .stdin(Stdio::piped())
        .output()
        .expect("cloo runs");
    assert_eq!(out.status.code(), Some(64));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("task label"),
        "the message must name the field"
    );
}

#[test]
fn a_profile_whose_program_is_missing_says_so_and_names_it() {
    // The acceptance criterion for M2-06: a launch-time failure, reported in
    // terms a user can act on rather than as a bare errno.
    let tty = open_tty();
    let mut child = spawn_on(&tty, &["cloo-no-such-program-exists"]);
    let status = child.wait().expect("cloo exits");
    assert_eq!(status.code(), Some(125), "a cloo failure, not the child's");

    let seen = read_until(&tty.master, "cloo-no-such-program-exists")
        .unwrap_or_else(|seen| panic!("the failure never named the program; saw:\n{seen}"));
    assert!(seen.contains("PATH"), "got:\n{seen}");
}

#[test]
fn without_a_terminal_cloo_refuses_rather_than_spawning_a_child() {
    let out = cloo()
        .arg("true")
        .stdin(Stdio::piped())
        .output()
        .expect("cloo runs");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("must be run from a terminal"),
        "got:\n{stderr}"
    );
}

// -- The smoke path --------------------------------------------------------

#[test]
fn a_child_program_is_spawned_and_its_output_is_rendered() {
    let tty = open_tty();
    let mut child = spawn_on(&tty, &["sh", "-c", "printf cloo-smoke-ok; sleep 1"]);

    let seen = read_until(&tty.master, "cloo-smoke-ok")
        .unwrap_or_else(|seen| panic!("the child's output never reached the screen; saw:\n{seen}"));

    // Rendered, not forwarded: the text arrives inside a frame the renderer
    // built, which always begins by hiding the cursor and clearing.
    assert!(
        seen.contains("\x1b[?25l\x1b[H\x1b[2J"),
        "output was not drawn as a frame; saw:\n{seen}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn typed_input_reaches_the_child_and_the_terminal_is_handed_back() {
    let tty = open_tty();
    // `read` needs a line; the shell is in raw mode, so the client forwards the
    // carriage return exactly as typed and the child's line discipline is what
    // turns it into a newline.
    let mut child = spawn_on(
        &tty,
        &["sh", "-c", "read line; printf 'echoed:%s' \"$line\""],
    );

    // Wait for the first frame, which means the child is up and reading.
    read_until(&tty.master, "\x1b[?25l")
        .unwrap_or_else(|seen| panic!("no frame was ever drawn; saw:\n{seen}"));
    assert!(is_raw(&tty.slave), "the terminal is raw while cloo runs");

    let mut master = unsafe {
        // SAFETY: `tty.master` outlives this wrapper, which never closes it.
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(tty.master.as_raw_fd()))
    };
    master
        .write_all(b"typed\r")
        .expect("the master side accepts input");

    let seen = read_until(&tty.master, "echoed:typed")
        .unwrap_or_else(|seen| panic!("input never reached the child; saw:\n{seen}"));
    assert!(seen.contains("echoed:typed"));

    let status = child.wait().expect("the child is reaped");
    assert!(status.success(), "cloo exited with {status}");
    assert!(
        !is_raw(&tty.slave),
        "cloo left the terminal raw on the way out"
    );
}

#[test]
fn a_sigwinch_resizes_the_pane_all_the_way_down_to_the_child() {
    let tty = open_tty();
    // The child asks the *inner* pty what shape it is, which nothing but a
    // `TIOCSWINSZ` on that pty's master can have changed. It reports on a loop
    // rather than on demand so the assertion does not depend on a keystroke and
    // a signal being handled in a particular order.
    let mut child = spawn_on(
        &tty,
        &["sh", "-c", "while :; do stty size; sleep 0.1; done"],
    );

    // "rows cols" at the size the pane started at, which is also proof the
    // child is up and reporting.
    read_until(&tty.master, "24 80")
        .unwrap_or_else(|seen| panic!("the child never reported its size; saw:\n{seen}"));

    // The outer terminal changes shape, then says so. A window manager does
    // both; here the geometry has to land first, since the signal carries no
    // size and cloo answers it with a `TIOCGWINSZ`.
    set_winsize(&tty.master, 100, 40);
    // SAFETY: `child` has not been reaped, so its pid is still its own.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGWINCH) };
    assert_ne!(rc, -1, "SIGWINCH could not be delivered");

    let seen = read_until(&tty.master, "40 100")
        .unwrap_or_else(|seen| panic!("the resize never reached the child's pty; saw:\n{seen}"));
    assert!(seen.contains("40 100"));

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_terminating_signal_still_hands_the_terminal_back() {
    use std::os::unix::process::ExitStatusExt;

    let tty = open_tty();
    // A child that outlives the signal, so what restores the terminal is the
    // handler and not an ordinary exit path. It self-terminates well inside the
    // timeout, since the signal leaves cloo no chance to reap it.
    let mut child = spawn_on(&tty, &["sleep", "30"]);

    read_until(&tty.master, "\x1b[?25l")
        .unwrap_or_else(|seen| panic!("no frame was ever drawn; saw:\n{seen}"));
    assert!(is_raw(&tty.slave), "the terminal is raw while cloo runs");

    // SAFETY: `child` has not been reaped, so its pid is still its own.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_ne!(rc, -1, "SIGTERM could not be delivered");

    let status = child.wait().expect("the child is reaped");
    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "the handler must re-raise, so the shell sees a signalled child"
    );
    assert!(
        !is_raw(&tty.slave),
        "cloo left the terminal raw after a signal"
    );
}

#[test]
fn the_childs_exit_code_becomes_cloos_exit_code() {
    let tty = open_tty();
    let mut child = spawn_on(&tty, &["sh", "-c", "exit 7"]);
    let status = child.wait().expect("the child is reaped");
    assert_eq!(status.code(), Some(7));
}
