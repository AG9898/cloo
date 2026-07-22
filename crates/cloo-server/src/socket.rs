//! The daemon's socket lifecycle: where a session's socket lives, who owns it,
//! and what happens to the one a dead daemon left behind.
//!
//! Two halves. [`session_socket_path`] and [`resolve_socket_path`] decide the
//! path — the latter is a pure function of the environment so it is testable
//! without touching the process's real one, matching `cloo-client::outer`.
//! [`Listener`] is the resource: it takes an exclusive lock, clears a stale
//! socket, binds, and unlinks on drop.
//!
//! Ownership is enforced by an advisory `flock` on a companion lock file, not
//! by the presence of the socket. A socket file proves nothing — a daemon that
//! was killed with `SIGKILL` leaves one behind, and a daemon that is alive and
//! serving has one too. The kernel drops a `flock` when the holding process
//! dies, however it dies, so the lock answers "is a daemon running" exactly.
//! Holding it is also what makes stale cleanup safe: a second daemon can only
//! reach the unlink after it has established that no first daemon exists.
//!
//! Cleanup only ever touches the one path it holds the lock for, and only when
//! that path is a socket. An ordinary file sitting where the socket belongs is
//! a refusal, never something to delete — `CLOO_SOCKET` is user-supplied and a
//! typo must not cost anyone a file.
//!
//! ```no_run
//! use cloo_server::socket::{Listener, session_socket_path};
//!
//! # fn example() -> Result<(), cloo_server::socket::SocketError> {
//! let path = session_socket_path("default")?;
//! let listener = Listener::bind(&path)?;
//! assert_eq!(listener.path(), path);
//! # Ok(())
//! # }
//! ```

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// The directory created under the runtime dir to hold session sockets.
const SOCKET_DIR: &str = "cloo";

/// The extension appended to a socket path to name its lock file.
const LOCK_SUFFIX: &str = ".lock";

/// The extension appended to a socket path to name its adapter control socket.
const CONTROL_SUFFIX: &str = ".control";

/// Mode for the socket directory and the lock file: owner only.
///
/// A session socket is a channel into the user's shell. Anything wider than
/// `0700` on the directory would let another local account connect.
const OWNER_ONLY_DIR: u32 = 0o700;
/// Mode for the lock file: owner read/write.
const OWNER_ONLY_FILE: u32 = 0o600;

/// Everything the socket layer can refuse to do.
#[derive(Debug)]
pub enum SocketError {
    /// The session name could not be turned into a file name.
    SessionName {
        /// The name as given.
        name: String,
        /// Why it was rejected.
        reason: NameRejection,
    },
    /// A socket path was requested but no directory could be derived for it.
    NoRuntimeDir,
    /// The socket path has no parent directory, so nothing can be created.
    NoParentDir(PathBuf),
    /// The socket directory could not be created or secured.
    Directory {
        /// The directory that was being prepared.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The lock file could not be opened.
    Lock {
        /// The lock file path.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// Another daemon already holds this session's socket.
    AlreadyRunning(PathBuf),
    /// Something that is not a socket already occupies the socket path. Never
    /// removed automatically — see the module docs.
    NotASocket(PathBuf),
    /// A stale socket could not be removed.
    Cleanup {
        /// The path that could not be unlinked.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// Binding the socket failed.
    Bind {
        /// The path that could not be bound.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
}

/// Why a session name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRejection {
    /// The name was empty.
    Empty,
    /// The name was `.` or `..`, which name a directory rather than a session.
    Reserved,
    /// The name contained a path separator, a NUL, or a control character.
    IllegalCharacter,
}

impl fmt::Display for NameRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("it is empty"),
            Self::Reserved => f.write_str("it is a reserved directory name"),
            Self::IllegalCharacter => {
                f.write_str("it contains a path separator or a control character")
            }
        }
    }
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionName { name, reason } => {
                write!(f, "invalid session name {name:?}: {reason}")
            }
            Self::NoRuntimeDir => f.write_str(
                "no socket directory: set XDG_RUNTIME_DIR, or CLOO_SOCKET to a full path",
            ),
            Self::NoParentDir(path) => {
                write!(f, "socket path {} has no parent directory", path.display())
            }
            Self::Directory { path, source } => {
                write!(
                    f,
                    "could not prepare socket directory {}: {source}",
                    path.display()
                )
            }
            Self::Lock { path, source } => {
                write!(f, "could not open lock file {}: {source}", path.display())
            }
            Self::AlreadyRunning(path) => {
                write!(
                    f,
                    "a cloo daemon is already running on {}; attach to it instead",
                    path.display()
                )
            }
            Self::NotASocket(path) => {
                write!(
                    f,
                    "{} exists and is not a socket; cloo will not remove it",
                    path.display()
                )
            }
            Self::Cleanup { path, source } => {
                write!(
                    f,
                    "could not remove stale socket {}: {source}",
                    path.display()
                )
            }
            Self::Bind { path, source } => {
                write!(f, "could not bind {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Directory { source, .. }
            | Self::Lock { source, .. }
            | Self::Cleanup { source, .. }
            | Self::Bind { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Checks that `name` can be used as a socket file name.
///
/// Session names reach the filesystem, so a name containing `/` would let a
/// session escape its directory and a name of `..` would point at the parent.
/// Both are refused rather than sanitized: silently renaming a session makes
/// the socket a user cannot find.
///
/// # Errors
///
/// Returns the specific [`NameRejection`] so the caller can explain itself.
pub fn validate_session_name(name: &str) -> Result<(), NameRejection> {
    if name.is_empty() {
        return Err(NameRejection::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameRejection::Reserved);
    }
    if name
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(NameRejection::IllegalCharacter);
    }
    Ok(())
}

/// Decides where a session's socket lives, from explicit inputs.
///
/// Precedence is `CLOO_SOCKET`, then `$XDG_RUNTIME_DIR/cloo/<session>.sock`,
/// then `/tmp/cloo-<uid>/<session>.sock`. `CLOO_SOCKET` is taken verbatim and
/// names a socket, not a directory — it exists so a development daemon can be
/// stood up beside a live one without inventing a session name for it, and it
/// therefore ignores the session entirely.
///
/// The `/tmp` fallback is per-uid so two users on one machine never collide.
/// It is a fallback and not the default because `/tmp` outlives a login
/// session and `$XDG_RUNTIME_DIR` does not.
///
/// # Errors
///
/// Returns [`SocketError::SessionName`] for a name that cannot be a file, and
/// [`SocketError::NoRuntimeDir`] if `runtime_dir` is unset or empty and no
/// override was given.
pub fn resolve_socket_path(
    session: &str,
    socket_override: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    uid: u32,
) -> Result<PathBuf, SocketError> {
    if let Some(path) = socket_override.filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    validate_session_name(session).map_err(|reason| SocketError::SessionName {
        name: session.to_owned(),
        reason,
    })?;

    let dir = match runtime_dir.filter(|d| !d.is_empty()) {
        Some(runtime) => Path::new(runtime).join(SOCKET_DIR),
        // No `$XDG_RUNTIME_DIR`: a per-uid directory under `/tmp` is the
        // conventional stand-in, and it is already scoped, so no `cloo`
        // component is appended.
        None => PathBuf::from(format!("/tmp/cloo-{uid}")),
    };

    let mut file_name = OsString::from(session);
    file_name.push(".sock");
    Ok(dir.join(file_name))
}

/// Reads the process environment and decides where a session's socket lives.
///
/// # Errors
///
/// As [`resolve_socket_path`].
pub fn session_socket_path(session: &str) -> Result<PathBuf, SocketError> {
    let socket_override = std::env::var_os("CLOO_SOCKET");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    // SAFETY: `geteuid` reads process credentials, takes no pointer, and cannot
    // fail.
    let uid = unsafe { libc::geteuid() };
    resolve_socket_path(
        session,
        socket_override.as_deref(),
        runtime_dir.as_deref(),
        uid,
    )
}

/// The lock file path that guards `socket`.
#[must_use]
pub fn lock_path_for(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(LOCK_SUFFIX);
    PathBuf::from(name)
}

/// The adapter control socket that belongs to `socket`.
///
/// Derived from the session socket rather than resolved separately, so
/// `CLOO_SOCKET` moves both halves of a development daemon together and the two
/// can never end up pointing at different sessions.
///
/// It is a *separate* endpoint on purpose. An opt-in adapter is not a client:
/// it speaks [`AdapterMessage`](cloo_proto::AdapterMessage), which has no
/// variant for keystrokes, geometry, or anything else that could reach a child,
/// so "an adapter may only report attention" is a property of which socket it
/// connected to rather than a refusal the server has to remember to make.
#[must_use]
pub fn control_path_for(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(CONTROL_SUFFIX);
    PathBuf::from(name)
}

/// A bound session socket, owned exclusively by this process.
///
/// Construction is the whole lifecycle: lock, clean, bind. Destruction is the
/// other half — the socket file is unlinked on drop, so a daemon that exits
/// normally leaves nothing for the next one to clean up. Restoration is by
/// ownership, matching `Pty` and `RawMode`.
///
/// The listener is non-blocking so `tokio::net::UnixListener::from_std` accepts
/// it. M1-02 does that conversion with a [`try_clone`](Listener::try_clone_std)
/// so this guard, and its unlink, stay alive alongside it.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    /// The socket's identity when it was bound, so drop can tell "the socket I
    /// created" from "a socket someone else created at the same path".
    identity: (u64, u64),
    /// Held for as long as this daemon owns the socket. Dropping it releases
    /// the `flock`.
    _lock: OwnedFd,
}

impl Listener {
    /// Takes ownership of `path` and binds it.
    ///
    /// Creates the parent directory `0700` if it does not exist, takes the
    /// exclusive lock, removes a socket a dead daemon left behind, and binds.
    ///
    /// # Errors
    ///
    /// [`SocketError::AlreadyRunning`] if a live daemon holds the lock — that
    /// is a normal outcome, not a fault, and the caller should suggest
    /// attaching. [`SocketError::NotASocket`] if a non-socket occupies the
    /// path; nothing is removed in that case. Otherwise the underlying
    /// directory, lock, cleanup, or bind failure.
    pub fn bind(path: &Path) -> Result<Self, SocketError> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| SocketError::NoParentDir(path.to_owned()))?;
        prepare_dir(dir)?;

        let lock = acquire_lock(path)?;

        // The lock is held, so no other cloo daemon owns this path and anything
        // sitting here is debris.
        clear_stale(path)?;

        let listener = UnixListener::bind(path).map_err(|source| SocketError::Bind {
            path: path.to_owned(),
            source,
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|source| SocketError::Bind {
                path: path.to_owned(),
                source,
            })?;

        let identity = identity_of(path).map_err(|source| SocketError::Bind {
            path: path.to_owned(),
            source,
        })?;

        Ok(Self {
            listener,
            path: path.to_owned(),
            identity,
            _lock: lock,
        })
    }

    /// Resolves a session name and binds its socket.
    ///
    /// # Errors
    ///
    /// As [`session_socket_path`] and [`Listener::bind`].
    pub fn bind_session(session: &str) -> Result<Self, SocketError> {
        let path = session_socket_path(session)?;
        Self::bind(&path)
    }

    /// The path this listener is bound to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The underlying non-blocking listener.
    #[must_use]
    pub fn as_std(&self) -> &UnixListener {
        &self.listener
    }

    /// A duplicate of the listener, for handing to an async reactor.
    ///
    /// # Errors
    ///
    /// Returns the `dup` failure.
    pub fn try_clone_std(&self) -> io::Result<UnixListener> {
        self.listener.try_clone()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Unlink only the socket this listener created. If the file at the path
        // has a different identity, a successor already replaced it and
        // removing it would take down a daemon that has nothing to do with this
        // one. Every failure here is ignored: drop cannot report, and a socket
        // left behind is cleaned up by the next `bind`.
        if identity_of(&self.path).is_ok_and(|id| id == self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Creates the socket directory if needed and narrows it to `0700`.
fn prepare_dir(dir: &Path) -> Result<(), SocketError> {
    let wrap = |source| SocketError::Directory {
        path: dir.to_owned(),
        source,
    };

    if !dir.exists() {
        fs::create_dir_all(dir).map_err(wrap)?;
    }

    let metadata = fs::metadata(dir).map_err(wrap)?;
    if !metadata.is_dir() {
        return Err(wrap(io::Error::new(
            io::ErrorKind::NotADirectory,
            "socket directory path is not a directory",
        )));
    }
    // Narrow an existing directory too: `create_dir_all` applies the umask, and
    // a directory left group-writable by an earlier version stays that way
    // otherwise.
    if metadata.permissions().mode() & 0o777 != OWNER_ONLY_DIR {
        fs::set_permissions(dir, fs::Permissions::from_mode(OWNER_ONLY_DIR)).map_err(wrap)?;
    }
    Ok(())
}

/// Opens the lock file for `socket` and takes it exclusively.
fn acquire_lock(socket: &Path) -> Result<OwnedFd, SocketError> {
    let path = lock_path_for(socket);
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(OWNER_ONLY_FILE)
        .open(&path)
        .map_err(|source| SocketError::Lock {
            path: path.clone(),
            source,
        })?;

    // SAFETY: `file` is a live open descriptor for the duration of the call and
    // `flock` takes no pointer. `LOCK_NB` keeps this from blocking a daemon
    // startup behind a running one.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == -1 {
        let err = io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => SocketError::AlreadyRunning(socket.to_owned()),
            _ => SocketError::Lock { path, source: err },
        });
    }
    Ok(OwnedFd::from(file))
}

/// Removes a socket left behind by a dead daemon.
///
/// The caller must already hold the lock; without it this cannot tell a stale
/// socket from a live one.
fn clear_stale(path: &Path) -> Result<(), SocketError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(SocketError::Cleanup {
                path: path.to_owned(),
                source,
            });
        }
    };

    // A symlink, a regular file, or a directory here is not ours to delete.
    // `symlink_metadata` is what makes that check honest: following a symlink
    // would report the *target's* type and unlink could then remove a link
    // pointing anywhere.
    if !metadata.file_type().is_socket() {
        return Err(SocketError::NotASocket(path.to_owned()));
    }

    fs::remove_file(path).map_err(|source| SocketError::Cleanup {
        path: path.to_owned(),
        source,
    })
}

/// The `(device, inode)` pair identifying whatever is at `path`.
fn identity_of(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a socket",
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_override_wins_and_ignores_the_session() {
        let path = resolve_socket_path(
            "work",
            Some(OsStr::new("/tmp/cloo-dev.sock")),
            Some(OsStr::new("/run/user/1000")),
            1000,
        )
        .expect("an override is always usable");
        assert_eq!(path, PathBuf::from("/tmp/cloo-dev.sock"));
    }

    #[test]
    fn an_empty_override_is_treated_as_unset() {
        let path = resolve_socket_path(
            "work",
            Some(OsStr::new("")),
            Some(OsStr::new("/run/user/1000")),
            1000,
        )
        .expect("an empty override falls through");
        assert_eq!(path, PathBuf::from("/run/user/1000/cloo/work.sock"));
    }

    #[test]
    fn the_runtime_dir_gets_a_cloo_component() {
        let path = resolve_socket_path("default", None, Some(OsStr::new("/run/user/1000")), 1000)
            .expect("a runtime dir is enough");
        assert_eq!(path, PathBuf::from("/run/user/1000/cloo/default.sock"));
    }

    #[test]
    fn no_runtime_dir_falls_back_to_a_per_uid_tmp_dir() {
        let path = resolve_socket_path("default", None, None, 501)
            .expect("the fallback needs nothing from the environment");
        assert_eq!(path, PathBuf::from("/tmp/cloo-501/default.sock"));

        let empty = resolve_socket_path("default", None, Some(OsStr::new("")), 501)
            .expect("an empty runtime dir is unset");
        assert_eq!(empty, PathBuf::from("/tmp/cloo-501/default.sock"));
    }

    #[test]
    fn a_name_that_would_escape_the_directory_is_refused() {
        for name in ["", ".", "..", "a/b", "a\\b", "a\nb", "a\0b"] {
            let err = resolve_socket_path(name, None, Some(OsStr::new("/run/user/1000")), 1000)
                .expect_err("{name} must be refused");
            assert!(
                matches!(err, SocketError::SessionName { .. }),
                "{name:?} produced {err}"
            );
        }
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["default", "work-2", "agent.codex", "a b"] {
            validate_session_name(name).expect("an ordinary name is usable");
        }
    }

    #[test]
    fn the_lock_file_sits_beside_the_socket() {
        assert_eq!(
            lock_path_for(Path::new("/run/user/1000/cloo/work.sock")),
            PathBuf::from("/run/user/1000/cloo/work.sock.lock")
        );
    }

    #[test]
    fn the_control_socket_is_derived_from_the_session_socket() {
        // Including an overridden one: a dev daemon must not serve adapters on
        // the live session's control socket.
        assert_eq!(
            control_path_for(Path::new("/run/user/1000/cloo/work.sock")),
            PathBuf::from("/run/user/1000/cloo/work.sock.control")
        );
        assert_eq!(
            control_path_for(Path::new("/tmp/cloo-dev.sock")),
            PathBuf::from("/tmp/cloo-dev.sock.control")
        );
    }

    #[test]
    fn the_control_socket_has_a_lock_of_its_own() {
        // Two endpoints, two guards: a stale control socket is cleaned up by
        // the same ownership rule the session socket uses.
        let control = control_path_for(Path::new("/run/user/1000/cloo/work.sock"));
        assert_eq!(
            lock_path_for(&control),
            PathBuf::from("/run/user/1000/cloo/work.sock.control.lock")
        );
    }
}
