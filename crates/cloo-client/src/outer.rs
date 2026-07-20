//! What the client knows about the *outer* terminal: its size and what it can
//! draw.
//!
//! Both belong to the client rather than the server. The server never learns
//! what the user's terminal is capable of beyond the [`TermCaps`] the client
//! reports at attach, which is what keeps a capability difference between two
//! attached clients from becoming session state.
//!
//! Capability detection is a pure function of the environment
//! ([`caps_from_env`]) so it is testable without touching the process's real
//! environment, with [`detect_caps`] as the thin wrapper that reads it.
//!
//! ```no_run
//! use std::io::stdout;
//! use std::os::fd::AsFd;
//!
//! # fn example() -> std::io::Result<()> {
//! let size = cloo_client::outer::window_size(stdout().as_fd())?;
//! let caps = cloo_client::outer::detect_caps();
//! # let _ = (size, caps);
//! # Ok(())
//! # }
//! ```

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use cloo_proto::{Size, TermCaps};

/// The fallback geometry for a terminal that reports none.
///
/// A `winsize` of zero rows or columns is legal and happens under some CI
/// runners and multiplexers. Rendering into a zero-sized grid would draw
/// nothing at all, so a conventional 80x24 is substituted instead.
pub const FALLBACK_SIZE: Size = Size::new(80, 24);

/// Asks the kernel how large `fd`'s terminal is.
///
/// # Errors
///
/// Returns the `TIOCGWINSZ` failure, which is what a non-terminal descriptor
/// produces.
pub fn window_size(fd: BorrowedFd<'_>) -> io::Result<Size> {
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is a valid borrowed descriptor for the duration of the call,
    // and `TIOCGWINSZ` writes exactly one `winsize` through the pointer, which
    // refers to live local storage.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ as _, &raw mut winsize) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(size_from_winsize(winsize.ws_col, winsize.ws_row))
}

/// Substitutes [`FALLBACK_SIZE`] for a degenerate report.
fn size_from_winsize(cols: u16, rows: u16) -> Size {
    if cols == 0 || rows == 0 {
        return FALLBACK_SIZE;
    }
    Size::new(cols, rows)
}

/// Reads the process environment and decides what the outer terminal can draw.
#[must_use]
pub fn detect_caps() -> TermCaps {
    caps_from_env(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

/// The pure form of [`detect_caps`].
///
/// Only capabilities that can be established without writing a query sequence
/// to the terminal and waiting for a reply are decided here. Everything else
/// stays false: a client must never claim a capability it has not established,
/// because the documented fallback is always safe and a wrongly claimed
/// capability corrupts the user's screen.
#[must_use]
pub fn caps_from_env(term: Option<&str>, colorterm: Option<&str>) -> TermCaps {
    let term = term.unwrap_or("");
    let colorterm = colorterm.unwrap_or("");

    // A terminal that says it is "dumb", or says nothing at all, gets the
    // most conservative treatment available.
    if term.is_empty() || term == "dumb" {
        return TermCaps::default();
    }

    let truecolor = colorterm.eq_ignore_ascii_case("truecolor")
        || colorterm.eq_ignore_ascii_case("24bit")
        || term.contains("truecolor")
        || term.contains("direct");

    TermCaps {
        truecolor,
        // Universal enough among terminals that report a `TERM` at all, and
        // harmless where unsupported: an unrecognized private mode is ignored.
        bracketed_paste: true,
        sgr_mouse: true,
        focus_events: true,
        ..TermCaps::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reported_size_is_used_as_is() {
        assert_eq!(size_from_winsize(120, 40), Size::new(120, 40));
    }

    #[test]
    fn a_degenerate_size_falls_back_rather_than_rendering_nothing() {
        assert_eq!(size_from_winsize(0, 40), FALLBACK_SIZE);
        assert_eq!(size_from_winsize(120, 0), FALLBACK_SIZE);
        assert_eq!(size_from_winsize(0, 0), FALLBACK_SIZE);
    }

    #[test]
    fn a_dumb_or_absent_terminal_claims_nothing() {
        assert_eq!(caps_from_env(None, Some("truecolor")), TermCaps::default());
        assert_eq!(caps_from_env(Some(""), None), TermCaps::default());
        assert_eq!(
            caps_from_env(Some("dumb"), Some("truecolor")),
            TermCaps::default()
        );
    }

    #[test]
    fn colorterm_is_what_establishes_truecolor() {
        assert!(caps_from_env(Some("xterm-256color"), Some("truecolor")).truecolor);
        assert!(caps_from_env(Some("xterm-256color"), Some("24bit")).truecolor);
        assert!(caps_from_env(Some("xterm-256color"), Some("TrueColor")).truecolor);
        assert!(!caps_from_env(Some("xterm-256color"), None).truecolor);
        assert!(!caps_from_env(Some("xterm-256color"), Some("")).truecolor);
    }

    #[test]
    fn a_direct_color_term_entry_also_establishes_truecolor() {
        assert!(caps_from_env(Some("xterm-direct"), None).truecolor);
    }

    #[test]
    fn unestablished_capabilities_stay_false() {
        let caps = caps_from_env(Some("xterm-256color"), Some("truecolor"));
        assert!(!caps.extended_keys, "needs a query and a reply");
        assert!(!caps.clipboard_osc52);
        assert!(!caps.hyperlinks);
        assert!(!caps.graphics);
    }
}
