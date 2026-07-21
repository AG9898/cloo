//! The *outer* terminal's geometry: how large the terminal cloo draws into is.
//!
//! It belongs to the client rather than the server, as does what that terminal
//! can draw — see [`capabilities`](crate::capabilities), which owns the other
//! half of "what the client knows about the terminal it is sitting in".
//!
//! ```no_run
//! use std::io::stdout;
//! use std::os::fd::AsFd;
//!
//! # fn example() -> std::io::Result<()> {
//! let size = cloo_client::outer::window_size(stdout().as_fd())?;
//! # let _ = size;
//! # Ok(())
//! # }
//! ```

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

use cloo_proto::Size;

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

/// The outer terminal's geometry right now.
///
/// Stdout is asked first because that is where frames are written, and stdin is
/// the fallback for the case where output was redirected but the session is
/// still interactive. A terminal that reports nothing gets [`FALLBACK_SIZE`]
/// rather than an error: refusing to draw over an unanswered `ioctl` would be a
/// worse failure than drawing at a conventional 80x24.
#[must_use]
pub fn current_size() -> Size {
    use std::os::fd::AsFd;
    window_size(io::stdout().as_fd())
        .or_else(|_| window_size(io::stdin().as_fd()))
        .unwrap_or(FALLBACK_SIZE)
}

/// Substitutes [`FALLBACK_SIZE`] for a degenerate report.
fn size_from_winsize(cols: u16, rows: u16) -> Size {
    if cols == 0 || rows == 0 {
        return FALLBACK_SIZE;
    }
    Size::new(cols, rows)
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
}
