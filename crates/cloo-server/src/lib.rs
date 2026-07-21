//! The cloo daemon: Unix socket listener, PTY reactor, and damage tracking.
//!
//! The server owns all authoritative state — PTYs, grids, scrollback, and
//! layout — and fans damage out to every attached client. It never decides what
//! anything looks like; chrome is rendered client-side.
//!
//! All session mutation funnels through a single session task over
//! `mpsc<Command>`. There is no `Mutex` on session state.
//!
//! Two modules today:
//!
//! - [`pty`] — the single-pane PTY reactor: pseudoterminal allocation, the
//!   child process, and the read loop that feeds a `cloo-term` grid.
//! - [`socket`] — the session socket lifecycle: path resolution, exclusive
//!   ownership, stale-socket cleanup, and unlink on drop.
//!
//! The handshake over that socket lands in M1-02 and the session task in M1-03.

pub mod pty;
pub mod socket;

pub use pty::{PaneSnapshot, Pty, PtyConfig, PtyError, PtyReactor, Pump};
pub use socket::{Listener, NameRejection, SocketError};
