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
//! The task pumps its PTY for its whole life, attached or not. A session that
//! only read while someone was watching would lose everything written in
//! between, and a reattaching client would find a stale grid.

use std::fmt;
use std::process::ExitStatus;

use cloo_core::layout::Layout;
use cloo_proto::{PaneId, PaneRect, Size};
use cloo_term::TermSize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::pty::{PaneSnapshot, PtyConfig, PtyError, PtyReactor, Pump};

/// How many commands may be in flight before a sender waits.
///
/// Deep enough that a burst of keystrokes never blocks the caller, shallow
/// enough that a wedged session applies backpressure instead of growing without
/// bound.
const COMMAND_QUEUE: usize = 64;

/// Everything that mutates a session.
///
/// Deliberately the whole vocabulary: if it is not here, it does not change
/// session state. Splits, closes, and focus join it in M2.
#[derive(Debug)]
pub enum Command {
    /// Keyboard bytes for the focused pane's child.
    Input(Vec<u8>),
    /// The session area changed. Triggers one layout pass and one `TIOCSWINSZ`
    /// per pane.
    Resize(Size),
    /// Asks for the current picture. The reply channel is how a reader gets
    /// state out without holding a reference to it.
    Snapshot(oneshot::Sender<SessionSnapshot>),
}

/// What a session tells whoever is listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// Something changed and the current snapshot differs from the last one
    /// drawn. Coalesced: at most one is pending at a time.
    Output,
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

/// The whole picture of a session at one instant.
///
/// Geometry and contents come from the same pass over the same state, which is
/// what lets a client apply them together without ever holding rows it has
/// nowhere to put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// The session's area, in cells.
    pub area: Size,
    /// Every pane and where it sits, from one [`Layout::resolve`].
    pub panes: Vec<PaneRect>,
    /// The focused pane, which is the one [`pane`](Self::pane) describes.
    pub focused: PaneId,
    /// The focused pane's contents.
    pub pane: PaneSnapshot,
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

    /// Tells the session its area changed.
    ///
    /// # Errors
    ///
    /// Returns [`SessionGone`] if the session task has ended.
    pub async fn resize(&self, area: Size) -> Result<(), SessionGone> {
        self.send(Command::Resize(area)).await
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

/// One session's authoritative state, owned by one task.
///
/// Not `Debug`: it owns an [`Emulator`](cloo_term::Emulator), whose grid is not,
/// and printing a session's whole scrollback would not be useful anyway.
pub struct Session {
    reactor: PtyReactor,
    layout: Layout,
    focused: PaneId,
    area: Size,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<SessionEvent>,
}

impl Session {
    /// Spawns a child on a fresh PTY and puts a session task in front of it.
    ///
    /// Must be called from inside a Tokio runtime context.
    ///
    /// # Errors
    ///
    /// Propagates any [`PtyReactor::spawn`] failure.
    pub fn spawn(config: &PtyConfig, pane: PaneId) -> Result<SpawnedSession, PtyError> {
        let reactor = PtyReactor::spawn(config)?;
        let child_id = reactor.child_id();
        let area = cloo_core::grid::wire_size(config.term_size());

        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        // Capacity one on purpose: `Output` is a level, so a second one adds
        // nothing a reader would act on differently.
        let (events, event_rx) = mpsc::channel(1);

        let session = Self {
            reactor,
            layout: Layout::new(pane),
            focused: pane,
            area,
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

    /// Runs until every handle is dropped, then reaps the child.
    async fn run(mut self) -> Result<ExitStatus, PtyError> {
        // Once the PTY is at end of file there is nothing left to pump, but the
        // task keeps answering commands so the child's last output can still be
        // asked for and drawn.
        let mut pumping = true;

        loop {
            let step = if pumping {
                // `pump` and `recv` are both cancel-safe: each awaits readiness
                // and buffers before it decides anything, so losing this race
                // drops a wakeup and never a byte or a command.
                tokio::select! {
                    pumped = self.reactor.pump() => Step::Pumped(pumped?),
                    command = self.commands.recv() => Step::Command(command),
                }
            } else {
                Step::Command(self.commands.recv().await)
            };

            match step {
                Step::Pumped(Pump::Bytes(_)) => self.notify(SessionEvent::Output),
                Step::Pumped(Pump::Eof) => {
                    pumping = false;
                    // Not `notify`: a pending `Output` must not swallow this.
                    let _ = self.events.send(SessionEvent::Exited).await;
                }
                Step::Command(Some(command)) => self.apply(command)?,
                // Nobody can reach this session any more.
                Step::Command(None) => break,
            }
        }

        self.reactor.wait()
    }

    /// Applies one command.
    fn apply(&mut self, command: Command) -> Result<(), PtyError> {
        match command {
            Command::Input(bytes) => self.reactor.write_all(&bytes),
            Command::Resize(area) => self.resize(area),
            Command::Snapshot(reply) => {
                // A caller that gave up before the answer arrived is ordinary.
                let _ = reply.send(self.snapshot());
                Ok(())
            }
        }
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

        // The single layout pass. Every pane's geometry comes from here and
        // from nowhere else, so the rect a client is told about and the winsize
        // its child is given cannot disagree.
        for rect in self.layout.resolve(self.area) {
            // A pane squeezed to nothing by a shrunken area keeps its last
            // usable geometry; the ratios are still there when it grows back.
            let Ok(size) = TermSize::new(rect.size.cols, rect.size.rows) else {
                continue;
            };
            if rect.pane == self.focused {
                // `PtyReactor::resize` is the ordering: grid first, so output
                // arriving right after the child's `SIGWINCH` lands on a grid
                // that is already the right shape.
                self.reactor.resize(size)?;
            }
        }

        // A resize repaints even if the child never writes another byte.
        self.notify(SessionEvent::Output);
        Ok(())
    }

    /// The current picture.
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            area: self.area,
            panes: self.layout.resolve(self.area),
            focused: self.focused,
            pane: self.reactor.snapshot(),
        }
    }

    /// Reports an event, dropping it if one is already pending.
    ///
    /// Coalescing is the point: a large `cat` must not turn into one wakeup per
    /// read, and a second `Output` tells a reader nothing the first did not.
    fn notify(&self, event: SessionEvent) {
        let _ = self.events.try_send(event);
    }
}

/// What one turn of the session loop did.
enum Step {
    /// The PTY produced output, or reached end of file.
    Pumped(Pump),
    /// A command arrived, or the last handle was dropped.
    Command(Option<Command>),
}

/// Whether an area is something a session can actually be laid out in.
#[must_use]
pub fn usable(area: Size) -> bool {
    area.cols > 0 && area.rows > 0
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn a_handle_whose_task_is_gone_reports_it_rather_than_hanging() {
        let (commands, rx) = mpsc::channel(1);
        let handle = SessionHandle { commands };
        drop(rx);
        assert_eq!(handle.input(vec![b'x']).await, Err(SessionGone));
        assert_eq!(handle.resize(Size::new(80, 24)).await, Err(SessionGone));
        assert_eq!(handle.snapshot().await.err(), Some(SessionGone));
    }
}
