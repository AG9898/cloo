//! Raw-mode entry and guaranteed restoration of the user's terminal.
//!
//! A client that leaves the outer terminal in raw mode is a critical bug: the
//! shell the user returns to has no echo, no line editing, and no `Ctrl-C`.
//! Restoration therefore does not depend on the client reaching its normal exit
//! path. Four routes are covered, and all four run the same restore:
//!
//! | Path | Mechanism |
//! |---|---|
//! | Normal | [`RawMode::restore`], or [`Drop`] if the caller never calls it |
//! | Error | [`Drop`] while an error unwinds out of the client |
//! | Panic | a panic hook installed on first entry, chained to the previous hook |
//! | Signal | `SIGINT`, `SIGTERM`, `SIGHUP`, and `SIGQUIT` handlers that restore, then re-raise |
//!
//! The same four paths also turn off any reporting modes the client asked the
//! outer terminal for, registered with [`RawMode::on_restore`]. A terminal left
//! reporting mouse motion into a shell that knows nothing about it is the same
//! class of bug as one left in raw mode, and it deserves the same guarantee.
//!
//! The panic hook and the signal handlers cannot borrow the guard, so the saved
//! `termios` and that reset sequence live in a process-global slot that the
//! guard arms on entry and disarms on restore. The slot is written with plain atomics and read by a
//! signal handler using only `tcsetattr`, which POSIX lists as
//! async-signal-safe — no allocation, no locking, and no `Mutex`.
//!
//! ```no_run
//! use cloo_client::raw_mode::RawMode;
//!
//! # fn example() -> Result<(), cloo_client::raw_mode::RawModeError> {
//! let guard = RawMode::stdin()?;
//! // ... run the client ...
//! guard.restore()?;
//! # Ok(())
//! # }
//! ```

use std::cell::UnsafeCell;
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::Once;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicUsize, Ordering};

/// Signals that would otherwise kill the client before [`Drop`] could run.
///
/// `SIGKILL` and `SIGSTOP` are deliberately absent — they cannot be caught, and
/// a terminal left raw by `SIGKILL` is not something a client can prevent.
const RESTORE_SIGNALS: [libc::c_int; 4] =
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];

/// Everything raw-mode entry can refuse to do.
#[derive(Debug)]
pub enum RawModeError {
    /// The descriptor is not a terminal, so there is no `termios` to change.
    /// Typically means output was redirected to a file or a pipe.
    NotATerminal,
    /// Raw mode is already active somewhere in this process. Only one guard may
    /// be armed at a time, because there is only one global restore slot.
    AlreadyActive,
    /// `tcgetattr` failed, so no original state was captured. Nothing is
    /// changed when this is returned.
    Get(io::Error),
    /// `tcsetattr` failed. On entry the terminal is unchanged; on restore it
    /// may still be raw, which is why the error is surfaced rather than
    /// swallowed.
    Set(io::Error),
    /// The reset sequence handed to [`RawMode::on_restore`] does not fit in the
    /// restore slot. Nothing was stored.
    ResetTooLong {
        /// How long the sequence was.
        len: usize,
        /// How long it may be.
        max: usize,
    },
}

impl fmt::Display for RawModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATerminal => write!(f, "not a terminal: cannot enter raw mode"),
            Self::AlreadyActive => write!(f, "raw mode is already active in this process"),
            Self::Get(e) => write!(f, "could not read the terminal attributes: {e}"),
            Self::Set(e) => write!(f, "could not write the terminal attributes: {e}"),
            Self::ResetTooLong { len, max } => write!(
                f,
                "the terminal reset sequence is {len} bytes, and at most {max} fit"
            ),
        }
    }
}

impl std::error::Error for RawModeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Get(e) | Self::Set(e) => Some(e),
            Self::NotATerminal | Self::AlreadyActive | Self::ResetTooLong { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The global restore slot
// ---------------------------------------------------------------------------

/// Nothing to restore.
const SLOT_IDLE: u8 = 0;
/// A guard is mid-way through writing the saved state. A handler that observes
/// this must not read the payload — it is not complete yet.
const SLOT_ARMING: u8 = 1;
/// The payload is complete and a handler may restore from it.
const SLOT_ARMED: u8 = 2;

/// How many bytes of reset sequence the slot can carry.
///
/// Generous for the handful of private mode resets a client turns on; a longer
/// one is refused rather than truncated, since half a sequence would be printed
/// on the user's screen.
pub const MAX_RESET_LEN: usize = 128;

/// Process-global saved terminal state, readable from a signal handler.
struct RestoreSlot {
    state: AtomicU8,
    fd: AtomicI32,
    saved: UnsafeCell<MaybeUninit<libc::termios>>,
    /// Escape sequences written before the `termios` is put back — the resets
    /// for whatever reporting modes the client turned on. Published length-last,
    /// so a handler either sees the whole thing or nothing.
    reset: UnsafeCell<[u8; MAX_RESET_LEN]>,
    reset_len: AtomicUsize,
}

// SAFETY: `saved` is only written by a thread that has won the `IDLE -> ARMING`
// compare-exchange, and it is only read once the state has been released as
// `ARMED`. The acquire/release pairing on `state` orders the payload write
// before any reader can observe `ARMED`, so no reader ever sees a torn or
// uninitialized `termios`.
unsafe impl Sync for RestoreSlot {}

static RESTORE: RestoreSlot = RestoreSlot {
    state: AtomicU8::new(SLOT_IDLE),
    fd: AtomicI32::new(-1),
    saved: UnsafeCell::new(MaybeUninit::uninit()),
    reset: UnsafeCell::new([0; MAX_RESET_LEN]),
    reset_len: AtomicUsize::new(0),
};

impl RestoreSlot {
    /// Publishes `saved` for `fd`, or returns `false` if a guard already owns
    /// the slot.
    fn arm(&self, fd: RawFd, saved: libc::termios) -> bool {
        if self
            .state
            .compare_exchange(SLOT_IDLE, SLOT_ARMING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        // SAFETY: winning the compare-exchange above makes this thread the sole
        // writer until it publishes `ARMED`, and no reader touches the payload
        // in the `ARMING` state.
        unsafe { (*self.saved.get()).write(saved) };
        self.fd.store(fd, Ordering::Relaxed);
        self.state.store(SLOT_ARMED, Ordering::Release);
        true
    }

    /// Takes the slot out of service. Idempotent.
    fn disarm(&self) {
        self.reset_len.store(0, Ordering::Release);
        self.state.store(SLOT_IDLE, Ordering::Release);
    }

    /// Publishes the escape sequence to write before restoring the `termios`.
    ///
    /// Returns `false` if `bytes` does not fit, in which case nothing is stored:
    /// a truncated reset is worse than none, because half a sequence lands on
    /// the user's screen as text.
    fn set_reset(&self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_RESET_LEN || self.state.load(Ordering::Acquire) != SLOT_ARMED {
            return false;
        }
        // Length last: a handler that fires mid-copy reads the old length and
        // writes the old sequence, never a half-written one.
        self.reset_len.store(0, Ordering::Release);
        // SAFETY: only a guard-owning thread calls this, and the only reader is
        // a handler gated on `reset_len`, which is still zero here.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.reset.get().cast(), bytes.len())
        };
        self.reset_len.store(bytes.len(), Ordering::Release);
        true
    }

    /// Writes the stored reset sequence, if there is one.
    ///
    /// Async-signal-safe: `write` is on the POSIX list, and a short write is
    /// retried rather than allocating anything to track it.
    fn write_reset(&self) {
        // Taken rather than read: a panic unwinds through the hook *and* then
        // drops the guard, and a mode reset written twice is at best noise on
        // the wire and at worst a sequence a terminal reacts to differently the
        // second time. The swap is lock-free, so it stays handler-safe.
        let len = self.reset_len.swap(0, Ordering::AcqRel);
        if len == 0 {
            return;
        }
        let fd = self.fd.load(Ordering::Relaxed);
        let mut written = 0;
        while written < len {
            // SAFETY: `reset` holds `MAX_RESET_LEN` initialized bytes and `len`
            // is never larger; the pointer is valid for the whole span and
            // `write` does not retain it.
            let rc = unsafe {
                libc::write(
                    fd,
                    self.reset.get().cast::<u8>().add(written).cast(),
                    len - written,
                )
            };
            match rc {
                // The terminal is gone or refusing. Nothing left to do about it,
                // and the `termios` restore below is the more important half.
                -1 | 0 => return,
                #[allow(clippy::cast_sign_loss)]
                progressed => written += progressed as usize,
            }
        }
    }

    /// Restores the saved attributes if the slot is armed.
    ///
    /// Safe to call from a signal handler: the only libc call it makes is
    /// `tcsetattr`, which POSIX lists as async-signal-safe, and it allocates
    /// nothing.
    fn restore(&self) -> Result<(), io::Error> {
        if self.state.load(Ordering::Acquire) != SLOT_ARMED {
            return Ok(());
        }
        // Reporting modes first: they were turned on after raw mode was entered,
        // and they are turned off before it is left.
        self.write_reset();
        let fd = self.fd.load(Ordering::Relaxed);
        // SAFETY: the state is `ARMED`, which is only published after the
        // payload has been fully written, so the pointer refers to an
        // initialized `termios`. `tcsetattr` reads it and does not retain it.
        let rc = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, (*self.saved.get()).as_ptr()) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Panic and signal hooks
// ---------------------------------------------------------------------------

static HOOKS: Once = Once::new();

/// Restores the terminal, then dies with the default disposition for `signum`.
///
/// Re-raising rather than calling `exit` preserves the wait status a parent
/// shell expects from a signalled child.
extern "C" fn handle_signal(signum: libc::c_int) {
    let _ = RESTORE.restore();
    // SAFETY: `signal` and `raise` are both async-signal-safe. Restoring the
    // default disposition before re-raising guarantees the process actually
    // terminates instead of re-entering this handler.
    unsafe {
        libc::signal(signum, libc::SIG_DFL);
        libc::raise(signum);
    }
}

/// Installs the panic hook and the signal handlers exactly once per process.
fn install_exit_hooks() {
    HOOKS.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Restore first: the default hook writes the panic message to
            // stderr, and a raw terminal renders it as a staircase.
            let _ = RESTORE.restore();
            previous(info);
        }));

        for signum in RESTORE_SIGNALS {
            // SAFETY: `action` is a fully initialized `sigaction` whose handler
            // field holds a valid `extern "C" fn(c_int)`. The old action is
            // discarded via a null pointer, which `sigaction` documents as
            // "do not report the previous action".
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&raw mut action.sa_mask);
                action.sa_flags = libc::SA_RESTART;
                libc::sigaction(signum, &raw const action, std::ptr::null_mut());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

/// An active raw-mode session on one terminal descriptor.
///
/// Restoration is by ownership, matching the PTY layer: dropping the guard puts
/// the terminal back. [`restore`](Self::restore) exists only so a caller that
/// wants to *report* a failed restore can, rather than having it swallowed in
/// `Drop`.
#[derive(Debug)]
pub struct RawMode {
    fd: RawFd,
    /// Cleared once the terminal has been put back, so `Drop` after an explicit
    /// [`restore`](Self::restore) does nothing.
    active: bool,
}

impl RawMode {
    /// Puts `fd` into raw mode and arms every restore path.
    ///
    /// # Errors
    ///
    /// Returns [`RawModeError::NotATerminal`] if `fd` is not a tty,
    /// [`RawModeError::AlreadyActive`] if another guard is live in this
    /// process, and [`RawModeError::Get`] or [`RawModeError::Set`] if the
    /// `termios` calls failed. The terminal is left unchanged in every case.
    pub fn enter(fd: BorrowedFd<'_>) -> Result<Self, RawModeError> {
        let fd = fd.as_raw_fd();

        // SAFETY: `fd` is a valid borrowed descriptor for the duration of the
        // call, and `isatty` only inspects it.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(RawModeError::NotATerminal);
        }

        let original = get_termios(fd)?;
        if !RESTORE.arm(fd, original) {
            return Err(RawModeError::AlreadyActive);
        }
        // The hooks are only useful once something is armed, and installing
        // them here keeps a client that never enters raw mode from replacing
        // the process's panic hook.
        install_exit_hooks();

        if let Err(err) = set_termios(fd, &raw_termios(original)) {
            RESTORE.disarm();
            return Err(err);
        }

        Ok(Self { fd, active: true })
    }

    /// Puts the process's standard input into raw mode.
    ///
    /// # Errors
    ///
    /// See [`enter`](Self::enter).
    pub fn stdin() -> Result<Self, RawModeError> {
        let stdin = std::io::stdin();
        Self::enter(stdin.as_fd_ref())
    }

    /// Registers an escape sequence to write on every restore path.
    ///
    /// This is how the reporting modes from
    /// [`OuterModes`](crate::input::OuterModes) get turned off on a panic or a
    /// signal, not only on the normal exit: the guard already owns the four
    /// paths, and a terminal left reporting mouse motion after cloo dies is the
    /// same class of bug as one left in raw mode. Call it *after* the modes have
    /// been enabled, with exactly the bytes that undo them.
    ///
    /// # Errors
    ///
    /// Returns [`RawModeError::ResetTooLong`] if the sequence does not fit in
    /// [`MAX_RESET_LEN`]. Nothing is stored in that case — a truncated reset
    /// would print itself on the user's screen.
    pub fn on_restore(&self, bytes: &[u8]) -> Result<(), RawModeError> {
        if RESTORE.set_reset(bytes) {
            return Ok(());
        }
        Err(RawModeError::ResetTooLong {
            len: bytes.len(),
            max: MAX_RESET_LEN,
        })
    }

    /// Restores the terminal and consumes the guard, surfacing any failure.
    ///
    /// # Errors
    ///
    /// Returns [`RawModeError::Set`] if `tcsetattr` failed. The terminal is
    /// still raw in that case and there is nothing further the client can do
    /// about it, which is exactly why the caller is told.
    pub fn restore(mut self) -> Result<(), RawModeError> {
        self.restore_in_place()
    }

    /// Whether this guard still owns the terminal's original state.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The descriptor this guard governs.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    fn restore_in_place(&mut self) -> Result<(), RawModeError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let result = RESTORE.restore().map_err(RawModeError::Set);
        RESTORE.disarm();
        result
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // The error path is the one case where nothing can be reported: this
        // may already be running during an unwind. `restore` exists for callers
        // that want to know.
        let _ = self.restore_in_place();
    }
}

/// Borrows a descriptor from `Stdin` without going through `AsFd`'s trait
/// import at every call site.
trait AsFdRef {
    fn as_fd_ref(&self) -> BorrowedFd<'_>;
}

impl AsFdRef for std::io::Stdin {
    fn as_fd_ref(&self) -> BorrowedFd<'_> {
        // SAFETY: descriptor 0 is open for the lifetime of the process in every
        // context a client runs in, and the borrow is tied to the `Stdin` lock
        // handle rather than escaping.
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

/// Reads the current terminal attributes.
fn get_termios(fd: RawFd) -> Result<libc::termios, RawModeError> {
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is a valid descriptor and `tcgetattr` writes exactly one
    // `termios` through the pointer, which refers to live stack storage.
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc == -1 {
        return Err(RawModeError::Get(io::Error::last_os_error()));
    }
    // SAFETY: `tcgetattr` returned success, so the value is initialized.
    Ok(unsafe { termios.assume_init() })
}

/// Writes terminal attributes, flushing pending input so half-typed bytes from
/// the previous mode are not reinterpreted under the new one.
fn set_termios(fd: RawFd, termios: &libc::termios) -> Result<(), RawModeError> {
    // SAFETY: `fd` is a valid descriptor and `tcsetattr` reads one `termios`
    // through the pointer without retaining it.
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, termios) };
    if rc == -1 {
        return Err(RawModeError::Set(io::Error::last_os_error()));
    }
    Ok(())
}

/// The raw-mode form of `original`.
///
/// Pure, so the transformation is unit-testable without a terminal. `cfmakeraw`
/// does the work; the explicit `VMIN`/`VTIME` afterwards make a read block for
/// at least one byte rather than spinning, which `cfmakeraw` does not guarantee
/// on every platform.
fn raw_termios(original: libc::termios) -> libc::termios {
    let mut raw = original;
    // SAFETY: `cfmakeraw` only rewrites the flag fields of the `termios` it is
    // given, and `raw` is live local storage.
    unsafe { libc::cfmakeraw(&raw mut raw) };
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anything that needs a real terminal lives in `tests/raw_mode.rs` — see
    // docs/TESTING.md. Everything here is pure.

    /// A `termios` in a known cooked state, built without touching a terminal.
    fn cooked() -> libc::termios {
        // SAFETY: `termios` is a plain C struct of integers and a byte array,
        // so an all-zero value is a valid inhabitant. The flags are then set
        // explicitly below.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        termios.c_iflag = libc::ICRNL | libc::IXON | libc::BRKINT;
        termios.c_oflag = libc::OPOST;
        termios.c_lflag = libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
        termios.c_cc[libc::VMIN] = 0;
        termios.c_cc[libc::VTIME] = 4;
        termios
    }

    #[test]
    fn raw_clears_echo_canonical_and_signal_generation() {
        let raw = raw_termios(cooked());
        assert_eq!(raw.c_lflag & libc::ECHO, 0, "echo must be off");
        assert_eq!(raw.c_lflag & libc::ICANON, 0, "line editing must be off");
        assert_eq!(raw.c_lflag & libc::ISIG, 0, "the client owns Ctrl-C");
        assert_eq!(raw.c_lflag & libc::IEXTEN, 0);
    }

    #[test]
    fn raw_clears_input_and_output_processing() {
        let raw = raw_termios(cooked());
        assert_eq!(raw.c_iflag & libc::ICRNL, 0, "CR must not become NL");
        assert_eq!(raw.c_iflag & libc::IXON, 0, "flow control must not eat ^S");
        assert_eq!(
            raw.c_oflag & libc::OPOST,
            0,
            "the renderer emits exact bytes"
        );
    }

    #[test]
    fn raw_blocks_for_at_least_one_byte() {
        let raw = raw_termios(cooked());
        assert_eq!(raw.c_cc[libc::VMIN], 1);
        assert_eq!(raw.c_cc[libc::VTIME], 0);
    }

    #[test]
    fn raw_termios_does_not_mutate_its_input() {
        let original = cooked();
        let _ = raw_termios(original);
        assert_ne!(
            original.c_lflag & libc::ECHO,
            0,
            "the saved copy stays cooked"
        );
    }

    #[test]
    fn slot_refuses_a_second_arm_and_accepts_one_after_disarm() {
        // The global slot is shared, so this test drives it directly rather
        // than racing a `RawMode` guard for it.
        let slot = RestoreSlot {
            state: AtomicU8::new(SLOT_IDLE),
            fd: AtomicI32::new(-1),
            saved: UnsafeCell::new(MaybeUninit::uninit()),
            reset: UnsafeCell::new([0; MAX_RESET_LEN]),
            reset_len: AtomicUsize::new(0),
        };
        assert!(slot.arm(7, cooked()));
        assert!(!slot.arm(9, cooked()), "a second guard must be refused");
        slot.disarm();
        assert!(slot.arm(9, cooked()), "the slot is reusable after disarm");
    }

    #[test]
    fn an_idle_slot_restores_nothing() {
        let slot = RestoreSlot {
            state: AtomicU8::new(SLOT_IDLE),
            fd: AtomicI32::new(-1),
            saved: UnsafeCell::new(MaybeUninit::uninit()),
            reset: UnsafeCell::new([0; MAX_RESET_LEN]),
            reset_len: AtomicUsize::new(0),
        };
        // Would be a `tcsetattr` on fd -1 if the state check were missing.
        assert!(slot.restore().is_ok());
    }
}
