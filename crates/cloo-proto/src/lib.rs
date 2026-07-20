//! Wire types, length framing, and the versioned handshake for cloo.
//!
//! This crate is the bottom of the dependency graph: it knows nothing about
//! PTYs, terminal emulation, or rendering. Every type here crosses the Unix
//! socket between `cloo-server` and `cloo-client`.
//!
//! Contents land in M0-02. See `docs/ARCHITECTURE.md` for the protocol shape.
