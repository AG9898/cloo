//! Session, tab, and pane model: the layout tree, keymap, profiles, pane
//! metadata, and configuration.
//!
//! `cloo-core` performs **no I/O**. Anything that reads a file or a socket
//! belongs in `cloo-server` or `cloo-client` instead. Everything here is pure
//! and unit-testable without a terminal.
//!
//! The layout tree lands in M0-03; layout is always stored as ratios, never as
//! cell counts.
