//! The cloo daemon: Unix socket listener, PTY reactor, and damage tracking.
//!
//! The server owns all authoritative state — PTYs, grids, scrollback, and
//! layout — and fans damage out to every attached client. It never decides what
//! anything looks like; chrome is rendered client-side.
//!
//! All session mutation funnels through a single session task over
//! `mpsc<Command>`. There is no `Mutex` on session state.
//!
//! Five modules today:
//!
//! - [`pty`] — the single-pane PTY reactor: pseudoterminal allocation, the
//!   child process, and the read loop that feeds a `cloo-term` grid.
//! - [`session`] — the session task: the single `mpsc<Command>` every mutation
//!   arrives on, the layout pass, and the coalesced events it reports.
//! - [`socket`] — the session socket lifecycle: path resolution, exclusive
//!   ownership, stale-socket cleanup, and unlink on drop.
//! - [`conn`] — one client connection: the versioned handshake, refusals, and
//!   the snapshot a freshly attached client is brought up to date with.
//! - [`daemon`] — the serving loop that owns the socket and outlives every
//!   client attached to it. It holds a [`SessionHandle`] and no session state
//!   of its own.
//!
//! Damage coalescing with fan-out to several clients lands in M1-04.

pub mod conn;
pub mod daemon;
pub mod damage;
pub mod pty;
pub mod session;
pub mod socket;

pub use conn::{AttachRejection, AttachRequest, Connection, accept_attach};
pub use daemon::{Daemon, DaemonError};
pub use damage::{DamageFrame, DamageTracker};
pub use pty::{PaneSnapshot, Pty, PtyConfig, PtyError, PtyReactor, Pump};
pub use session::{
    Command, Session, SessionEvent, SessionGone, SessionHandle, SessionSnapshot, SpawnedSession,
};
pub use socket::{Listener, NameRejection, SocketError};
