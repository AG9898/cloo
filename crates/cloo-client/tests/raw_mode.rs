//! Raw-mode behaviour against a real terminal.
//!
//! Everything here needs an actual tty, so it opens a pseudoterminal pair and
//! drives the slave side — the same reason `cloo-server`'s PTY coverage lives in
//! `tests/` rather than in a unit module. The pure `termios` transformation is
//! unit tested in `src/raw_mode.rs` instead.
//!
//! The restore slot is process-global, so every test that arms it takes
//! [`GUARD`] first. Rust runs integration tests in parallel threads within one
//! binary, and two live guards would legitimately collide.

use std::os::fd::{AsFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, MutexGuard};

use cloo_client::raw_mode::{RawMode, RawModeError};

/// Serializes access to the process-global restore slot.
static GUARD: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A pseudoterminal pair. The slave is a real tty and stands in for the user's
/// terminal; the master is held only to keep the pair open.
struct TtyPair {
    _master: OwnedFd,
    slave: OwnedFd,
}

fn open_tty() -> TtyPair {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: `openpty` writes one descriptor into each valid out parameter;
    // the termios and winsize pointers are null, meaning "use the defaults".
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
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
        TtyPair {
            _master: OwnedFd::from_raw_fd(master),
            slave: OwnedFd::from_raw_fd(slave),
        }
    }
}

/// Reads a descriptor's current attributes.
fn attributes(fd: &OwnedFd) -> libc::termios {
    use std::os::fd::AsRawFd;
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is a live descriptor and `tcgetattr` writes exactly one
    // `termios` into the valid pointer.
    let rc = unsafe { libc::tcgetattr(fd.as_raw_fd(), termios.as_mut_ptr()) };
    assert_ne!(
        rc,
        -1,
        "tcgetattr failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the call succeeded, so the value is initialized.
    unsafe { termios.assume_init() }
}

fn is_raw(termios: &libc::termios) -> bool {
    termios.c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG) == 0
}

#[test]
fn entering_raw_mode_changes_the_terminal_and_dropping_puts_it_back() {
    let _lock = exclusive();
    let tty = open_tty();
    let before = attributes(&tty.slave);
    assert!(!is_raw(&before), "a fresh pty starts cooked");

    {
        let guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
        assert!(guard.is_active());
        assert!(is_raw(&attributes(&tty.slave)), "raw mode took effect");
    }

    let after = attributes(&tty.slave);
    assert!(!is_raw(&after), "drop restored the terminal");
    assert_eq!(after.c_lflag, before.c_lflag, "the exact flags came back");
    assert_eq!(after.c_iflag, before.c_iflag);
    assert_eq!(after.c_oflag, before.c_oflag);
}

#[test]
fn an_explicit_restore_reports_success_and_makes_drop_a_no_op() {
    let _lock = exclusive();
    let tty = open_tty();
    let before = attributes(&tty.slave);

    let guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
    guard.restore().expect("restoring a live pty cannot fail");

    assert!(!is_raw(&attributes(&tty.slave)));
    assert_eq!(attributes(&tty.slave).c_lflag, before.c_lflag);

    // The slot is free again, which is only true if `restore` disarmed it.
    let second = RawMode::enter(tty.slave.as_fd()).expect("the slot was released");
    drop(second);
}

#[test]
fn an_unwinding_error_path_still_restores() {
    let _lock = exclusive();
    let tty = open_tty();
    let before = attributes(&tty.slave);

    // A guard held across a `?`-style early return is dropped by the unwind or
    // by the return, and either way the terminal has to come back.
    let result: Result<(), &str> = (|| {
        let _guard = RawMode::enter(tty.slave.as_fd()).map_err(|_| "enter failed")?;
        assert!(is_raw(&attributes(&tty.slave)));
        Err("the client failed mid-session")
    })();

    assert_eq!(result, Err("the client failed mid-session"));
    assert!(!is_raw(&attributes(&tty.slave)), "the error path restored");
    assert_eq!(attributes(&tty.slave).c_lflag, before.c_lflag);
}

#[test]
fn a_panic_restores_before_the_message_is_printed() {
    let _lock = exclusive();
    let tty = open_tty();
    let before = attributes(&tty.slave);

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
        assert!(is_raw(&attributes(&tty.slave)));
        panic!("the client panicked with the terminal raw");
    }));

    assert!(panicked.is_err(), "the panic propagated");
    assert!(!is_raw(&attributes(&tty.slave)), "the panic hook restored");
    assert_eq!(attributes(&tty.slave).c_lflag, before.c_lflag);
}

#[test]
fn a_second_guard_is_refused_rather_than_overwriting_the_saved_state() {
    let _lock = exclusive();
    let first_tty = open_tty();
    let second_tty = open_tty();
    let pristine = attributes(&second_tty.slave);

    let _first = RawMode::enter(first_tty.slave.as_fd()).expect("a pty slave is a terminal");
    let second = RawMode::enter(second_tty.slave.as_fd());

    assert!(
        matches!(second, Err(RawModeError::AlreadyActive)),
        "only one guard may own the restore slot"
    );
    assert_eq!(
        attributes(&second_tty.slave).c_lflag,
        pristine.c_lflag,
        "the refused terminal was never touched"
    );
}

/// Everything the terminal has been sent since the pair was opened.
fn drain(pair: &TtyPair) -> Vec<u8> {
    use std::os::fd::AsRawFd;
    let fd = pair._master.as_raw_fd();
    // SAFETY: `fd` is a live descriptor; `F_SETFL` only changes its flags.
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    let mut buf = [0_u8; 256];
    // SAFETY: `buf` is live local storage of exactly the length passed.
    let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if read <= 0 {
        return Vec::new();
    }
    #[allow(clippy::cast_sign_loss)]
    buf[..read as usize].to_vec()
}

#[test]
fn a_registered_reset_is_written_on_the_normal_restore_path() {
    let _lock = exclusive();
    let tty = open_tty();
    let reset = b"\x1b[?1006l\x1b[?1000l";

    let guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
    guard.on_restore(reset).expect("the sequence fits");
    guard.restore().expect("restoring a live pty cannot fail");

    assert_eq!(
        drain(&tty),
        reset,
        "a reporting mode left on outlives cloo and confuses the user's shell"
    );
}

#[test]
fn a_registered_reset_is_written_on_the_panic_path_too() {
    let _lock = exclusive();
    let tty = open_tty();
    let reset = b"\x1b[?2004l";

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
        guard.on_restore(reset).expect("the sequence fits");
        panic!("the client panicked with bracketed paste on");
    }));

    assert!(panicked.is_err(), "the panic propagated");
    assert_eq!(drain(&tty), reset, "the panic hook wrote the reset");
    assert!(!is_raw(&attributes(&tty.slave)), "and still restored");
}

#[test]
fn a_restored_guard_leaves_no_reset_behind_for_the_next_one() {
    let _lock = exclusive();
    let tty = open_tty();

    let first = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");
    first.on_restore(b"\x1b[?1004l").expect("the sequence fits");
    first.restore().expect("restoring a live pty cannot fail");
    let _ = drain(&tty);

    let second = RawMode::enter(tty.slave.as_fd()).expect("the slot was released");
    second.restore().expect("restoring a live pty cannot fail");
    assert!(
        drain(&tty).is_empty(),
        "a guard that registered nothing must write nothing"
    );
}

#[test]
fn a_reset_that_does_not_fit_is_refused_rather_than_truncated() {
    let _lock = exclusive();
    let tty = open_tty();
    let guard = RawMode::enter(tty.slave.as_fd()).expect("a pty slave is a terminal");

    let oversized = vec![b'x'; cloo_client::raw_mode::MAX_RESET_LEN + 1];
    assert!(matches!(
        guard.on_restore(&oversized),
        Err(RawModeError::ResetTooLong { .. })
    ));

    guard.restore().expect("restoring a live pty cannot fail");
    assert!(
        drain(&tty).is_empty(),
        "half a sequence would be printed on the user's screen"
    );
}

#[test]
fn a_pipe_is_refused_as_not_a_terminal() {
    let _lock = exclusive();
    let mut fds: [RawFd; 2] = [-1, -1];
    // SAFETY: `pipe` writes two descriptors into the valid two-element array.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_ne!(rc, -1, "pipe failed: {}", std::io::Error::last_os_error());
    // SAFETY: `pipe` succeeded, so both descriptors are open and unowned.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    assert!(matches!(
        RawMode::enter(read.as_fd()),
        Err(RawModeError::NotATerminal)
    ));
    drop(write);
}
