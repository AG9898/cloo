//! The single-pane PTY reactor.
//!
//! Two layers live here. [`Pty`] is the raw resource: a Unix pseudoterminal
//! pair, a child process attached to the slave side, and the `libc` calls that
//! read, write, and resize it. [`PtyReactor`] is the actor-shaped layer above
//! it — one of these exists per pane, it owns that pane's [`Emulator`], and its
//! only job is to move bytes from the PTY into the grid.
//!
//! Everything fallible returns a [`Result`]. There is no `unwrap` in the read
//! path, and every `unsafe` block carries a `// SAFETY:` comment.
//!
//! Resource restoration is handled by ownership rather than by a shutdown call
//! the caller has to remember: the master descriptor is an [`OwnedFd`], and
//! [`Pty`]'s `Drop` reaps the child so a dropped pane cannot leak a zombie.
//!
//! ```no_run
//! use cloo_server::pty::{PtyConfig, PtyReactor, Pump};
//! use cloo_term::TermSize;
//!
//! # async fn example() -> Result<(), cloo_server::pty::PtyError> {
//! let config = PtyConfig::new("sh").arg("-c").arg("echo hello").size(TermSize::new(80, 24)?);
//! let mut reactor = PtyReactor::spawn(&config)?;
//! while !matches!(reactor.pump().await?, Pump::Eof) {}
//! assert_eq!(reactor.emulator().row_text(0).as_deref(), Some("hello"));
//! # Ok(())
//! # }
//! ```

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};

use cloo_core::grid::{wire_cursor, wire_row, wire_size};
use cloo_proto::{CursorShape, Point, RowUpdate, Size};
use cloo_term::{Emulator, TermError, TermSize};
use tokio::io::unix::AsyncFd;

/// Size of a single PTY read.
///
/// A PTY's kernel buffer is a few kilobytes, so a larger buffer mostly costs
/// nothing; the point is to keep a fast producer from turning into one syscall
/// per line. Damage coalescing happens above this layer — the reactor still
/// only feeds the emulator, and never emits a render update per read.
const READ_BUF_LEN: usize = 8192;

/// Everything the PTY layer can refuse to do.
#[derive(Debug)]
pub enum PtyError {
    /// `openpty` failed and no pseudoterminal pair was allocated.
    Open(io::Error),
    /// The child process could not be spawned onto the slave side.
    Spawn {
        /// The program that was being executed.
        program: OsString,
        /// The underlying failure.
        source: io::Error,
    },
    /// A descriptor could not be configured (non-blocking mode, close-on-exec,
    /// or a duplicate for the child's stdio).
    Configure(io::Error),
    /// A read from the PTY master failed.
    Read(io::Error),
    /// A write to the PTY master failed.
    Write(io::Error),
    /// `TIOCSWINSZ` failed, so the child was not told about a new size.
    Resize(io::Error),
    /// Waiting on the child failed.
    Wait(io::Error),
    /// Registering the master descriptor with the Tokio reactor failed. Most
    /// often this means construction happened outside a runtime context.
    Register(io::Error),
    /// The requested grid geometry was rejected by `cloo-term`.
    Size(TermError),
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(e) => write!(f, "could not allocate a pseudoterminal: {e}"),
            Self::Spawn { program, source } => {
                write!(f, "could not spawn {}: {source}", program.to_string_lossy())
            }
            Self::Configure(e) => write!(f, "could not configure a pty descriptor: {e}"),
            Self::Read(e) => write!(f, "pty read failed: {e}"),
            Self::Write(e) => write!(f, "pty write failed: {e}"),
            Self::Resize(e) => write!(f, "could not resize the pty: {e}"),
            Self::Wait(e) => write!(f, "could not wait on the pty child: {e}"),
            Self::Register(e) => write!(f, "could not register the pty with the runtime: {e}"),
            Self::Size(e) => write!(f, "invalid pty size: {e}"),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(e)
            | Self::Configure(e)
            | Self::Read(e)
            | Self::Write(e)
            | Self::Resize(e)
            | Self::Wait(e)
            | Self::Register(e) => Some(e),
            Self::Spawn { source, .. } => Some(source),
            Self::Size(e) => Some(e),
        }
    }
}

impl From<TermError> for PtyError {
    fn from(value: TermError) -> Self {
        Self::Size(value)
    }
}

/// How to start a pane's child process.
///
/// Deliberately small: this is the M0 surface. Working directory and profile
/// environment arrive with pane metadata in M2.
#[derive(Debug, Clone)]
pub struct PtyConfig {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    size: TermSize,
}

impl PtyConfig {
    /// Default grid for a pane whose size has not been decided yet.
    ///
    /// The layout pass supplies the real geometry; this only exists so a
    /// config is constructible without one.
    pub const DEFAULT_COLS: u16 = 80;
    /// Default grid height. See [`DEFAULT_COLS`](Self::DEFAULT_COLS).
    pub const DEFAULT_ROWS: u16 = 24;

    /// Starts a config for `program` at the default size.
    #[must_use]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let size = match TermSize::new(Self::DEFAULT_COLS, Self::DEFAULT_ROWS) {
            Ok(size) => size,
            // Unreachable: both constants are non-zero literals.
            Err(_) => unreachable!("default pty size must be valid"),
        };
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            env: Vec::new(),
            size,
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Sets one environment variable for the child.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Sets the initial grid size, which is also the child's initial `winsize`.
    #[must_use]
    pub fn size(mut self, size: TermSize) -> Self {
        self.size = size;
        self
    }

    /// Sets the initial grid size from a wire geometry.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Size`] if either dimension is zero. A client that
    /// cannot describe its own geometry is not something to guess about.
    pub fn wire_size(self, size: Size) -> Result<Self, PtyError> {
        Ok(self.size(TermSize::new(size.cols, size.rows)?))
    }

    /// The configured grid size.
    #[must_use]
    pub fn term_size(&self) -> TermSize {
        self.size
    }
}

/// A pseudoterminal and the child process running on it.
///
/// The master descriptor is owned and non-blocking. Dropping a `Pty` closes
/// the master and reaps the child, so a closed pane never leaves a zombie or a
/// leaked descriptor behind.
#[derive(Debug)]
pub struct Pty {
    master: OwnedFd,
    child: Child,
    reaped: bool,
}

impl Pty {
    /// Allocates a pseudoterminal and spawns `config`'s program on its slave
    /// side as a session leader with a controlling terminal.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Open`] if no pair could be allocated,
    /// [`PtyError::Configure`] if a descriptor could not be prepared, and
    /// [`PtyError::Spawn`] if the child could not be started.
    pub fn spawn(config: &PtyConfig) -> Result<Self, PtyError> {
        let (master, slave) = open_pty(config.size)?;

        // The master must not survive into the child: the child holding a
        // writable master would keep the descriptor alive after we close ours,
        // and reads on our side would never see EOF.
        set_cloexec(master.as_fd())?;
        set_nonblocking(master.as_fd())?;

        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .stdin(Stdio::from(dup(slave.as_fd())?))
            .stdout(Stdio::from(dup(slave.as_fd())?))
            .stderr(Stdio::from(dup(slave.as_fd())?));
        for (key, value) in &config.env {
            command.env(key, value);
        }

        // SAFETY: `pre_exec` runs in the forked child between `fork` and
        // `exec`, where only async-signal-safe calls are permitted. The
        // closure makes exactly two: `setsid` and an `ioctl`, both of which are
        // async-signal-safe, and it allocates nothing. By this point the
        // standard library has already dup2'd the slave onto descriptors 0, 1,
        // and 2, so `TIOCSCTTY` on descriptor 0 acquires the slave as the new
        // session's controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn().map_err(|source| PtyError::Spawn {
            program: config.program.clone(),
            source,
        })?;

        // The parent has no use for the slave. Holding it open would keep the
        // PTY from ever reporting EOF after the child exits.
        drop(slave);

        Ok(Self {
            master,
            child,
            reaped: false,
        })
    }

    /// Reads available output, returning `Ok(0)` at end of file.
    ///
    /// A read on a Linux PTY master whose slave has been fully closed fails
    /// with `EIO` rather than returning zero. That is the normal end of a
    /// pane's life, so it is translated into an ordinary EOF here; callers only
    /// see a genuine error.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Read`] if the underlying read failed. A
    /// [`io::ErrorKind::WouldBlock`] error is surfaced as-is: the master is
    /// non-blocking, and readiness is the reactor's concern.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, PtyError> {
        self.read_io(buf).map_err(PtyError::Read)
    }

    /// The `io::Result` form of [`read`](Self::read), for readiness-guarded
    /// callers such as [`AsyncFd::try_io`].
    fn read_io(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `master` is a valid open descriptor for the lifetime of
        // `self`, and the pointer and length describe `buf` exactly, so the
        // kernel writes only within it.
        let read = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if read >= 0 {
            // A non-negative `ssize_t` return never exceeds `buf.len()`.
            return Ok(read.unsigned_abs());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EIO) {
            Ok(0)
        } else {
            Err(err)
        }
    }

    /// Writes input to the child.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Write`] if the underlying write failed.
    pub fn write(&self, buf: &[u8]) -> Result<usize, PtyError> {
        // SAFETY: `master` is a valid open descriptor for the lifetime of
        // `self`, and the pointer and length describe `buf` exactly, so the
        // kernel reads only within it.
        let written = unsafe {
            libc::write(
                self.master.as_raw_fd(),
                buf.as_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if written < 0 {
            return Err(PtyError::Write(io::Error::last_os_error()));
        }
        Ok(written.unsigned_abs())
    }

    /// Tells the child about a new geometry via `TIOCSWINSZ`.
    ///
    /// This is only half of a resize: the grid must be resized too, and the two
    /// together are the ordering hazard described in `AGENTS.md`. Use
    /// [`PtyReactor::resize`], which does both.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Resize`] if the `ioctl` failed.
    pub fn resize(&self, size: TermSize) -> Result<(), PtyError> {
        let winsize = winsize_for(size);
        // SAFETY: `master` is a valid open descriptor, `TIOCSWINSZ` takes a
        // pointer to one `winsize`, and that is exactly what is passed.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &winsize) };
        if rc == -1 {
            return Err(PtyError::Resize(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Waits for the child to exit and reaps it.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Wait`] if the wait failed.
    pub fn wait(&mut self) -> Result<ExitStatus, PtyError> {
        let status = self.child.wait().map_err(PtyError::Wait)?;
        self.reaped = true;
        Ok(status)
    }

    /// The child's process id, useful for diagnostics.
    #[must_use]
    pub fn child_id(&self) -> u32 {
        self.child.id()
    }
}

impl AsRawFd for Pty {
    fn as_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }
}

impl AsFd for Pty {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Closing the master sends the child a `SIGHUP` in most cases, but a
        // child that ignores it would linger, so ask directly and then reap.
        // Both failures are expected when the child already exited.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What one [`PtyReactor::pump`] accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pump {
    /// Bytes were read and fed to the emulator.
    Bytes(usize),
    /// The PTY reached end of file. The child has closed its side and no
    /// further output will arrive.
    Eof,
}

/// Everything a client needs to draw one pane from scratch.
///
/// This is the server's side of "the server sends contents and geometry, the
/// client decides what it looks like": nothing here describes an appearance.
/// Row damage on the wire in M1-04 is the same [`RowUpdate`] type, so an
/// incremental update and a full resync stay one code path on the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// The pane's geometry.
    pub size: Size,
    /// Every visible row, top first.
    pub rows: Vec<RowUpdate>,
    /// Where to draw the cursor, or `None` if it should not be drawn.
    pub cursor: Option<(Point, CursorShape)>,
}

/// One pane's PTY plus its terminal grid.
///
/// This is the actor body: one Tokio task owns one `PtyReactor` and loops on
/// [`pump`](Self::pump). The reactor never renders and never decides what
/// anything looks like — it reads bytes, feeds the emulator, and lets the
/// session task coalesce damage above it.
///
/// Not `Debug`: the emulation backend's grid is not, and printing a pane's
/// whole scrollback would not be useful anyway.
pub struct PtyReactor {
    pty: AsyncFd<Pty>,
    emulator: Emulator,
}

impl PtyReactor {
    /// Spawns a child on a fresh PTY and registers the master with the runtime.
    ///
    /// Must be called from inside a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Propagates any [`Pty::spawn`] failure, or returns
    /// [`PtyError::Register`] if the descriptor could not be registered.
    pub fn spawn(config: &PtyConfig) -> Result<Self, PtyError> {
        let pty = Pty::spawn(config)?;
        Ok(Self {
            pty: AsyncFd::new(pty).map_err(PtyError::Register)?,
            emulator: Emulator::with_default_scrollback(config.term_size()),
        })
    }

    /// Waits for output, reads once, and feeds the result to the emulator.
    ///
    /// Returns [`Pump::Eof`] exactly once the child's side is closed; a caller
    /// that keeps pumping after that will keep seeing `Eof`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Read`] if the read failed for any reason other than
    /// the descriptor not actually being ready.
    pub async fn pump(&mut self) -> Result<Pump, PtyError> {
        let mut buf = [0_u8; READ_BUF_LEN];
        loop {
            let mut guard = self.pty.readable().await.map_err(PtyError::Read)?;
            match guard.try_io(|inner| inner.get_ref().read_io(&mut buf)) {
                Ok(Ok(0)) => return Ok(Pump::Eof),
                Ok(Ok(read)) => {
                    self.emulator.feed(&buf[..read]);
                    return Ok(Pump::Bytes(read));
                }
                Ok(Err(err)) => return Err(PtyError::Read(err)),
                // A spurious readiness notification. Clear it and wait again.
                Err(_would_block) => continue,
            }
        }
    }

    /// Pumps until end of file, returning the total number of bytes fed.
    ///
    /// Only appropriate for a child that terminates on its own — a scripted
    /// shell in a test, not an interactive one.
    ///
    /// # Errors
    ///
    /// Propagates the first [`pump`](Self::pump) failure.
    pub async fn run_to_eof(&mut self) -> Result<usize, PtyError> {
        let mut total = 0;
        loop {
            match self.pump().await? {
                Pump::Bytes(read) => total += read,
                Pump::Eof => return Ok(total),
            }
        }
    }

    /// Resizes the grid and the child's `winsize` together.
    ///
    /// The grid is resized first so that output arriving immediately after the
    /// child's `SIGWINCH` lands on a grid that is already the right shape.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Resize`] if `TIOCSWINSZ` failed. The grid has
    /// already been resized in that case, which is the recoverable direction to
    /// be inconsistent in: a later successful resize converges.
    pub fn resize(&mut self, size: TermSize) -> Result<(), PtyError> {
        self.emulator.resize(size);
        self.pty.get_ref().resize(size)
    }

    /// Forwards input bytes to the child, retrying short writes.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Write`] if a write failed.
    pub fn write_all(&self, mut buf: &[u8]) -> Result<(), PtyError> {
        while !buf.is_empty() {
            let written = self.pty.get_ref().write(buf)?;
            if written == 0 {
                return Err(PtyError::Write(io::Error::from(io::ErrorKind::WriteZero)));
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    /// Captures the whole visible grid as wire contents.
    ///
    /// A full capture per frame is the M0 shape. It is bounded by the frame
    /// rate rather than by output volume — the caller renders on a timer, not
    /// once per [`pump`](Self::pump) — which is the property that matters;
    /// M1-04 replaces the capture itself with coalesced per-row damage.
    #[must_use]
    pub fn snapshot(&self) -> PaneSnapshot {
        PaneSnapshot {
            size: wire_size(self.emulator.size()),
            rows: self
                .emulator
                .rows()
                .iter()
                .enumerate()
                .map(|(index, cells)| {
                    // A grid never has more rows than a `u16` can index: its
                    // height came from one in the first place.
                    wire_row(u16::try_from(index).unwrap_or(u16::MAX), cells)
                })
                .collect(),
            cursor: wire_cursor(self.emulator.cursor()),
        }
    }

    /// The pane's grid.
    #[must_use]
    pub fn emulator(&self) -> &Emulator {
        &self.emulator
    }

    /// The pane's grid, mutably — for scrollback navigation.
    pub fn emulator_mut(&mut self) -> &mut Emulator {
        &mut self.emulator
    }

    /// Waits for the child to exit and reaps it.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Wait`] if the wait failed.
    pub fn wait(&mut self) -> Result<ExitStatus, PtyError> {
        self.pty.get_mut().wait()
    }
}

/// Allocates a master/slave pair sized to `size`.
fn open_pty(size: TermSize) -> Result<(OwnedFd, OwnedFd), PtyError> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let winsize = winsize_for(size);
    // SAFETY: `openpty` writes one descriptor to each of the two out
    // parameters, both of which are valid, initialized locals. The termios
    // pointer is null (inherit defaults) and the winsize pointer refers to a
    // live local that outlives the call.
    let rc = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &winsize,
        )
    };
    if rc == -1 {
        return Err(PtyError::Open(io::Error::last_os_error()));
    }
    // SAFETY: `openpty` returned success, so both descriptors are open, owned
    // by this process, and not owned by anything else yet.
    let pair = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    Ok(pair)
}

/// Duplicates `fd` into a fresh owned descriptor for the child's stdio.
fn dup(fd: BorrowedFd<'_>) -> Result<OwnedFd, PtyError> {
    fd.try_clone_to_owned().map_err(PtyError::Configure)
}

/// Marks `fd` close-on-exec.
fn set_cloexec(fd: BorrowedFd<'_>) -> Result<(), PtyError> {
    // SAFETY: `fd` is a valid borrowed descriptor for the duration of the
    // call, and both `fcntl` commands used here take an `int` argument.
    let rc = unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFD);
        if flags == -1 {
            -1
        } else {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC)
        }
    };
    if rc == -1 {
        return Err(PtyError::Configure(io::Error::last_os_error()));
    }
    Ok(())
}

/// Puts `fd` in non-blocking mode, which `AsyncFd` requires.
fn set_nonblocking(fd: BorrowedFd<'_>) -> Result<(), PtyError> {
    // SAFETY: `fd` is a valid borrowed descriptor for the duration of the
    // call, and both `fcntl` commands used here take an `int` argument.
    let rc = unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags == -1 {
            -1
        } else {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
        }
    };
    if rc == -1 {
        return Err(PtyError::Configure(io::Error::last_os_error()));
    }
    Ok(())
}

/// Converts a grid size to the kernel's `winsize`.
///
/// Pixel dimensions are reported as zero: cloo is a cell-grid multiplexer and
/// has no per-cell pixel metrics to offer.
fn winsize_for(size: TermSize) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows(),
        ws_col: size.cols(),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests never spawn a PTY — see docs/TESTING.md. Everything below is
    // pure. Behaviour that needs a real child lives in `tests/pty.rs`.

    #[test]
    fn config_defaults_to_a_valid_size() {
        let config = PtyConfig::new("sh");
        assert_eq!(config.term_size().cols(), PtyConfig::DEFAULT_COLS);
        assert_eq!(config.term_size().rows(), PtyConfig::DEFAULT_ROWS);
    }

    #[test]
    fn config_builder_records_size() {
        let size = TermSize::new(120, 40).expect("120x40 is a valid size");
        let config = PtyConfig::new("sh").arg("-c").arg("true").size(size);
        assert_eq!(config.term_size(), size);
    }

    #[test]
    fn winsize_carries_rows_and_cols_and_no_pixels() {
        let size = TermSize::new(100, 30).expect("100x30 is a valid size");
        let winsize = winsize_for(size);
        assert_eq!(winsize.ws_col, 100);
        assert_eq!(winsize.ws_row, 30);
        assert_eq!(winsize.ws_xpixel, 0);
        assert_eq!(winsize.ws_ypixel, 0);
    }

    #[test]
    fn size_errors_convert_into_pty_errors() {
        let err = TermSize::new(0, 24).expect_err("a zero column count is invalid");
        assert!(matches!(PtyError::from(err), PtyError::Size(_)));
    }
}
