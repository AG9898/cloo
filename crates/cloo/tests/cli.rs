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
fn the_childs_exit_code_becomes_cloos_exit_code() {
    let tty = open_tty();
    let mut child = spawn_on(&tty, &["sh", "-c", "exit 7"]);
    let status = child.wait().expect("the child is reaped");
    assert_eq!(status.code(), Some(7));
}
