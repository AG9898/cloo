//! Noticing that the outer terminal changed size.
//!
//! `SIGWINCH` is the only notification a terminal application gets that its
//! window changed, and it says nothing about the new geometry — the size has to
//! be asked for afterwards with `TIOCGWINSZ`. [`ResizeWatch`] is that pair, and
//! nothing else: it turns the signal into an awaitable event and answers with
//! the size the kernel now reports.
//!
//! Two properties make it safe to put in a `select!`.
//!
//! It is **cancel-safe**. The only suspension point is the underlying signal
//! receive, which is itself cancel-safe, and the size is read and recorded with
//! no `await` in between. A `ResizeWatch` dropped mid-`select!` has therefore
//! consumed nothing, so a resize that arrived cannot be lost.
//!
//! It **reports changes, not signals**. A `SIGWINCH` whose `TIOCGWINSZ` reports
//! the same geometry is swallowed rather than forwarded, because a resize
//! command costs a layout pass, a grid reflow, and a `SIGWINCH` delivered to
//! the child — and a child redrawing itself for a size it already had is
//! exactly the flicker this avoids. Signals coalesce in the kernel anyway, so a
//! fast drag produces far fewer events than it does intermediate sizes.
//!
//! ```no_run
//! use cloo_client::outer::current_size;
//! use cloo_client::resize::ResizeWatch;
//!
//! # async fn example() -> std::io::Result<()> {
//! let mut resizes = ResizeWatch::new(current_size())?;
//! let new_size = resizes.changed().await;
//! # let _ = new_size;
//! # Ok(())
//! # }
//! ```

use std::io;

use cloo_proto::Size;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::outer::current_size;

/// A `SIGWINCH` watcher that reports the outer terminal's new size.
#[derive(Debug)]
pub struct ResizeWatch {
    signal: Signal,
    last: Size,
}

impl ResizeWatch {
    /// Starts watching, treating `current` as the size already in effect.
    ///
    /// Must be called from inside a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Returns the failure to install a `SIGWINCH` handler.
    pub fn new(current: Size) -> io::Result<Self> {
        Ok(Self {
            signal: signal(SignalKind::window_change())?,
            last: current,
        })
    }

    /// The size the watcher last reported, or was constructed with.
    #[must_use]
    pub fn last(&self) -> Size {
        self.last
    }

    /// Waits for the outer terminal's geometry to actually change.
    ///
    /// Cancel-safe: see the module docs. A watcher whose signal source has gone
    /// away never resolves rather than reporting a spurious size, because a
    /// resize that cannot be detected is not the same as a resize to the size
    /// that happens to be current.
    pub async fn changed(&mut self) -> Size {
        loop {
            if self.signal.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
            // No `await` from here to the return, which is what keeps the whole
            // future cancel-safe.
            let size = current_size();
            if size != self.last {
                self.last = size;
                return size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Delivering a real `SIGWINCH` to the test runner would race every other
    // test in the process, so the signal path is exercised from the binary
    // against a real pseudoterminal in `crates/cloo/tests/cli.rs` — see
    // docs/TESTING.md.

    #[tokio::test]
    async fn a_watcher_starts_from_the_size_it_was_given() {
        let watch = ResizeWatch::new(Size::new(100, 40)).expect("a handler must install");
        assert_eq!(watch.last(), Size::new(100, 40));
    }

    #[tokio::test]
    async fn nothing_is_reported_without_a_signal() {
        let mut watch = ResizeWatch::new(Size::new(1, 1)).expect("a handler must install");
        // The recorded size is deliberately one no terminal reports, so a
        // watcher that answered from `TIOCGWINSZ` alone would resolve here.
        let timeout =
            tokio::time::timeout(std::time::Duration::from_millis(50), watch.changed()).await;
        assert!(timeout.is_err(), "a resize was reported without a SIGWINCH");
    }
}
