//! Discovery of live local cloo sessions without attaching to them.
//!
//! The catalog trusts neither directory-entry names nor filesystem object
//! types as proof of a session. It considers only sockets directly inside the
//! resolved per-user cloo runtime directory, rejects symlinks without
//! following them, and includes an entry only after the peer answers the
//! versioned read-only inspection handshake.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cloo_proto::SessionSummary;
use tokio::task::JoinSet;

use crate::attach::inspect;

/// Maximum time one socket candidate may occupy catalog discovery.
pub const INSPECTION_DEADLINE: Duration = Duration::from_millis(500);

/// One independently verified local session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCatalogEntry {
    /// The socket that answered the inspection.
    pub socket: PathBuf,
    /// The daemon-owned, read-only session summary.
    pub summary: SessionSummary,
}

/// A catalog failure outside an individual untrusted socket candidate.
#[derive(Debug)]
pub enum SessionCatalogError {
    /// The resolved runtime directory could not be enumerated.
    ReadDirectory {
        /// The directory discovery attempted to enumerate.
        path: PathBuf,
        /// The underlying filesystem failure.
        source: io::Error,
    },
}

impl fmt::Display for SessionCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDirectory { path, source } => {
                write!(
                    f,
                    "could not discover cloo sessions in {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for SessionCatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadDirectory { source, .. } => Some(source),
        }
    }
}

/// Discovers sessions using this process's socket-related environment.
///
/// A non-empty `CLOO_SOCKET` is one exact untrusted candidate. Otherwise the
/// function enumerates `$XDG_RUNTIME_DIR/cloo`, falling back to
/// `/tmp/cloo-$UID`, with every candidate independently bounded by
/// [`INSPECTION_DEADLINE`].
///
/// # Errors
///
/// Returns an error only when the resolved runtime directory exists but cannot
/// be enumerated. Missing directories and failed socket candidates produce an
/// empty or smaller honest catalog.
pub async fn discover_sessions() -> Result<Vec<SessionCatalogEntry>, SessionCatalogError> {
    let socket_override = std::env::var_os("CLOO_SOCKET");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    // SAFETY: `geteuid` reads process credentials, takes no pointer, and cannot
    // fail.
    let uid = unsafe { libc::geteuid() };
    discover_sessions_from(
        socket_override.as_deref(),
        runtime_dir.as_deref(),
        uid,
        INSPECTION_DEADLINE,
    )
    .await
}

/// Discovers sessions from explicit environment inputs.
///
/// This is the pure-environment seam used by tests and by callers that already
/// resolved their environment. `socket_override` follows `CLOO_SOCKET`
/// semantics: a non-empty value suppresses directory enumeration and yields at
/// most one verified entry.
///
/// # Errors
///
/// As [`discover_sessions`].
pub async fn discover_sessions_from(
    socket_override: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    uid: u32,
    deadline: Duration,
) -> Result<Vec<SessionCatalogEntry>, SessionCatalogError> {
    let candidates = if let Some(socket) = socket_override.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(socket);
        if is_socket(&path) {
            vec![path]
        } else {
            Vec::new()
        }
    } else {
        candidate_sockets(&catalog_directory(runtime_dir, uid))?
    };

    let mut inspections = JoinSet::new();
    for socket in candidates {
        inspections.spawn(async move {
            let result = tokio::time::timeout(deadline, inspect(&socket)).await;
            result
                .ok()
                .and_then(Result::ok)
                .map(|summary| SessionCatalogEntry { socket, summary })
        });
    }

    let mut entries = Vec::new();
    while let Some(result) = inspections.join_next().await {
        if let Ok(Some(entry)) = result {
            entries.push(entry);
        }
    }
    entries.sort_by(|left, right| {
        left.summary
            .name
            .cmp(&right.summary.name)
            .then_with(|| left.socket.cmp(&right.socket))
    });
    Ok(entries)
}

/// The directory holding ordinary session sockets for this environment.
fn catalog_directory(runtime_dir: Option<&OsStr>, uid: u32) -> PathBuf {
    runtime_dir.filter(|dir| !dir.is_empty()).map_or_else(
        || PathBuf::from(format!("/tmp/cloo-{uid}")),
        |dir| Path::new(dir).join("cloo"),
    )
}

/// Enumerates actual socket objects directly inside `directory`.
fn candidate_sockets(directory: &Path) -> Result<Vec<PathBuf>, SessionCatalogError> {
    let read = match fs::read_dir(directory) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionCatalogError::ReadDirectory {
                path: directory.to_owned(),
                source,
            });
        }
    };

    let mut candidates = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_socket(path))
        .collect::<Vec<_>>();
    candidates.sort();
    Ok(candidates)
}

/// Checks the candidate itself, never a symlink target.
fn is_socket(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_catalog_directory_follows_runtime_and_uid_fallback_rules() {
        assert_eq!(
            catalog_directory(Some(OsStr::new("/run/user/7")), 7),
            PathBuf::from("/run/user/7/cloo")
        );
        assert_eq!(
            catalog_directory(Some(OsStr::new("")), 7),
            PathBuf::from("/tmp/cloo-7")
        );
        assert_eq!(catalog_directory(None, 7), PathBuf::from("/tmp/cloo-7"));
    }
}
