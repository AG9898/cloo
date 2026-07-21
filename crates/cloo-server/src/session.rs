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
use cloo_proto::{
    MouseButton, MouseEvent, MouseKind, MouseTracking, PaneId, PaneModes, PaneRect, Size,
};
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
            Command::Paste(text) => self.reactor.write_all(&paste_bytes(self.modes(), &text)),
            Command::Focus(focused) => match focus_bytes(self.modes(), focused) {
                Some(bytes) => self.reactor.write_all(bytes),
                // The application never asked to hear about focus. Saying
                // nothing is the whole of the fallback.
                None => Ok(()),
            },
            Command::Mouse(event) => {
                // A mouse event names the pane the client hit-tested it into. A
                // stale one naming some other pane is dropped rather than
                // delivered to whatever is focused now.
                if event.pane != self.focused {
                    return Ok(());
                }
                match mouse_bytes(self.modes(), &event) {
                    Some(bytes) => self.reactor.write_all(&bytes),
                    None => Ok(()),
                }
            }
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
            modes: self.modes(),
        }
    }

    /// What the focused pane's application has negotiated.
    fn modes(&self) -> PaneModes {
        cloo_core::grid::wire_modes(self.reactor.emulator().modes())
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
