//! The cloo daemon: Unix socket listener, PTY reactor, and damage tracking.
//!
//! The server owns all authoritative state — PTYs, grids, scrollback, and
//! layout — and fans damage out to every attached client. It never decides what
//! anything looks like; chrome is rendered client-side.
//!
//! All session mutation funnels through a single session task over
//! `mpsc<Command>`. There is no `Mutex` on session state.
//!
//! Six modules today:
//!
//! - [`pty`] — the single-pane PTY reactor: pseudoterminal allocation, the
//!   child process, and the read loop that feeds a `cloo-term` grid.
//! - [`launch`] — a profile plus what the user typed, turned into a spawn
//!   configuration and the pane's metadata. The only way a pane is created, and
//!   where `$SHELL` and a working directory are resolved.
//! - [`session`] — the session task: the single `mpsc<Command>` every mutation
//!   arrives on, the layout pass, and the coalesced events it reports.
//! - [`socket`] — the session socket lifecycle: path resolution, exclusive
//!   ownership, stale-socket cleanup, and unlink on drop.
//! - [`config`] — server-side configuration-file I/O, atomic reload, and the
//!   `SIGHUP` source that asks the daemon owner to reload.
//! - [`conn`] — one client connection: the versioned handshake, refusals, and
//!   the snapshot a freshly attached client is brought up to date with.
//! - [`daemon`] — the serving loop that owns the socket and outlives every
//!   client attached to it. It holds a [`SessionHandle`] and no session state
//!   of its own.
//!
//! Damage coalescing with fan-out to several clients lands in M1-04.

pub mod config;
pub mod conn;
pub mod daemon;
pub mod damage;
pub mod launch;
pub mod pty;
pub mod session;
pub mod socket;

pub use config::{
    ConfigFile, ConfigLoadError, ConfigManager, ConfigPathError, InitialConfig, Reload,
    ReloadWatch, load_from_environment, resolve_config_path,
};
pub use conn::{AttachRejection, AttachRequest, Connection, accept_attach};
pub use daemon::{Daemon, DaemonError};
pub use damage::{DamageFrame, DamageTracker};
pub use launch::{Launch, login_shell};
pub use pty::{PaneSnapshot, Pty, PtyConfig, PtyError, PtyReactor, Pump};
pub use session::{
    Command, CopyModeError, Session, SessionEvent, SessionGone, SessionHandle, SessionSnapshot,
    SpawnedSession,
};
pub use socket::{Listener, NameRejection, SocketError};
