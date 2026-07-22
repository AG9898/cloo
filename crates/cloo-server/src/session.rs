//! The session task: the one thing that mutates session state.
//!
//! Everything that changes a session — a keystroke, a resize, a future split —
//! arrives here as a [`Command`] on a single `mpsc` channel and is applied in
//! arrival order by one task. There is **no `Mutex` on session state**, and
//! there is no second path to the grid or the PTY: a [`SessionHandle`] is a
//! sender and nothing more, so a caller cannot reach past it.
//!
//! That serialization is not lock avoidance for its own sake. Resize is a
//! three-way race between the grid, the child's `TIOCSWINSZ`, and the
//! application's own `SIGWINCH` handling, and the only way to reason about it
//! is for one actor to do both halves in a fixed order. [`Session::resize`]
//! runs **one layout pass** — `Layout::resolve` — and drives every pane's
//! geometry from its output, so the rect a client is told about and the
//! `winsize` the child is given can never come from two different computations.
//!
//! Output flows the other way as [`SessionEvent`]. `Output` is a *level*, not
//! an edge: the channel holds one, and a session producing bytes faster than
//! anyone reads them coalesces into a single pending notification rather than
//! one per PTY read. The reader then asks for a [`SessionSnapshot`] whenever it
//! is ready to draw, which is what keeps the render rate capped by a timer
//! rather than by the child.
//!
//! The task pumps every pane's PTY for its whole life, attached or not. A
//! session that only read while someone was watching would lose everything
//! written in between, and a reattaching client would find a stale grid.
//!
//! A session owns one PTY per pane, and the layout tree is the only record of
//! which panes exist. [`Session::split`] and [`Session::close`] are what keep
//! the two in step: a split that the layout refuses spawns nothing, a child
//! that fails to spawn rolls the layout back, and a close drops the pane's
//! reactor — which kills and reaps its child — in the same turn that collapses
//! its parent. There is no intermediate state in which a pane exists in one and
//! not the other, because nothing else runs between them.
//!
//! Focus and zoom sit on top of that without disturbing it.
//! [`Command::MoveFocus`] asks the layout which pane a user sees in a
//! direction, and [`Command::ToggleZoom`] sets a view flag the layout tree
//! never sees. Both then run the same geometry pass a resize does — which is
//! the reason zoom cannot restart a PTY: the only thing it can do to a child is
//! change its `winsize`, and a hidden pane's child is not even told that.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::process::ExitStatus;
use std::task::Poll;

use cloo_core::error::LayoutError;
use cloo_core::layout::Side;
use cloo_core::pane::{AttentionSource, AttentionState, PaneMeta};
use cloo_core::session::Session as SessionModel;
use cloo_core::tab::TabName;
use cloo_core::{CopyMode, CopyMotion, SearchDirection, SearchError};
use cloo_proto::{
    ClipboardTarget, CopyModeState, CopySelection as WireCopySelection, Direction, GraphicsEffect,
    MouseButton, MouseEvent, MouseKind, MouseTracking, OuterTerminalEffect, PaneAttention, PaneId,
    PaneInfo, PaneModes, PaneRect, ProgressState, ScrollPoint, SearchMatch as WireSearchMatch,
    SessionId, Size, TabId, TabSummary,
};
use cloo_term::TermSize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::launch::Launch;
use crate::pty::{PaneSnapshot, PtyConfig, PtyError, PtyReactor, Pump};

/// How many commands may be in flight before a sender waits.
///
/// Deep enough that a burst of keystrokes never blocks the caller, shallow
/// enough that a wedged session applies backpressure instead of growing without
/// bound.
const COMMAND_QUEUE: usize = 64;

/// The share of a pane a split leaves with the pane that was split.
///
/// Down the middle, which is what a keybinding means by "split". A caller that
/// wants another ratio passes one.
pub const EVEN_SPLIT: f32 = 0.5;

/// How many times a pane's child is polled for a status at end of file before
/// giving up.
///
/// End of file means the child closed the slave, which for an ordinary process
/// is part of exiting — but the kernel closes the descriptors a hair before the
/// process becomes reapable, so a single non-blocking check can lose the race.
/// A short bounded spin closes that window; the common case reaps on the first
/// try, and a child that closed its terminal and kept running falls through
/// after a trivial delay rather than blocking the actor forever.
const EXIT_REAP_TRIES: usize = 256;

/// Everything that mutates a session.
///
/// Deliberately the whole vocabulary: if it is not here, it does not change
/// session state.
#[derive(Debug)]
pub enum Command {
    /// Keyboard bytes for the focused pane's child, already encoded.
    Input(Vec<u8>),
    /// Text the user pasted, as text. Bracketed here or not at all: whether the
    /// child wants brackets is a mode only this side can see.
    Paste(Vec<u8>),
    /// The client gained or lost focus. Reported to the child only if it asked.
    Focus(bool),
    /// A mouse event the client decided belongs to the application. Encoded
    /// here, in the scheme and at the level the child negotiated.
    Mouse(MouseEvent),
    /// The session area changed. Triggers one layout pass and one `TIOCSWINSZ`
    /// per pane.
    Resize(Size),
    /// Splits the focused pane along `dir`, spawning a child in the new pane.
    Split {
        /// The axis the focused pane is divided along.
        dir: Direction,
        /// The share of it kept by the pane being split.
        ratio: f32,
        /// What to launch in the new pane. `None` repeats the launch the
        /// session was created with, which is what an unqualified "split" means.
        ///
        /// Boxed because it is much larger than the other variants and this
        /// enum rides an `mpsc` on every keystroke.
        launch: Option<Box<Launch>>,
        /// The new pane's id, or why no pane was created.
        reply: oneshot::Sender<Result<PaneId, PaneError>>,
    },
    /// Closes a pane, killing its child and collapsing its parent split.
    Close {
        /// The pane to close.
        pane: PaneId,
        /// Whether the pane was closed, or why it was not.
        reply: oneshot::Sender<Result<(), PaneError>>,
    },
    /// Moves focus to the pane on one side of the focused one.
    ///
    /// No reply: there is nothing to refuse. An edge pane asked to move past
    /// the edge simply stays where it is, because the alternative — wrapping
    /// around to the far side — moves a user's attention somewhere they were
    /// not looking.
    MoveFocus(Side),
    /// Shows the focused pane alone at the full area, or undoes that.
    ToggleZoom,
    /// Creates a new tab with one pane running the session's default launch.
    NewTab(oneshot::Sender<Result<TabId, PaneError>>),
    /// Closes the active tab and every pane it owns.
    CloseTab(oneshot::Sender<Result<(), TabError>>),
    /// Activates the tab after the active one, wrapping at the end.
    NextTab,
    /// Activates the tab before the active one, wrapping at the beginning.
    PrevTab,
    /// Renames the active tab.
    RenameTab {
        /// The validated replacement title.
        name: TabName,
        /// Reports a model rejection to the caller.
        reply: oneshot::Sender<Result<(), TabError>>,
    },
    /// Records a pane's attention state and where it came from.
    ///
    /// The single serialized path for attention: a bell, a child exit, an
    /// explicit user mark, and an opt-in adapter all arrive here and are applied
    /// in arrival order by the one task, so nothing races the coalescing rule in
    /// [`Attention::set`](cloo_core::pane::Attention::set). An update naming a
    /// pane that has since closed is dropped, exactly as a stale mouse event is.
    SetAttention {
        /// The pane whose state is being reported.
        pane: PaneId,
        /// The state the source is claiming.
        state: AttentionState,
        /// Who is making the claim.
        source: AttentionSource,
    },
    /// Marks a pane's current attention state as seen, taking it out of the
    /// attention queue without changing what the state is.
    AcknowledgeAttention {
        /// The pane the user has looked at.
        pane: PaneId,
    },
    /// Starts copy mode on the focused pane, keeping its cursor and search
    /// state in the session actor rather than in one attached client.
    EnterCopyMode,
    /// Leaves copy mode on the focused pane and resumes following live output.
    ExitCopyMode,
    /// Applies a vim-like copy cursor motion to the focused pane.
    CopyMotion(CopyMotion),
    /// Starts a linear visual selection at the copy cursor.
    BeginCopySelection,
    /// Clears the focused pane's visual selection without moving its cursor.
    ClearCopySelection,
    /// Compiles and runs a regex against the focused pane's retained
    /// scrollback, reporting parse errors without ending the session task.
    SearchCopy {
        /// Regex text supplied by the user.
        query: String,
        /// Direction in which the result set is entered.
        direction: SearchDirection,
        /// Whether a match was found, or the parse failure.
        reply: oneshot::Sender<Result<bool, CopyModeError>>,
    },
    /// Moves to another result of the active copy-mode search.
    NextCopyMatch(SearchDirection),
    /// Extracts the focused pane's copy-mode selection as a typed clipboard
    /// effect for the one client that asked.
    ///
    /// It replies rather than broadcasting: a copy is one user's explicit act
    /// on one terminal, and fanning the text out would store one client's
    /// selection in every attached terminal's clipboard. Reading a selection
    /// changes no session state at all.
    CopySelection {
        /// Which clipboard the user asked for.
        target: ClipboardTarget,
        /// The pane copied from and the effect to apply, or `None` when
        /// nothing is selected.
        reply: oneshot::Sender<Option<(PaneId, OuterTerminalEffect)>>,
    },
    /// Asks for the current picture. The reply channel is how a reader gets
    /// state out without holding a reference to it.
    Snapshot(oneshot::Sender<SessionSnapshot>),
}

/// Why a pane was not created or not closed.
///
/// The two variants are the two halves that have to agree. [`Layout`](Self::Layout)
/// means the layout refused before anything was spawned; [`Spawn`](Self::Spawn)
/// means it accepted and the child could not be started, in which case the
/// layout has already been rolled back. Either way the session is exactly as it
/// was before the command arrived.
#[derive(Debug)]
pub enum PaneError {
    /// The layout refused the operation. Nothing was spawned or dropped.
    Layout(LayoutError),
    /// The child could not be started. The layout was rolled back.
    Spawn(PtyError),
    /// The session task is no longer running.
    Gone,
}

/// Why a tab lifecycle operation could not complete.
#[derive(Debug)]
pub enum TabError {
    /// The pure tab model rejected the operation.
    Model(cloo_core::SessionError),
    /// The session task is no longer running.
    Gone,
}

/// Why a copy-mode operation could not complete.
#[derive(Debug)]
pub enum CopyModeError {
    /// The user-supplied regex did not compile. The prior search state is kept.
    Search(SearchError),
    /// The session task ended before applying the operation.
    Gone,
}

impl fmt::Display for CopyModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Search(error) => write!(f, "{error}"),
            Self::Gone => write!(f, "{SessionGone}"),
        }
    }
}

impl std::error::Error for CopyModeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Search(error) => Some(error),
            Self::Gone => None,
        }
    }
}

impl From<SessionGone> for CopyModeError {
    fn from(_: SessionGone) -> Self {
        Self::Gone
    }
}

impl fmt::Display for TabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model(error) => write!(f, "{error}"),
            Self::Gone => write!(f, "{SessionGone}"),
        }
    }
}

impl std::error::Error for TabError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Model(error) => Some(error),
            Self::Gone => None,
        }
    }
}

impl From<SessionGone> for TabError {
    fn from(_: SessionGone) -> Self {
        Self::Gone
    }
}

impl fmt::Display for PaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(e) => write!(f, "{e}"),
            Self::Spawn(e) => write!(f, "the pane's child could not be started: {e}"),
            Self::Gone => write!(f, "{SessionGone}"),
        }
    }
}

impl std::error::Error for PaneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Layout(e) => Some(e),
            Self::Spawn(e) => Some(e),
            Self::Gone => None,
        }
    }
}

impl From<SessionGone> for PaneError {
    fn from(_: SessionGone) -> Self {
        Self::Gone
    }
}

/// What a session tells whoever is listening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Something changed and the current snapshot differs from the last one
    /// drawn. Coalesced: at most one is pending at a time.
    Output,
    /// A pane requested one typed, client-local outer-terminal effect.
    ///
    /// This is not coalesced with output: two title changes are two ordered
    /// requests, even if they leave the grid identical. The emulator queue may
    /// safely suppress an effect before it reaches this point, but a value
    /// delivered here reaches the daemon exactly once.
    Effect {
        /// Pane whose application emitted the request.
        pane: PaneId,
        /// The wire-owned, allowlisted request.
        effect: OuterTerminalEffect,
    },
    /// The session's child exited. The task stays alive and still answers
    /// [`Command::Snapshot`], so the child's last words can still be drawn.
    Exited,
}

/// The session task is no longer running.
///
/// Not an error a user did anything about: it means the child exited and the
/// task returned, or the runtime is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionGone;

impl fmt::Display for SessionGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the session task is no longer running")
    }
}

impl std::error::Error for SessionGone {}

/// The active-tab picture of a session at one instant.
///
/// Geometry and contents come from the same pass over the same state, which is
/// what lets a client apply them together without ever holding rows it has
/// nowhere to put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The tab whose layout and focused-pane grid this snapshot describes.
    pub tab: TabId,
    /// The whole tab bar, in display order. This is projection data: the
    /// session actor remains the sole owner of tab state.
    pub tabs: Vec<TabSummary>,
    /// The session's area, in cells.
    pub area: Size,
    /// Every visible pane in the active tab and where it sits, from one
    /// [`Layout::resolve`](cloo_core::Layout::resolve).
    pub panes: Vec<PaneRect>,
    /// The focused pane, which is the one [`pane`](Self::pane) describes.
    pub focused: PaneId,
    /// The pane shown alone at the full area, if any. Always the focused pane
    /// while it is set: zoom follows focus rather than pinning it.
    pub zoomed: Option<PaneId>,
    /// Who every visible pane is: profile, name, task label, and working
    /// directory.
    ///
    /// In the same order as [`panes`](Self::panes), and always describing the
    /// same set. Explicit metadata every time — nothing in this vector is
    /// derived from what a child printed.
    pub metas: Vec<PaneInfo>,
    /// Every pane's attention state, its provenance, and whether it has been
    /// acknowledged.
    ///
    /// Projected from the same [`Layout::resolve`] pass as [`metas`](Self::metas),
    /// so a client is never told a pane's attention without also being told who
    /// the pane is. An uninstrumented pane appears here as
    /// [`AttentionState::Unknown`](cloo_core::pane::AttentionState::Unknown)
    /// rather than being omitted.
    pub attention: Vec<PaneAttention>,
    /// The active pane's copy and search state, if the server is in copy mode.
    ///
    /// It is projection data only: the actor remains the sole owner, and a
    /// reattaching client receives the same selection and matches it left.
    pub copy_mode: Option<CopyModeState>,
    /// The focused pane's contents.
    pub pane: PaneSnapshot,
    /// The input modes the focused pane's application has negotiated. A client
    /// cannot observe these for itself, and it needs them to decide whether a
    /// mouse event is the application's or cloo's chrome's.
    pub modes: PaneModes,
}

/// A sender into a session task.
///
/// Cloneable, because one task per attached client is the shape M1-04 fans out
/// to and every one of them funnels through this single channel.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    commands: mpsc::Sender<Command>,
}

impl SessionHandle {
    /// Forwards keyboard bytes to the focused pane.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn input(&self, bytes: Vec<u8>) -> Result<(), SessionGone> {
        self.send(Command::Input(bytes)).await
    }

    /// Hands the focused pane pasted text.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn paste(&self, text: Vec<u8>) -> Result<(), SessionGone> {
        self.send(Command::Paste(text)).await
    }

    /// Tells the focused pane the client gained or lost focus.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn focus(&self, focused: bool) -> Result<(), SessionGone> {
        self.send(Command::Focus(focused)).await
    }

    /// Forwards a mouse event the client routed to the application.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn mouse(&self, event: MouseEvent) -> Result<(), SessionGone> {
        self.send(Command::Mouse(event)).await
    }

    /// Tells the session its area changed.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn resize(&self, area: Size) -> Result<(), SessionGone> {
        self.send(Command::Resize(area)).await
    }

    /// Splits the focused pane, spawning a child in the new one.
    ///
    /// The new pane becomes the focused one, which is what makes a split
    /// followed by typing do what a user means by it.
    ///
    /// # Errors
    ///
    /// Returns [`PaneError::Layout`] if the split was refused — most often
    /// [`LayoutError::TooSmall`] — [`PaneError::Spawn`] if the child could not
    /// be started, and [`PaneError::Gone`] if the session task has ended. The
    /// session is unchanged in every one of those cases.
    pub async fn split(&self, dir: Direction, ratio: f32) -> Result<PaneId, PaneError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Split {
            dir,
            ratio,
            launch: None,
            reply,
        })
        .await?;
        answer.await.map_err(|_| PaneError::Gone)?
    }

    /// Splits the focused pane and launches `launch` in the new one.
    ///
    /// The explicit form of [`split`](Self::split): the new pane runs the named
    /// profile, under the name, task label, and working directory the user gave,
    /// and carries all of that as metadata from the moment it exists. A plain
    /// split repeats the session's own launch instead.
    ///
    /// # Errors
    ///
    /// As [`split`](Self::split). A program that is not on `PATH` is a
    /// [`PaneError::Spawn`] whose message names it, and the layout has already
    /// been rolled back when it arrives.
    pub async fn launch(
        &self,
        dir: Direction,
        ratio: f32,
        launch: Launch,
    ) -> Result<PaneId, PaneError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Split {
            dir,
            ratio,
            launch: Some(Box::new(launch)),
            reply,
        })
        .await?;
        answer.await.map_err(|_| PaneError::Gone)?
    }

    /// Splits the focused pane down the middle.
    ///
    /// # Errors
    ///
    /// As [`split`](Self::split).
    pub async fn split_even(&self, dir: Direction) -> Result<PaneId, PaneError> {
        self.split(dir, EVEN_SPLIT).await
    }

    /// Closes a pane, killing its child and collapsing its parent split.
    ///
    /// # Errors
    ///
    /// Returns [`PaneError::Layout`] if the pane is unknown or is its tab's
    /// last one — a tab with no panes is closed rather than represented — and
    /// [`PaneError::Gone`] if the session task has ended.
    pub async fn close(&self, pane: PaneId) -> Result<(), PaneError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Close { pane, reply }).await?;
        answer.await.map_err(|_| PaneError::Gone)?
    }

    /// Moves focus one pane in a direction, if there is a pane there.
    ///
    /// Asking to move past the edge of the layout is not an error and does
    /// nothing. Ordering is the channel's: a [`snapshot`](Self::snapshot) sent
    /// afterwards sees the move.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn move_focus(&self, side: Side) -> Result<(), SessionGone> {
        self.send(Command::MoveFocus(side)).await
    }

    /// Shows the focused pane alone at the full area, or undoes that.
    ///
    /// No PTY is created or destroyed either way, and no split ratio changes —
    /// zoom is a view over the same tree.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn toggle_zoom(&self) -> Result<(), SessionGone> {
        self.send(Command::ToggleZoom).await
    }

    /// Creates a tab running the session's default launch and makes it active.
    ///
    /// # Errors
    ///
    /// Returns [`PaneError::Spawn`] if its child could not start, without
    /// adding a tab, or [`PaneError::Gone`] if the session task has ended.
    pub async fn new_tab(&self) -> Result<TabId, PaneError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::NewTab(reply)).await?;
        answer.await.map_err(|_| PaneError::Gone)?
    }

    /// Closes the active tab and every PTY it owns.
    ///
    /// # Errors
    ///
    /// The final tab is refused by the pure session model, and a closed session
    /// reports [`TabError::Gone`].
    pub async fn close_tab(&self) -> Result<(), TabError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::CloseTab(reply)).await?;
        answer.await.map_err(|_| TabError::Gone)?
    }

    /// Activates the next tab, wrapping at the end of the tab bar.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn next_tab(&self) -> Result<(), SessionGone> {
        self.send(Command::NextTab).await
    }

    /// Activates the previous tab, wrapping at the beginning of the tab bar.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn prev_tab(&self) -> Result<(), SessionGone> {
        self.send(Command::PrevTab).await
    }

    /// Renames the active tab.
    ///
    /// # Errors
    ///
    /// Returns a model rejection or [`TabError::Gone`] if the session ended.
    pub async fn rename_tab(&self, name: TabName) -> Result<(), TabError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::RenameTab { name, reply }).await?;
        answer.await.map_err(|_| TabError::Gone)?
    }

    /// Reports a pane's attention state and where the report came from.
    ///
    /// The one serialized path for attention. A report naming a pane that has
    /// closed is dropped by the session task; re-reporting a state a pane
    /// already holds keeps any acknowledgment, so a chatty source cannot refill
    /// a queue the user just cleared. Ordering is the channel's: a
    /// [`snapshot`](Self::snapshot) sent afterwards sees the update.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn set_attention(
        &self,
        pane: PaneId,
        state: AttentionState,
        source: AttentionSource,
    ) -> Result<(), SessionGone> {
        self.send(Command::SetAttention {
            pane,
            state,
            source,
        })
        .await
    }

    /// Marks a pane's current attention as seen, without changing the state.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn acknowledge_attention(&self, pane: PaneId) -> Result<(), SessionGone> {
        self.send(Command::AcknowledgeAttention { pane }).await
    }

    /// Starts copy mode on the focused pane.
    ///
    /// The state belongs to the session task and therefore remains live when a
    /// client detaches or another client attaches.
    pub async fn enter_copy_mode(&self) -> Result<(), SessionGone> {
        self.send(Command::EnterCopyMode).await
    }

    /// Leaves copy mode on the focused pane and returns its viewport to live
    /// output.
    pub async fn exit_copy_mode(&self) -> Result<(), SessionGone> {
        self.send(Command::ExitCopyMode).await
    }

    /// Moves the focused pane's copy cursor.
    pub async fn copy_motion(&self, motion: CopyMotion) -> Result<(), SessionGone> {
        self.send(Command::CopyMotion(motion)).await
    }

    /// Begins visual selection at the focused pane's copy cursor.
    pub async fn begin_copy_selection(&self) -> Result<(), SessionGone> {
        self.send(Command::BeginCopySelection).await
    }

    /// Clears the focused pane's visual selection.
    pub async fn clear_copy_selection(&self) -> Result<(), SessionGone> {
        self.send(Command::ClearCopySelection).await
    }

    /// Searches the focused pane's retained scrollback with a regex.
    ///
    /// An invalid expression returns [`CopyModeError::Search`] through the
    /// normal reply path; it cannot panic or take the session actor down.
    pub async fn search_copy(
        &self,
        query: impl Into<String>,
        direction: SearchDirection,
    ) -> Result<bool, CopyModeError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::SearchCopy {
            query: query.into(),
            direction,
            reply,
        })
        .await?;
        answer.await.map_err(|_| CopyModeError::Gone)?
    }

    /// Visits the next or previous result of the active copy-mode search.
    pub async fn next_copy_match(&self, direction: SearchDirection) -> Result<(), SessionGone> {
        self.send(Command::NextCopyMatch(direction)).await
    }

    /// Reads the focused pane's copy-mode selection as a typed clipboard
    /// effect, for the caller to hand to exactly one client.
    ///
    /// `None` means there was nothing selected to copy — an ordinary answer,
    /// not a failure. The caller still applies its own policy and capability
    /// gate before anything reaches a terminal.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task ended before replying.
    pub async fn copy_selection(
        &self,
        target: ClipboardTarget,
    ) -> Result<Option<(PaneId, OuterTerminalEffect)>, SessionGone> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::CopySelection { target, reply }).await?;
        answer.await.map_err(|_| SessionGone)
    }

    /// Asks for the current picture.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task ended before replying.
    pub async fn snapshot(&self) -> Result<SessionSnapshot, SessionGone> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Snapshot(reply)).await?;
        answer.await.map_err(|_| SessionGone)
    }

    async fn send(&self, command: Command) -> Result<(), SessionGone> {
        self.commands.send(command).await.map_err(|_| SessionGone)
    }
}

/// A running session task and the ends of its channels.
pub struct SpawnedSession {
    /// The only way to mutate the session.
    pub handle: SessionHandle,
    /// What the session reports. Poll it, or the session's `Exited` waits.
    pub events: mpsc::Receiver<SessionEvent>,
    /// Resolves to the child's exit status once every [`SessionHandle`] is
    /// dropped and the task has reaped it.
    pub task: JoinHandle<Result<ExitStatus, PtyError>>,
    /// The child's process id, for diagnostics and for a test that has to prove
    /// a detach did not kill it.
    pub child_id: u32,
}

/// One pane: its id, the PTY and grid behind it, and whether its child is gone.
///
/// Ownership is the whole of pane teardown. Dropping a `Pane` drops its
/// [`PtyReactor`], which closes the master and reaps the child, so removing one
/// from [`Session::panes`] is all a close has to do.
struct Pane {
    id: PaneId,
    reactor: PtyReactor,
    /// Who this pane is: the profile it was launched from and what the user
    /// called it. Explicit from the moment the pane exists, and never revised by
    /// anything the child prints.
    meta: PaneMeta,
    /// Selection and regex state over this pane's scrollback. The emulator
    /// keeps the text; this actor-owned value keeps the user's intent across
    /// client changes without letting a client mutate a grid.
    copy_mode: Option<CopyMode>,
    /// Set once this pane's PTY reached end of file. Its grid is still drawn —
    /// a child's last words outlive it — but it is never pumped again.
    ended: bool,
}

/// One session's authoritative state, owned by one task.
///
/// The pure model's tab-local layouts and [`panes`](Self::panes) are two views
/// of one fact: the union of every tab layout names exactly the reactors held
/// here, and every mutation restores that relation before returning. `panes` is
/// never empty, because closing the final tab is refused by the model.
///
/// Not `Debug`: it owns an [`Emulator`](cloo_term::Emulator), whose grid is not,
/// and printing a session's whole scrollback would not be useful anyway.
pub struct Session {
    panes: Vec<Pane>,
    /// The pure tab and layout model. PTYs live beside it in `panes`, while the
    /// model is the one source of truth for which tab owns each pane and which
    /// tab is active.
    model: SessionModel,
    area: Size,
    /// What belongs to the *session* rather than to a profile: the environment
    /// every pane inherits and a fallback geometry. A [`Launch`] is applied over
    /// it to produce one pane's configuration.
    config: PtyConfig,
    /// What a plain [`Command::Split`] launches — the launch this session was
    /// created with, which is what "split this again" means.
    default_launch: Launch,
    ids: cloo_core::PaneIdAllocator,
    /// Where the next pump starts looking, so a loud pane cannot starve a quiet
    /// one of its turn.
    cursor: usize,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<SessionEvent>,
}

impl Session {
    /// Launches a session's first pane on a fresh PTY and puts a session task
    /// in front of it.
    ///
    /// `base` carries the session's environment and its fallback geometry;
    /// `launch` decides what actually runs and what the pane is called. The
    /// launch was validated before it got here, so the only thing left to fail
    /// is the spawn itself.
    ///
    /// Must be called from inside a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Propagates any [`PtyReactor::spawn`] failure — including a program that
    /// is not on `PATH`, whose message names it.
    pub fn spawn(
        base: &PtyConfig,
        pane: PaneId,
        launch: Launch,
    ) -> Result<SpawnedSession, PtyError> {
        let config = launch.configure(base);
        let reactor = PtyReactor::spawn(&config)?;
        let child_id = reactor.child_id();
        let area = cloo_core::grid::wire_size(config.term_size());

        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        // Capacity one on purpose: `Output` is a level, so a second one adds
        // nothing a reader would act on differently.
        let (events, event_rx) = mpsc::channel(1);

        let session = Self {
            panes: vec![Pane {
                id: pane,
                reactor,
                meta: launch.meta().clone(),
                copy_mode: None,
                ended: false,
            }],
            model: SessionModel::new(
                SessionId::new(1),
                TabName::from_pane_name(&launch.meta().name),
                pane,
            ),
            area,
            config: base.clone(),
            default_launch: launch,
            ids: cloo_core::PaneIdAllocator::resuming_after(pane),
            cursor: 0,
            commands: command_rx,
            events,
        };

        Ok(SpawnedSession {
            handle: SessionHandle { commands },
            events: event_rx,
            task: tokio::spawn(session.run()),
            child_id,
        })
    }

    /// Runs until every handle is dropped, then reaps the children.
    async fn run(mut self) -> Result<ExitStatus, PtyError> {
        loop {
            // Once every pane is at end of file there is nothing left to pump,
            // but the task keeps answering commands so the children's last
            // output can still be asked for and drawn.
            let step = if self.panes.iter().any(|pane| !pane.ended) {
                // `pump_any` and `recv` are both cancel-safe: each awaits
                // readiness and buffers before it decides anything, so losing
                // this race drops a wakeup and never a byte or a command. The
                // two borrow disjoint fields, which is why this is not a method.
                tokio::select! {
                    pumped = pump_any(&mut self.panes, &mut self.cursor) => Step::Pumped(pumped),
                    command = self.commands.recv() => Step::Command(command),
                }
            } else {
                Step::Command(self.commands.recv().await)
            };

            match step {
                Step::Pumped(Some((pane, Ok(Pump::Bytes(_))))) => {
                    self.report_effects(pane).await;
                    self.refresh_copy_search(pane);
                    // A bell rings in the bytes just fed. It is an attention
                    // source, not something read out of the grid.
                    self.note_bell(pane);
                    self.notify(SessionEvent::Output);
                }
                Step::Pumped(Some((pane, Ok(Pump::Eof)))) => {
                    // The child's exit is the lifecycle source: a clean exit is
                    // `Ready`, a crash is `Failed`, both with `Lifecycle`
                    // provenance. Read before the grid is touched.
                    self.note_exit(pane);
                    if let Some(pane) = self.pane_mut(pane) {
                        pane.ended = true;
                    }
                    // A pane whose child exited keeps its grid until someone
                    // closes it. The session is over only when every child is.
                    if self.panes.iter().all(|pane| pane.ended) {
                        // Not `notify`: a pending `Output` must not swallow this.
                        let _ = self.events.send(SessionEvent::Exited).await;
                    } else {
                        self.notify(SessionEvent::Output);
                    }
                }
                Step::Pumped(Some((_, Err(err)))) => return Err(err),
                // Unreachable while any pane is unended, which is the only case
                // that polls it.
                Step::Pumped(None) => {}
                Step::Command(Some(command)) => self.apply(command)?,
                // Nobody can reach this session any more.
                Step::Command(None) => break,
            }
        }

        // Every pane but the first is dropped before the wait, which kills and
        // reaps its child: waiting first would block that cleanup behind a
        // child that has not exited yet. The session's status is the surviving
        // pane's.
        let mut panes = std::mem::take(&mut self.panes);
        let first = if panes.is_empty() {
            None
        } else {
            Some(panes.remove(0))
        };
        drop(panes);
        match first {
            Some(mut pane) => pane.reactor.wait(),
            // Unreachable: closing the last pane is refused.
            None => Ok(std::os::unix::process::ExitStatusExt::from_raw(0)),
        }
    }

    /// Applies one command.
    fn apply(&mut self, command: Command) -> Result<(), PtyError> {
        match command {
            Command::Input(bytes) => self.write_focused(&bytes),
            Command::Paste(text) => {
                let bytes = paste_bytes(self.modes(), &text);
                self.write_focused(&bytes)
            }
            Command::Focus(focused) => match focus_bytes(self.modes(), focused) {
                Some(bytes) => self.write_focused(bytes),
                // The application never asked to hear about focus. Saying
                // nothing is the whole of the fallback.
                None => Ok(()),
            },
            Command::Mouse(event) => self.deliver_mouse(&event),
            Command::Resize(area) => self.resize(area),
            Command::Split {
                dir,
                ratio,
                launch,
                reply,
            } => {
                let outcome = self.split(dir, ratio, launch.map(|launch| *launch));
                let changed = outcome.is_ok();
                // A caller that gave up before the answer arrived is ordinary.
                let _ = reply.send(outcome);
                self.settle(changed)
            }
            Command::Close { pane, reply } => {
                let outcome = self.close(pane);
                let changed = outcome.is_ok();
                let _ = reply.send(outcome);
                self.settle(changed)
            }
            Command::MoveFocus(side) => {
                let changed = self.move_focus(side);
                self.settle(changed)
            }
            Command::ToggleZoom => {
                self.toggle_zoom();
                self.settle(true)
            }
            Command::NewTab(reply) => {
                let outcome = self.new_tab();
                let changed = outcome.is_ok();
                let _ = reply.send(outcome);
                self.settle(changed)
            }
            Command::CloseTab(reply) => {
                let outcome = self.close_tab();
                let changed = outcome.is_ok();
                let _ = reply.send(outcome);
                self.settle(changed)
            }
            Command::NextTab => {
                let changed = self.select_relative_tab(1);
                self.settle(changed)
            }
            Command::PrevTab => {
                let changed = self.select_relative_tab(-1);
                self.settle(changed)
            }
            Command::RenameTab { name, reply } => {
                let outcome = self
                    .model
                    .rename_tab(self.model.active(), name)
                    .map_err(TabError::Model);
                let changed = outcome.is_ok();
                let _ = reply.send(outcome);
                if changed {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::SetAttention {
                pane,
                state,
                source,
            } => {
                let changed = self.set_attention(pane, state, source);
                // Only a repaint, never a geometry pass: attention changes what
                // a pane's chrome says, not where any pane sits.
                if changed {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::AcknowledgeAttention { pane } => {
                let changed = self.acknowledge_attention(pane);
                if changed {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::EnterCopyMode => {
                if self.enter_copy_mode() {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::ExitCopyMode => {
                if self.exit_copy_mode() {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::CopyMotion(motion) => {
                if self.copy_motion(motion) {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::BeginCopySelection => {
                if self.begin_copy_selection() {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::ClearCopySelection => {
                if self.clear_copy_selection() {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::SearchCopy {
                query,
                direction,
                reply,
            } => {
                let outcome = self.search_copy(query, direction);
                if outcome.is_ok() {
                    // A zero-match search still changed the active query, which
                    // is state a copy-mode client can render.
                    self.notify(SessionEvent::Output);
                }
                let _ = reply.send(outcome);
                Ok(())
            }
            Command::NextCopyMatch(direction) => {
                if self.next_copy_match(direction) {
                    self.notify(SessionEvent::Output);
                }
                Ok(())
            }
            Command::CopySelection { target, reply } => {
                // A read, so nothing is notified: copying does not move the
                // cursor, clear the selection, or change a single grid cell.
                let _ = reply.send(self.copy_selection(target));
                Ok(())
            }
            Command::Snapshot(reply) => {
                let _ = reply.send(self.snapshot());
                Ok(())
            }
        }
    }

    /// Splits the focused pane and spawns a child in the new one.
    ///
    /// The order is what makes the two halves atomic. The layout is asked
    /// first, because it is the half that can refuse — a split too small to
    /// honor [`MIN_PANE_SIZE`](cloo_core::MIN_PANE_SIZE) must not cost a
    /// process. Only then is the child spawned, at the geometry that same
    /// layout pass produced, and a spawn that fails collapses the split back
    /// out. There is no await in between, so no other command can observe a
    /// pane that exists in one half and not the other.
    fn split(
        &mut self,
        dir: Direction,
        ratio: f32,
        launch: Option<Launch>,
    ) -> Result<PaneId, PaneError> {
        let launch = launch.unwrap_or_else(|| self.default_launch.clone());
        let target = self.focused();
        let new_pane = self.ids.peek();
        let area = self.area;
        // A successful split unzooms — the new pane is what the user is about
        // to type into, and it cannot be seen behind a zoom. A failed one must
        // put the zoom back, since a rollback restores everything or nothing.
        let zoomed = self.active_tab().layout().zoomed();
        self.active_tab_mut()
            .layout_mut()
            .split(target, dir, ratio, new_pane, area)
            .map_err(PaneError::Layout)?;

        match self.spawn_pane(new_pane, &launch) {
            Ok(pane) => {
                self.panes.push(pane);
                let _ = self.ids.allocate();
                // Focus follows the split: it is what makes splitting and then
                // typing do what a user means by it.
                let _ = self.active_tab_mut().focus(new_pane);
                Ok(new_pane)
            }
            Err(err) => {
                // Roll back. Collapsing a split whose second child is a fresh
                // leaf promotes the first one, which restores the tree exactly.
                let _ = self.active_tab_mut().layout_mut().close(new_pane);
                if let Some(pane) = zoomed {
                    let _ = self.active_tab_mut().layout_mut().zoom(pane);
                }
                Err(PaneError::Spawn(err))
            }
        }
    }

    /// Starts a child for `pane` at the geometry the layout just gave it.
    ///
    /// The geometry pass that follows the split would correct a wrong size a
    /// moment later, but a child reads its `winsize` at startup: handing it the
    /// session's whole area and then shrinking it is a spurious `SIGWINCH` and,
    /// for a program that only looks once, a lasting wrong answer.
    fn spawn_pane(&self, pane: PaneId, launch: &Launch) -> Result<Pane, PtyError> {
        let size = match self.active_tab().layout().rect_of(pane, self.area) {
            Some(rect) => TermSize::new(rect.size.cols, rect.size.rows)?,
            // Unreachable: the pane was resolved a moment ago.
            None => self.config.term_size(),
        };
        let config = launch.configure(&self.config.clone().size(size));
        Ok(Pane {
            id: pane,
            reactor: PtyReactor::spawn(&config)?,
            meta: launch.meta().clone(),
            copy_mode: None,
            ended: false,
        })
    }

    /// Closes a pane: the layout collapses and the pane's PTY is dropped.
    ///
    /// The layout is asked first here too, so a refusal — an unknown pane, or
    /// the session's last one — never kills a child. Dropping the [`Pane`] is
    /// what kills and reaps it; there is no separate teardown to forget.
    fn close(&mut self, pane: PaneId) -> Result<(), PaneError> {
        self.active_tab_mut()
            .layout_mut()
            .close(pane)
            .map_err(PaneError::Layout)?;
        self.panes.retain(|held| held.id != pane);
        if self.focused() == pane {
            // The survivor first in traversal order. Directional focus needs a
            // pane to start from, and the one that was just closed is gone.
            if let Some(next) = self.active_tab().layout().panes().first() {
                let _ = self.active_tab_mut().focus(*next);
            }
            // `Layout::close` already unzoomed if the closed pane was the
            // zoomed one; this keeps the invariant that a zoom always names the
            // focused pane when focus moved for any other reason.
            self.follow_zoom();
        }
        Ok(())
    }

    /// Moves focus one pane in a direction, reporting whether it moved.
    ///
    /// Geometric, from [`Layout::neighbor`]: "left" means the pane a user sees
    /// to the left, not whichever sibling the tree holds. Asking to move past
    /// the edge is not an error and does nothing — wrapping around would move
    /// attention somewhere nobody was looking.
    fn move_focus(&mut self, side: Side) -> bool {
        let focused = self.focused();
        let Some(next) = self
            .active_tab()
            .layout()
            .neighbor(focused, side, self.area)
        else {
            return false;
        };
        if next == focused {
            return false;
        }
        let _ = self.active_tab_mut().focus(next);
        // Zoom follows focus. The alternative — moving focus to a pane the zoom
        // is hiding — leaves a user typing into something they cannot see.
        self.follow_zoom();
        true
    }

    /// Shows the focused pane alone at the full area, or undoes that.
    ///
    /// No PTY is spawned, killed, or restarted: the geometry pass that follows
    /// resizes the zoomed pane's child to the full area and leaves every hidden
    /// pane's child exactly as it was, holding the winsize it last had. Unzoom
    /// gives all of them the ratios that were there the whole time.
    fn toggle_zoom(&mut self) {
        // Unreachable failure: focus always names a pane in the layout.
        let focused = self.focused();
        let _ = self.active_tab_mut().layout_mut().toggle_zoom(focused);
    }

    /// Retargets an active zoom at the focused pane. A no-op when nothing is
    /// zoomed, which is why moving focus in an ordinary layout costs nothing.
    fn follow_zoom(&mut self) {
        if self.active_tab().layout().zoomed().is_some() {
            let focused = self.focused();
            let _ = self.active_tab_mut().layout_mut().zoom(focused);
        }
    }

    /// Creates a fresh tab only after its initial PTY has started, so a spawn
    /// failure cannot leave the tab model pointing at a pane that does not
    /// exist. The tab owns the pane's layout; the session retains its reactor
    /// and keeps pumping it even while another tab is active.
    fn new_tab(&mut self) -> Result<TabId, PaneError> {
        let pane_id = self.ids.peek();
        let size = TermSize::new(self.area.cols, self.area.rows)
            .unwrap_or_else(|_| self.config.term_size());
        let config = self
            .default_launch
            .configure(&self.config.clone().size(size));
        let reactor = PtyReactor::spawn(&config).map_err(PaneError::Spawn)?;
        let pane = Pane {
            id: pane_id,
            reactor,
            meta: self.default_launch.meta().clone(),
            copy_mode: None,
            ended: false,
        };
        let name = TabName::from_pane_name(&self.default_launch.meta().name);
        let tab = self.model.create_tab(name, pane_id);
        self.panes.push(pane);
        let _ = self.ids.allocate();
        Ok(tab)
    }

    /// Closes the active tab and drops every PTY its layout owns. The pure
    /// model refuses its final tab before a reactor is removed, so the two
    /// ownership records remain in step on every rejection.
    fn close_tab(&mut self) -> Result<(), TabError> {
        let tab = self.model.active();
        let panes = self.active_tab().layout().panes().to_vec();
        self.model.close_tab(tab).map_err(TabError::Model)?;
        self.panes.retain(|pane| !panes.contains(&pane.id));
        Ok(())
    }

    /// Selects a neighbour in tab-bar order, wrapping at either edge.
    fn select_relative_tab(&mut self, offset: isize) -> bool {
        let tabs = self.model.tabs();
        if tabs.len() < 2 {
            return false;
        }
        let Some(current) = tabs.iter().position(|tab| tab.id() == self.model.active()) else {
            return false;
        };
        let next = match offset {
            -1 if current == 0 => tabs.len() - 1,
            -1 => current - 1,
            1 => (current + 1) % tabs.len(),
            _ => current,
        };
        let tab = tabs[next].id();
        let _ = self.model.select_tab(tab);
        true
    }

    /// Records a pane's attention state, reporting whether anything changed.
    ///
    /// The coalescing rule lives in [`Attention::set`](cloo_core::pane::Attention::set),
    /// not here: re-reporting a state a pane already holds keeps its
    /// acknowledgment, so a source that re-announces every second cannot refill
    /// a queue the user just cleared. A change is reported only when the state,
    /// its source, or its acknowledgment actually moved, so an idle re-report
    /// costs no repaint. An unknown pane is a silent no-op.
    fn set_attention(
        &mut self,
        pane: PaneId,
        state: AttentionState,
        source: AttentionSource,
    ) -> bool {
        let Some(pane) = self.pane_mut(pane) else {
            return false;
        };
        let before = pane.meta.attention.clone();
        pane.meta.attention.set(state, source);
        pane.meta.attention != before
    }

    /// Maps a terminal bell to a pane's attention.
    ///
    /// A bell is the one thing every application means the same way: this pane
    /// wants a human. It becomes [`AttentionState::NeedsInput`] with
    /// [`AttentionSource::Bell`] provenance, coalesced by
    /// [`Attention::set`](cloo_core::pane::Attention::set) like every other
    /// source — a pane that bells while already flagged is not flagged twice.
    /// This is the whole of "a bell is a source": there is no reading of what
    /// the child printed.
    fn note_bell(&mut self, pane: PaneId) {
        if self
            .pane_mut(pane)
            .is_some_and(|pane| pane.reactor.take_bell())
        {
            self.set_attention(pane, AttentionState::NeedsInput, AttentionSource::Bell);
        }
    }

    /// Maps a child's exit to a pane's attention.
    ///
    /// End of file is the lifecycle event cloo observes directly. A clean exit
    /// becomes [`AttentionState::Ready`] — finished, nobody has looked — and any
    /// other status becomes [`AttentionState::Failed`], both with
    /// [`AttentionSource::Lifecycle`] provenance. A child that closed its
    /// terminal without a reapable status is treated as finished rather than as
    /// a failure invented from a missing one. No exit code is ever guessed from
    /// the grid.
    fn note_exit(&mut self, pane: PaneId) {
        let state = match self.exit_status(pane) {
            Some(status) if status.success() => AttentionState::Ready,
            Some(_) => AttentionState::Failed,
            None => AttentionState::Ready,
        };
        self.set_attention(pane, state, AttentionSource::Lifecycle);
    }

    /// The child's exit status once its PTY has reached end of file, or `None`
    /// if it cannot be reaped without blocking.
    ///
    /// The bounded spin is [`EXIT_REAP_TRIES`]: end of file all but guarantees
    /// the child is exiting, so a status appears within a few tries, while a
    /// child that detached and kept running falls through rather than wedging
    /// the session on a blocking wait.
    fn exit_status(&mut self, pane: PaneId) -> Option<ExitStatus> {
        for _ in 0..EXIT_REAP_TRIES {
            match self.pane_mut(pane)?.reactor.try_exit_status() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::yield_now(),
                Err(_) => return None,
            }
        }
        None
    }

    /// Marks a pane's current attention as seen, reporting whether that changed
    /// anything. An already-acknowledged or unknown pane is a no-op.
    fn acknowledge_attention(&mut self, pane: PaneId) -> bool {
        let Some(pane) = self.pane_mut(pane) else {
            return false;
        };
        if pane.meta.attention.acknowledged {
            return false;
        }
        pane.meta.attention.acknowledge();
        true
    }

    /// Starts copy mode over the focused pane's complete retained history.
    ///
    /// The emulator owns both history and the viewport, while [`CopyMode`]
    /// owns only selection/search intent. Keeping them together in this actor
    /// is what lets a second client inherit the first client's position.
    fn enter_copy_mode(&mut self) -> bool {
        let pane_id = self.focused();
        let Some((lines, columns)) = self.copy_text(pane_id) else {
            return false;
        };
        let Some(pane) = self.pane_mut(pane_id) else {
            return false;
        };
        if pane.copy_mode.is_some() {
            return false;
        }
        pane.copy_mode = Some(CopyMode::new(&lines, columns));
        reveal_copy_cursor(pane);
        true
    }

    /// Leaves copy mode and resumes following live output in the focused pane.
    fn exit_copy_mode(&mut self) -> bool {
        let pane_id = self.focused();
        let Some(pane) = self.pane_mut(pane_id) else {
            return false;
        };
        if pane.copy_mode.take().is_none() {
            return false;
        }
        pane.reactor.emulator_mut().scroll_to_bottom();
        true
    }

    /// Moves the focused copy cursor and scrolls only enough to keep it visible.
    fn copy_motion(&mut self, motion: CopyMotion) -> bool {
        let pane_id = self.focused();
        let Some((lines, columns)) = self.copy_text(pane_id) else {
            return false;
        };
        let Some(pane) = self.pane_mut(pane_id) else {
            return false;
        };
        let Some(copy_mode) = pane.copy_mode.as_mut() else {
            return false;
        };
        let changed = copy_mode.move_cursor(motion, &lines, columns);
        if changed {
            reveal_copy_cursor(pane);
        }
        changed
    }

    /// Begins visual selection at the focused copy cursor.
    fn begin_copy_selection(&mut self) -> bool {
        let Some(copy_mode) = self
            .pane_mut(self.focused())
            .and_then(|pane| pane.copy_mode.as_mut())
        else {
            return false;
        };
        if copy_mode.selection().is_some() {
            return false;
        }
        copy_mode.begin_selection();
        true
    }

    /// Clears the focused copy selection while retaining its cursor and query.
    fn clear_copy_selection(&mut self) -> bool {
        let Some(copy_mode) = self
            .pane_mut(self.focused())
            .and_then(|pane| pane.copy_mode.as_mut())
        else {
            return false;
        };
        if copy_mode.selection().is_none() {
            return false;
        }
        copy_mode.clear_selection();
        true
    }

    /// Searches the focused pane's retained history and keeps the old state on
    /// a parse failure. Regex validation remains a normal answer, never a
    /// session-task failure.
    fn search_copy(
        &mut self,
        query: String,
        direction: SearchDirection,
    ) -> Result<bool, CopyModeError> {
        let pane_id = self.focused();
        let Some((lines, columns)) = self.copy_text(pane_id) else {
            return Ok(false);
        };
        let Some(pane) = self.pane_mut(pane_id) else {
            return Ok(false);
        };
        if pane.copy_mode.is_none() {
            pane.copy_mode = Some(CopyMode::new(&lines, columns));
        }
        let found = match pane.copy_mode.as_mut() {
            Some(copy_mode) => copy_mode
                .search(query, direction, &lines, columns)
                .map_err(CopyModeError::Search)?,
            // `Some` was installed just above; retain a non-panicking fallback
            // if that invariant ever changes.
            None => return Ok(false),
        };
        reveal_copy_cursor(pane);
        Ok(found)
    }

    /// Visits the next retained match of the focused pane's active query.
    fn next_copy_match(&mut self, direction: SearchDirection) -> bool {
        let Some(pane) = self.pane_mut(self.focused()) else {
            return false;
        };
        let Some(copy_mode) = pane.copy_mode.as_mut() else {
            return false;
        };
        let changed = copy_mode.search_next(direction);
        if changed {
            reveal_copy_cursor(pane);
        }
        changed
    }

    /// Extracts the focused pane's selection as a typed clipboard effect.
    ///
    /// The selection is read out of retained scrollback, which is why this
    /// answer can only come from here: a client caches the visible grid alone
    /// and a selection routinely reaches above it. Nothing is mutated, and an
    /// empty selection produces no effect rather than an empty clipboard store.
    fn copy_selection(&self, target: ClipboardTarget) -> Option<(PaneId, OuterTerminalEffect)> {
        let pane = self.focused();
        let (lines, columns) = self.copy_text(pane)?;
        let text = self
            .panes
            .iter()
            .find(|held| held.id == pane)?
            .copy_mode
            .as_ref()?
            .selected_text(&lines, columns)?;
        if text.is_empty() {
            return None;
        }
        Some((pane, OuterTerminalEffect::ClipboardStore { target, text }))
    }

    /// The complete retained text and terminal width for one pane, captured
    /// before copy-mode mutation borrows it mutably.
    fn copy_text(&self, pane: PaneId) -> Option<(Vec<String>, u16)> {
        self.panes.iter().find(|held| held.id == pane).map(|held| {
            let emulator = held.reactor.emulator();
            (emulator.scrollback_text(), emulator.size().cols())
        })
    }

    /// Re-runs an active copy search after new output enters retained history.
    fn refresh_copy_search(&mut self, pane: PaneId) {
        // Most PTY output arrives while no copy search is active. Looking up
        // the mode first keeps that hot path from cloning every retained grid
        // line merely to discover there is no regex to refresh.
        let has_active_search = self
            .panes
            .iter()
            .find(|held| held.id == pane)
            .and_then(|held| held.copy_mode.as_ref())
            .and_then(CopyMode::search_state)
            .is_some();
        if !has_active_search {
            return;
        }
        let Some((lines, columns)) = self.copy_text(pane) else {
            return;
        };
        let Some(copy_mode) = self.pane_mut(pane).and_then(|held| held.copy_mode.as_mut()) else {
            return;
        };
        copy_mode.refresh_search(&lines, columns);
    }

    /// Runs the geometry pass and repaints after a split or close changed the
    /// tree. A command that changed nothing costs nothing.
    fn settle(&mut self, changed: bool) -> Result<(), PtyError> {
        if !changed {
            return Ok(());
        }
        self.apply_geometry()?;
        self.notify(SessionEvent::Output);
        Ok(())
    }

    /// Resizes the session: one layout pass, then one `TIOCSWINSZ` per pane.
    ///
    /// A degenerate area is ignored rather than refused. A client that briefly
    /// reports zero rows — which happens under some terminals mid-drag — has no
    /// bearing on a child that is running fine, and refusing would turn a
    /// cosmetic glitch into a dead session.
    fn resize(&mut self, area: Size) -> Result<(), PtyError> {
        if area == self.area || !usable(area) {
            return Ok(());
        }
        self.area = area;
        self.apply_geometry()?;

        // A resize repaints even if the child never writes another byte.
        self.notify(SessionEvent::Output);
        Ok(())
    }

    /// Gives every pane the geometry of one layout pass.
    ///
    /// The single layout pass. Every pane's geometry comes from here and from
    /// nowhere else, so the rect a client is told about and the winsize its
    /// child is given cannot disagree.
    fn apply_geometry(&mut self) -> Result<(), PtyError> {
        let rects = self.active_tab().layout().resolve(self.area);
        for rect in rects {
            // A pane squeezed to nothing by a shrunken area keeps its last
            // usable geometry; the ratios are still there when it grows back.
            let Ok(size) = TermSize::new(rect.size.cols, rect.size.rows) else {
                continue;
            };
            if let Some(pane) = self.pane_mut(rect.pane) {
                // `PtyReactor::resize` is the ordering: grid first, so output
                // arriving right after the child's `SIGWINCH` lands on a grid
                // that is already the right shape.
                pane.reactor.resize(size)?;
            }
        }
        Ok(())
    }

    /// The current picture.
    ///
    /// Metadata is projected from the same pass that resolves geometry, so a
    /// client can never be told about a pane it has no identity for, or given an
    /// identity for a pane that is not on screen.
    fn snapshot(&self) -> SessionSnapshot {
        let tab = self.model.active();
        let active = self.active_tab();
        let panes = active.layout().resolve(self.area);
        let metas = panes
            .iter()
            .filter_map(|rect| {
                self.panes
                    .iter()
                    .find(|pane| pane.id == rect.pane)
                    .map(|pane| pane.meta.to_wire(pane.id))
            })
            .collect();
        let attention = panes
            .iter()
            .filter_map(|rect| {
                self.panes
                    .iter()
                    .find(|pane| pane.id == rect.pane)
                    .map(|pane| pane.meta.attention.to_wire(pane.id))
            })
            .collect();
        SessionSnapshot {
            tab,
            tabs: self.tab_summaries(),
            area: self.area,
            panes,
            metas,
            attention,
            copy_mode: self.copy_mode_state(),
            focused: active.focused(),
            zoomed: active.layout().zoomed(),
            pane: self
                .focused_pane()
                .map_or_else(PaneSnapshot::default, |pane| pane.reactor.snapshot()),
            modes: self.modes(),
        }
    }

    /// What the focused pane's application has negotiated.
    fn modes(&self) -> PaneModes {
        self.focused_pane().map_or_else(PaneModes::default, |pane| {
            cloo_core::grid::wire_modes(pane.reactor.emulator().modes())
        })
    }

    /// Delivers a mouse event to the pane it names, or to nobody.
    ///
    /// The client hit-tested the event into a pane before sending it, and only
    /// events it decided belong to an *application* are sent at all. The server
    /// is the second half of that contract and it checks the same two things
    /// again, because a client is not something to be trusted with a write into
    /// an arbitrary child:
    ///
    /// - The named pane must be one the user can actually see — in the active
    ///   tab, and not hidden behind a zoom. A stale event naming a pane that has
    ///   closed, moved to another tab, or been zoomed away is dropped.
    /// - The bytes are encoded from *that pane's* modes, never the focused
    ///   pane's, so an application that never asked for the mouse cannot be
    ///   handed a report because its neighbour did.
    ///
    /// Delivering to the named pane rather than to whatever is focused is what
    /// makes "an application's mouse events are not stolen" true when the
    /// pointer is over an unfocused pane, and it is also the stricter rule:
    /// nothing is ever written to a pane the event did not name.
    fn deliver_mouse(&self, event: &MouseEvent) -> Result<(), PtyError> {
        if !self.is_visible(event.pane) {
            return Ok(());
        }
        let Some(pane) = self.panes.iter().find(|pane| pane.id == event.pane) else {
            return Ok(());
        };
        let modes = cloo_core::grid::wire_modes(pane.reactor.emulator().modes());
        match mouse_bytes(modes, event) {
            Some(bytes) => pane.reactor.write_all(&bytes),
            // The application is not tracking the mouse, or not at the level
            // this event needs. Writing nothing is the whole of the fallback.
            None => Ok(()),
        }
    }

    /// Whether a pane is one the user is currently looking at.
    ///
    /// A zoomed tab shows exactly one pane, so every other pane in it is as
    /// unreachable by the pointer as a pane in another tab.
    fn is_visible(&self, pane: PaneId) -> bool {
        let layout = self.active_tab().layout();
        match layout.zoomed() {
            Some(zoomed) => zoomed == pane,
            None => layout.contains(pane),
        }
    }

    /// Writes to the focused pane's child.
    fn write_focused(&self, bytes: &[u8]) -> Result<(), PtyError> {
        match self.focused_pane() {
            Some(pane) => pane.reactor.write_all(bytes),
            // Unreachable: a session always holds at least one pane, and focus
            // always names one of them. Dropping the bytes beats panicking in
            // an input path.
            None => Ok(()),
        }
    }

    /// The focused pane, which the invariant says always exists.
    fn focused_pane(&self) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == self.focused())
    }

    /// The tab the user is looking at. The pure model guarantees it exists.
    fn active_tab(&self) -> &cloo_core::tab::Tab {
        self.model.active_tab()
    }

    /// The active tab, mutably. See [`Self::active_tab`].
    fn active_tab_mut(&mut self) -> &mut cloo_core::tab::Tab {
        self.model.active_tab_mut()
    }

    /// The current tab's focus, which is always a pane in that tab's layout.
    fn focused(&self) -> PaneId {
        self.active_tab().focused()
    }

    /// Projects the pure tab model onto the wire's compact tab-bar data.
    fn tab_summaries(&self) -> Vec<TabSummary> {
        let active = self.model.active();
        self.model
            .tabs()
            .iter()
            .map(|tab| TabSummary {
                tab: tab.id(),
                title: tab.name().as_str().to_owned(),
                active: tab.id() == active,
            })
            .collect()
    }

    /// Projects the focused pane's copy state for clients without giving them
    /// any mutable handle to scrollback or the selection itself.
    fn copy_mode_state(&self) -> Option<CopyModeState> {
        let pane = self.focused();
        let held = self.panes.iter().find(|held| held.id == pane)?;
        let copy_mode = held.copy_mode.as_ref()?;
        // The client caches only the visible grid, so it needs the retained
        // line its first row is showing to place any of the positions below.
        // Both halves come from this one borrow, which is what stops a
        // highlight from being placed against a viewport it never described.
        let emulator = held.reactor.emulator();
        let viewport_top = emulator
            .scrollback_len()
            .saturating_sub(emulator.scroll_offset());
        let selection = copy_mode.selection().map(|selection| WireCopySelection {
            anchor: wire_scroll_point(selection.anchor),
            head: wire_scroll_point(selection.head),
        });
        let (query, matches) = copy_mode.search_state().map_or_else(
            || (None, Vec::new()),
            |search| {
                (
                    Some(search.query().to_owned()),
                    search
                        .matches()
                        .iter()
                        .map(|matched| WireSearchMatch {
                            start: wire_scroll_point(matched.start),
                            end: wire_scroll_point(matched.end),
                        })
                        .collect(),
                )
            },
        );
        Some(CopyModeState {
            pane,
            viewport_top: u32::try_from(viewport_top).unwrap_or(u32::MAX),
            cursor: wire_scroll_point(copy_mode.cursor()),
            selection,
            query,
            matches,
        })
    }

    /// One pane by id, mutably.
    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    /// Reports an event, dropping it if one is already pending.
    ///
    /// Coalescing is the point: a large `cat` must not turn into one wakeup per
    /// read, and a second `Output` tells a reader nothing the first did not.
    fn notify(&self, event: SessionEvent) {
        let _ = self.events.try_send(event);
    }

    /// Drains one pane's typed outer-terminal requests after feeding output.
    ///
    /// Effects are sent before the coalesced output level so a title-only OSC
    /// is observable even when it made no grid cell dirty. Unlike an output
    /// wakeup, each request carries information and is therefore awaited by
    /// the session-to-daemon channel; that channel has no socket write in its
    /// path, so a slow terminal cannot stall this actor.
    async fn report_effects(&mut self, pane: PaneId) {
        let effects = self
            .pane_mut(pane)
            .map_or_else(Vec::new, |pane| pane.reactor.emulator_mut().drain_effects());
        for effect in effects {
            if self
                .events
                .send(SessionEvent::Effect {
                    pane,
                    effect: wire_effect(effect),
                })
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// Converts a core retained-scrollback point to its fixed-width wire form.
fn wire_scroll_point(point: cloo_core::CopyPoint) -> ScrollPoint {
    ScrollPoint::new(u32::try_from(point.line).unwrap_or(u32::MAX), point.column)
}

/// Scrolls a pane's server-owned viewport only enough to include its copy
/// cursor. The calculation is in retained-line coordinates, while the emulator
/// exposes a display offset measured back from the live bottom.
fn reveal_copy_cursor(pane: &mut Pane) {
    let Some(cursor) = pane.copy_mode.as_ref().map(CopyMode::cursor) else {
        return;
    };
    let emulator = pane.reactor.emulator_mut();
    let history = emulator.scrollback_len();
    let rows = usize::from(emulator.size().rows());
    if rows == 0 {
        return;
    }
    let offset = emulator.scroll_offset();
    let top = history.saturating_sub(offset);
    let bottom = top.saturating_add(rows.saturating_sub(1));
    let desired_top = if cursor.line < top {
        cursor.line
    } else if cursor.line > bottom {
        cursor.line.saturating_add(1).saturating_sub(rows)
    } else {
        return;
    };
    let desired_offset = history.saturating_sub(desired_top);
    let delta = i64::try_from(desired_offset)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(offset).unwrap_or(i64::MAX));
    let delta = match i32::try_from(delta) {
        Ok(delta) => delta,
        Err(_) if delta.is_negative() => i32::MIN,
        Err(_) => i32::MAX,
    };
    emulator.scroll(delta);
}

/// Converts the emulation crate's leaf-owned effect vocabulary to the wire.
///
/// `cloo-term` and `cloo-proto` deliberately cannot depend on one another, so
/// this is the explicit conversion boundary `cloo-core::grid` owns for cells
/// and pane modes.
fn wire_effect(effect: cloo_term::OuterTerminalEffect) -> OuterTerminalEffect {
    match effect {
        cloo_term::OuterTerminalEffect::SetTitle(title) => OuterTerminalEffect::SetTitle(title),
        cloo_term::OuterTerminalEffect::ResetTitle => OuterTerminalEffect::ResetTitle,
        cloo_term::OuterTerminalEffect::ClipboardStore { target, text } => {
            OuterTerminalEffect::ClipboardStore {
                target: match target {
                    cloo_term::ClipboardTarget::Clipboard => ClipboardTarget::Clipboard,
                    cloo_term::ClipboardTarget::PrimarySelection => {
                        ClipboardTarget::PrimarySelection
                    }
                },
                text,
            }
        }
        cloo_term::OuterTerminalEffect::Hyperlink { uri } => OuterTerminalEffect::Hyperlink { uri },
        cloo_term::OuterTerminalEffect::Notification { title, body } => {
            OuterTerminalEffect::Notification { title, body }
        }
        cloo_term::OuterTerminalEffect::Progress(progress) => {
            OuterTerminalEffect::Progress(match progress {
                cloo_term::ProgressState::Clear => ProgressState::Clear,
                cloo_term::ProgressState::Indeterminate => ProgressState::Indeterminate,
                cloo_term::ProgressState::Value(value) => ProgressState::Value(value),
                cloo_term::ProgressState::Error => ProgressState::Error,
            })
        }
        cloo_term::OuterTerminalEffect::Graphics(graphics) => {
            OuterTerminalEffect::Graphics(match graphics {
                cloo_term::GraphicsEffect::Unavailable => GraphicsEffect::Unavailable,
            })
        }
    }
}

/// What one turn of the session loop did.
enum Step {
    /// A pane's PTY produced output, reached end of file, or failed. `None`
    /// means there was nothing left to pump.
    Pumped(Option<(PaneId, Result<Pump, PtyError>)>),
    /// A command arrived, or the last handle was dropped.
    Command(Option<Command>),
}

/// Waits until any pane's PTY has something to say, and reports which.
///
/// A hand-rolled `select_all`: the set of panes is decided at runtime, so the
/// macro cannot describe it, and the alternative is a dependency for fifteen
/// lines. Every [`PtyReactor::pump`] is cancel-safe, so dropping the futures
/// that did not win — which is what happens on every call, and again whenever
/// the caller's own `select!` loses — costs a wakeup and never a byte.
///
/// `cursor` rotates the polling order, so a pane producing output continuously
/// cannot starve a quieter one behind it.
async fn pump_any(
    panes: &mut [Pane],
    cursor: &mut usize,
) -> Option<(PaneId, Result<Pump, PtyError>)> {
    type Pumping<'a> = (
        PaneId,
        Pin<Box<dyn Future<Output = Result<Pump, PtyError>> + Send + 'a>>,
    );

    let mut pending: Vec<Pumping<'_>> = panes
        .iter_mut()
        .filter(|pane| !pane.ended)
        .map(|pane| {
            let id = pane.id;
            let future: Pin<Box<dyn Future<Output = _> + Send>> = Box::pin(pane.reactor.pump());
            (id, future)
        })
        .collect();
    if pending.is_empty() {
        return None;
    }

    let start = *cursor % pending.len();
    *cursor = start.wrapping_add(1);

    std::future::poll_fn(|context| {
        for offset in 0..pending.len() {
            let index = (start + offset) % pending.len();
            let (id, future) = &mut pending[index];
            if let Poll::Ready(pumped) = future.as_mut().poll(context) {
                return Poll::Ready(Some((*id, pumped)));
            }
        }
        Poll::Pending
    })
    .await
}

/// Whether an area is something a session can actually be laid out in.
#[must_use]
pub fn usable(area: Size) -> bool {
    area.cols > 0 && area.rows > 0
}

// ---------------------------------------------------------------------------
// Encoding input for a pane's application
// ---------------------------------------------------------------------------
//
// Every function below is a pure function of the pane's negotiated
// [`PaneModes`] and the event, which is what makes the whole of input routing
// testable without a PTY. The rule they share: **encode what the application
// asked for, or send nothing.** A mode the application never enabled is never
// synthesised, because a paste bracket or a mouse report arriving at a program
// that is not expecting one lands in its input as literal garbage.

/// The sequence that opens a bracketed paste.
pub const PASTE_START: &[u8] = b"\x1b[200~";
/// The sequence that closes a bracketed paste.
pub const PASTE_END: &[u8] = b"\x1b[201~";
/// Reported to an application that enabled focus reporting when focus is gained.
pub const FOCUS_IN: &[u8] = b"\x1b[I";
/// Reported to an application that enabled focus reporting when focus is lost.
pub const FOCUS_OUT: &[u8] = b"\x1b[O";

/// Encodes pasted text for the focused pane.
///
/// Two things happen regardless of the mode. Line endings are normalised to
/// carriage returns, because that is what the Enter key sends and a pasted `\n`
/// otherwise reaches a shell as a literal newline it will not run. And any paste
/// delimiter *inside* the pasted text is stripped: without that, pasted content
/// could close the bracket early and have the rest of itself interpreted as
/// typed input, which is the injection bracketed paste exists to prevent.
#[must_use]
pub fn paste_bytes(modes: PaneModes, text: &[u8]) -> Vec<u8> {
    let body = normalize_newlines(&strip_paste_markers(text));
    if !modes.bracketed_paste {
        // The documented fallback: pasted text arrives as ordinary typed input.
        return body;
    }
    let mut out = Vec::with_capacity(body.len() + PASTE_START.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START);
    out.extend_from_slice(&body);
    out.extend_from_slice(PASTE_END);
    out
}

/// The focus report for an application that asked for one, or `None`.
#[must_use]
pub fn focus_bytes(modes: PaneModes, focused: bool) -> Option<&'static [u8]> {
    if !modes.focus_events {
        return None;
    }
    Some(if focused { FOCUS_IN } else { FOCUS_OUT })
}

/// Encodes a mouse event for the focused pane, or `None` if the application
/// would not want it.
///
/// `None` covers three distinct cases that all mean "write nothing": the
/// application is not tracking the mouse at all, it is tracking at a level below
/// what this event needs (a bare pointer move under click-only tracking), or the
/// cell is beyond what the legacy encoding can address. The third is why the SGR
/// encoding exists, and it is the reason a client is told to prefer it.
#[must_use]
pub fn mouse_bytes(modes: PaneModes, event: &MouseEvent) -> Option<Vec<u8>> {
    if modes.mouse < required_tracking(event.kind) {
        return None;
    }

    let released = matches!(event.kind, MouseKind::Release(_));
    let mut code = button_code(event.kind);
    if matches!(event.kind, MouseKind::Motion(_)) {
        code += 32;
    }
    if event.mods.shift {
        code += 4;
    }
    if event.mods.alt {
        code += 8;
    }
    if event.mods.ctrl {
        code += 16;
    }

    // Both encodings are one-based; the wire carries zero-based cells.
    let col = u32::from(event.at.col) + 1;
    let row = u32::from(event.at.row) + 1;

    if modes.sgr_mouse {
        let final_byte = if released { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{col};{row}{final_byte}").into_bytes());
    }

    // Legacy X10: a release is button 3 rather than a distinct final byte, and
    // every field is a single byte biased by 32.
    let legacy = if released { 3 + (code & !3) } else { code };
    let byte = |value: u32| u8::try_from(value + 32).ok();
    Some(vec![
        0x1b,
        b'[',
        b'M',
        byte(legacy)?,
        byte(col)?,
        byte(row)?,
    ])
}

/// The lowest tracking level at which an application wants to hear about `kind`.
fn required_tracking(kind: MouseKind) -> MouseTracking {
    match kind {
        MouseKind::Press(_)
        | MouseKind::Release(_)
        | MouseKind::ScrollUp
        | MouseKind::ScrollDown => MouseTracking::Click,
        // Dragging is reported from 1002 up; a move with no button held needs
        // 1003, which is the mode that produces a report per pointer move.
        MouseKind::Motion(Some(_)) => MouseTracking::Drag,
        MouseKind::Motion(None) => MouseTracking::Motion,
    }
}

/// The base button number an event encodes as, before modifiers.
fn button_code(kind: MouseKind) -> u32 {
    match kind {
        MouseKind::Press(button) | MouseKind::Release(button) => button_number(button),
        MouseKind::Motion(Some(button)) => button_number(button),
        // A move with nothing held reports the "no button" code.
        MouseKind::Motion(None) => 3,
        MouseKind::ScrollUp => 64,
        MouseKind::ScrollDown => 65,
    }
}

/// The button numbers both encodings share.
fn button_number(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Removes any paste delimiter found *inside* pasted text.
fn strip_paste_markers(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for marker in [PASTE_START, PASTE_END] {
            if rest.starts_with(marker) {
                rest = &rest[marker.len()..];
                continue 'outer;
            }
        }
        out.push(rest[0]);
        rest = &rest[1..];
    }
    out
}

/// Rewrites `\r\n` and a bare `\n` as the carriage return Enter actually sends.
fn normalize_newlines(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        match text[index] {
            b'\r' if text.get(index + 1) == Some(&b'\n') => {
                out.push(b'\r');
                index += 2;
            }
            b'\n' => {
                out.push(b'\r');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloo_core::Layout;

    // Unit tests never spawn a PTY — see docs/TESTING.md. Resize against a real
    // child is `crates/cloo/tests/attach.rs`.

    #[test]
    fn a_degenerate_area_is_not_usable() {
        assert!(usable(Size::new(80, 24)));
        assert!(!usable(Size::new(0, 24)));
        assert!(!usable(Size::new(80, 0)));
    }

    #[test]
    fn one_layout_pass_gives_a_single_pane_the_whole_area() {
        let layout = Layout::new(PaneId::new(1));
        let rects = layout.resolve(Size::new(100, 40));
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].size, Size::new(100, 40));
        assert_eq!((rects[0].x, rects[0].y), (0, 0));
    }

    #[test]
    fn a_gone_session_reads_as_something_other_than_a_user_error() {
        assert!(SessionGone.to_string().contains("no longer running"));
    }

    // -- encoding input for a pane's application ----------------------------

    use cloo_proto::{MouseMods, Point};

    fn modes() -> PaneModes {
        PaneModes::default()
    }

    fn event(kind: MouseKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            pane: PaneId::new(1),
            at: Point::new(col, row),
            kind,
            mods: MouseMods::NONE,
        }
    }

    #[test]
    fn a_paste_is_bracketed_only_for_an_application_that_asked() {
        let bracketing = PaneModes {
            bracketed_paste: true,
            ..modes()
        };
        assert_eq!(paste_bytes(bracketing, b"ls"), b"\x1b[200~ls\x1b[201~");
        assert_eq!(
            paste_bytes(modes(), b"ls"),
            b"ls",
            "the fallback is ordinary typed input, never a bracket the child \
             would print literally"
        );
    }

    #[test]
    fn a_paste_cannot_close_its_own_bracket() {
        let bracketing = PaneModes {
            bracketed_paste: true,
            ..modes()
        };
        let hostile = b"safe\x1b[201~rm -rf /\x1b[200~";
        let encoded = paste_bytes(bracketing, hostile);
        assert_eq!(encoded, b"\x1b[200~saferm -rf /\x1b[201~");
        assert_eq!(
            encoded
                .windows(PASTE_END.len())
                .filter(|w| *w == PASTE_END)
                .count(),
            1,
            "exactly one terminator, at the end, or the rest of the paste is \
             interpreted as typed input"
        );
    }

    #[test]
    fn pasted_line_endings_become_the_carriage_return_enter_sends() {
        assert_eq!(
            paste_bytes(modes(), b"one\r\ntwo\nthree"),
            b"one\rtwo\rthree"
        );
    }

    #[test]
    fn focus_is_reported_only_to_an_application_that_asked() {
        let watching = PaneModes {
            focus_events: true,
            ..modes()
        };
        assert_eq!(focus_bytes(watching, true), Some(FOCUS_IN));
        assert_eq!(focus_bytes(watching, false), Some(FOCUS_OUT));
        assert_eq!(
            focus_bytes(modes(), true),
            None,
            "an application that never enabled focus reporting is treated as \
             always focused, and hears nothing"
        );
    }

    #[test]
    fn an_untracked_mouse_produces_no_bytes_at_all() {
        assert_eq!(
            mouse_bytes(modes(), &event(MouseKind::Press(MouseButton::Left), 0, 0)),
            None,
            "an application not tracking the mouse must never see a report"
        );
    }

    /// One fixture per event kind: the tracking level it needs, and the SGR
    /// report it produces there. A level below is silence.
    #[test]
    fn every_mouse_event_is_encoded_at_the_level_that_asked_for_it() {
        let cases: [(MouseKind, MouseTracking, &str); 6] = [
            (
                MouseKind::Press(MouseButton::Left),
                MouseTracking::Click,
                "\x1b[<0;11;6M",
            ),
            (
                MouseKind::Release(MouseButton::Middle),
                MouseTracking::Click,
                "\x1b[<1;11;6m",
            ),
            (MouseKind::ScrollUp, MouseTracking::Click, "\x1b[<64;11;6M"),
            (
                MouseKind::ScrollDown,
                MouseTracking::Click,
                "\x1b[<65;11;6M",
            ),
            (
                MouseKind::Motion(Some(MouseButton::Right)),
                MouseTracking::Drag,
                "\x1b[<34;11;6M",
            ),
            (
                MouseKind::Motion(None),
                MouseTracking::Motion,
                "\x1b[<35;11;6M",
            ),
        ];

        for (kind, needs, expected) in cases {
            let sgr = PaneModes {
                mouse: needs,
                sgr_mouse: true,
                ..modes()
            };
            assert_eq!(
                mouse_bytes(sgr, &event(kind, 10, 5)).as_deref(),
                Some(expected.as_bytes()),
                "{kind:?} at {needs:?}"
            );

            if needs > MouseTracking::Click {
                let below = PaneModes {
                    mouse: MouseTracking::Click,
                    sgr_mouse: true,
                    ..modes()
                };
                assert_eq!(
                    mouse_bytes(below, &event(kind, 10, 5)),
                    None,
                    "{kind:?} must be silent below {needs:?}"
                );
            }
        }
    }

    #[test]
    fn mouse_modifiers_ride_in_the_button_code() {
        let sgr = PaneModes {
            mouse: MouseTracking::Click,
            sgr_mouse: true,
            ..modes()
        };
        let mut click = event(MouseKind::Press(MouseButton::Left), 0, 0);
        click.mods = MouseMods {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(
            mouse_bytes(sgr, &click).as_deref(),
            Some("\x1b[<28;1;1M".as_bytes()),
            "4 + 8 + 16 on top of button 0"
        );
    }

    #[test]
    fn a_legacy_application_gets_the_x10_encoding_and_its_limits() {
        let legacy = PaneModes {
            mouse: MouseTracking::Click,
            sgr_mouse: false,
            ..modes()
        };
        assert_eq!(
            mouse_bytes(legacy, &event(MouseKind::Press(MouseButton::Left), 0, 0)).as_deref(),
            Some(&[0x1b, b'[', b'M', 32, 33, 33][..]),
            "every X10 field is biased by 32"
        );
        assert_eq!(
            mouse_bytes(legacy, &event(MouseKind::Release(MouseButton::Right), 0, 0)).as_deref(),
            Some(&[0x1b, b'[', b'M', 35, 33, 33][..]),
            "X10 has no distinct release: it reports button 3"
        );
        assert_eq!(
            mouse_bytes(legacy, &event(MouseKind::Press(MouseButton::Left), 300, 0)),
            None,
            "a cell the legacy encoding cannot address is dropped, never sent wrong"
        );
    }

    #[test]
    fn the_sgr_encoding_addresses_a_cell_the_legacy_one_cannot() {
        let sgr = PaneModes {
            mouse: MouseTracking::Click,
            sgr_mouse: true,
            ..modes()
        };
        assert_eq!(
            mouse_bytes(sgr, &event(MouseKind::Press(MouseButton::Left), 300, 0)).as_deref(),
            Some("\x1b[<0;301;1M".as_bytes())
        );
    }

    #[tokio::test]
    async fn a_handle_whose_task_is_gone_reports_it_rather_than_hanging() {
        let (commands, rx) = mpsc::channel(1);
        let handle = SessionHandle { commands };
        drop(rx);
        assert_eq!(handle.input(vec![b'x']).await, Err(SessionGone));
        assert_eq!(handle.paste(vec![b'x']).await, Err(SessionGone));
        assert_eq!(handle.focus(true).await, Err(SessionGone));
        assert_eq!(
            handle
                .mouse(event(MouseKind::Press(MouseButton::Left), 0, 0))
                .await,
            Err(SessionGone)
        );
        assert_eq!(handle.resize(Size::new(80, 24)).await, Err(SessionGone));
        assert_eq!(handle.snapshot().await.err(), Some(SessionGone));
    }
}
