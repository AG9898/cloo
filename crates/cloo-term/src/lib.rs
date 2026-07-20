//! Thin wrapper over the terminal emulation backend.
//!
//! This is the **only** crate in the workspace permitted to import
//! `alacritty_terminal`, and it must never leak that crate's types across its
//! public API. The pin plus this boundary is the entire mitigation for upstream
//! API churn — see `docs/DECISIONS.md` RESOLVED-02.
//!
//! The public surface is deliberately narrow: feed bytes, read cells, resize,
//! access scrollback. Contents land in M0-04.
