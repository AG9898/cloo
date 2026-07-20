//! The cloo daemon: Unix socket listener, PTY reactor, and damage tracking.
//!
//! The server owns all authoritative state — PTYs, grids, scrollback, and
//! layout — and fans damage out to every attached client. It never decides what
//! anything looks like; chrome is rendered client-side.
//!
//! All session mutation funnels through a single session task over
//! `mpsc<Command>`. There is no `Mutex` on session state.
//!
//! One module today:
//!
//! - [`pty`] — the single-pane PTY reactor: pseudoterminal allocation, the
//!   child process, and the read loop that feeds a `cloo-term` grid.
//!
//! The socket lifecycle and the session task land in M1.

pub mod pty;

pub use pty::{PaneSnapshot, Pty, PtyConfig, PtyError, PtyReactor, Pump};
