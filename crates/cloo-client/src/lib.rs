//! The cloo attach client: raw mode, renderer, theming, and input encoding.
//!
//! The client holds only a cache of the visible cell grid — never authoritative
//! session state. **All chrome is rendered here**, which is why theming never
//! touches the server.
//!
//! Raw mode and termios changes must be restored on every exit path, including
//! panic and signal.
//!
//! Raw-mode guarding and grid rendering land in M0-06.
