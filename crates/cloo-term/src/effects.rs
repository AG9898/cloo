//! Typed outer-terminal effects observed while emulating a pane.
//!
//! These mirror the wire types in `cloo-proto` without depending on that crate:
//! both crates are leaves in the workspace layering. The server maps between
//! the two owned vocabularies when it carries an effect to a client. The model
//! intentionally has no raw OSC or DCS payload, so a pane cannot ask cloo to
//! forward arbitrary terminal bytes around the renderer.

/// A narrowly allowlisted change a pane requests of an attached client's
/// terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OuterTerminalEffect {
    /// Set the outer terminal's window title.
    SetTitle(String),
    /// Restore the outer terminal's default window title.
    ResetTitle,
    /// Store text in one named clipboard target.
    ClipboardStore {
        /// Where the text should be stored.
        target: ClipboardTarget,
        /// Plain UTF-8 text to store.
        text: String,
    },
    /// Make a URI available as a terminal hyperlink.
    Hyperlink {
        /// The link destination.
        uri: String,
    },
    /// Ask the terminal to present an application notification.
    Notification {
        /// Short notification heading.
        title: String,
        /// Notification body.
        body: String,
    },
    /// Update the terminal's progress presentation.
    Progress(ProgressState),
    /// Report the only graphics outcome cloo can currently model safely.
    Graphics(GraphicsEffect),
}

/// A clipboard target cloo permits an effect to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardTarget {
    /// The regular clipboard (OSC 52's `c` selection).
    Clipboard,
    /// The primary selection (OSC 52's `p` selection).
    PrimarySelection,
}

/// A terminal-progress state with no renderer-specific payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// Remove a previous progress indication.
    Clear,
    /// Show activity whose completion is unknown.
    Indeterminate,
    /// Show a completion percentage from 0 through 100.
    Value(u8),
    /// Show a failed progress state.
    Error,
}

/// The safe graphics model for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsEffect {
    /// The pane remains usable, but no inline graphic can be rendered.
    Unavailable,
}
