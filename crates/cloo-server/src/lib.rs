//! The cloo daemon: Unix socket listener, PTY reactor, and damage tracking.
//!
//! The server owns all authoritative state — PTYs, grids, scrollback, and
//! layout — and fans damage out to every attached client. It never decides what
//! anything looks like; chrome is rendered client-side.
//!
//! All session mutation funnels through a single session task over
//! `mpsc<Command>`. There is no `Mutex` on session state.
//!
//! Four modules today:
//!
//! - [`pty`] — the single-pane PTY reactor: pseudoterminal allocation, the
//!   child process, and the read loop that feeds a `cloo-term` grid.
//! - [`socket`] — the session socket lifecycle: path resolution, exclusive
//!   ownership, stale-socket cleanup, and unlink on drop.
//! - [`conn`] — one client connection: the versioned handshake, refusals, and
//!   the snapshot a freshly attached client is brought up to date with.
//! - [`daemon`] — the serving loop that owns the pane and outlives every
//!   client attached to it.
//!
//! The session task that serializes input and resize through `mpsc<Command>`
//! lands in M1-03, and damage coalescing with fan-out to several clients in
//! M1-04.

pub mod conn;
pub mod daemon;
pub mod pty;
pub mod socket;

pub use conn::{AttachRejection, AttachRequest, Connection, accept_attach};
pub use daemon::{Daemon, DaemonError};
pub use pty::{PaneSnapshot, Pty, PtyConfig, PtyError, PtyReactor, Pump};
pub use socket::{Listener, NameRejection, SocketError};
