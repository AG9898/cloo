//! Integration coverage for split and close through the session actor.
//!
//! These drive real pseudoterminals, so they live in `tests/` rather than in a
//! `#[cfg(test)]` module — see `docs/TESTING.md`.
//!
//! Every child here reports its own `winsize` **on demand**, when a line is
//! written to it, rather than on a loop. That is deliberate: a pane that has
//! never been asked shows nothing, so an assertion on a report can only pass if
//! the report was produced after the split or close under test. A looping
//! reporter would leave its old answer on the grid and pass whether or not
//! anything still worked.
//!
//! What is being proved throughout is one property: the layout tree and the set
//! of PTYs the session owns always name the same panes. A split the layout
//! refuses spawns nothing, a close drops the pane's PTY in the same turn that
//! collapses its parent, and every surviving pane is given the geometry of the
//! layout pass that followed.

use std::time::Duration;

use cloo_core::LayoutError;
use cloo_proto::{Direction, PaneId, Size};
use cloo_server::pty::PtyConfig;
use cloo_server::session::{PaneError, Session, SessionHandle, SessionSnapshot};
use cloo_term::TermSize;

/// How long a child gets to answer before a test gives up.
///
/// Generous, because it is a failure deadline and not a delay: every wait polls
/// and returns as soon as the condition holds.
const DEADLINE: Duration = Duration::from_secs(20);

/// A child that reports its terminal size once per line it is given.
///
/// On demand rather than on a loop, so a report on the grid is always evidence
/// of something that happened after the last thing the test did.
const REPORT_ON_DEMAND: &str = "while read _; do stty size; done";

/// A config running `script` under `sh`, at `cols` x `rows`.
fn scripted(script: &str, cols: u16, rows: u16) -> PtyConfig {
    let size = TermSize::new(cols, rows).expect("test sizes are non-zero");
    PtyConfig::new("sh")
        .arg("-c")
        .arg(script)
        .env("TERM", "xterm-256color")
        .size(size)
}

/// The focused pane's grid as lines, trailing blanks trimmed.
fn text(snapshot: &SessionSnapshot) -> Vec<String> {
    snapshot
        .pane
        .rows
        .iter()
        .map(|row| row.cells.iter().map(|cell| cell.ch).collect::<String>())
        .map(|line| line.trim_end().to_owned())
        .collect()
}

/// Asks the focused pane's child for its size and waits for the answer.
///
/// The answer must be the *last* thing on the grid, so an older identical
/// report cannot satisfy it.
async fn ask_size(handle: &SessionHandle, expected: &str) {
    handle
        .input(b"\n".to_vec())
        .await
        .expect("the session must be alive");

    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = handle.snapshot().await.expect("the session must be alive");
        let lines = text(&snapshot);
        if lines
            .iter()
            .rfind(|line| !line.is_empty())
            .is_some_and(|line| line == expected)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the focused pane never reported {expected:?}; it shows {lines:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The rectangle the layout gave `pane`.
fn rect_of(snapshot: &SessionSnapshot, pane: PaneId) -> Size {
    snapshot
        .panes
        .iter()
        .find(|rect| rect.pane == pane)
        .unwrap_or_else(|| panic!("pane {pane:?} is not in the layout"))
        .size
}

/// A session of one pane running [`REPORT_ON_DEMAND`] at `cols` x `rows`.
fn session(cols: u16, rows: u16) -> (PaneId, cloo_server::session::SpawnedSession) {
    let root = PaneId::new(0);
    let spawned = Session::spawn(&scripted(REPORT_ON_DEMAND, cols, rows), root)
        .expect("a pty and an sh child must be available");
    (root, spawned)
}

#[tokio::test]
async fn a_split_creates_a_pane_whose_child_starts_at_its_own_geometry() {
    let (root, session) = session(120, 40);

    let new_pane = session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("a 120x40 session has room for two panes");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.panes.len(), 2, "the layout must hold both panes");
    assert_eq!(
        snapshot.focused, new_pane,
        "focus follows the split, so typing goes where a user is looking"
    );
    assert_eq!(rect_of(&snapshot, root), Size::new(60, 40));
    assert_eq!(rect_of(&snapshot, new_pane), Size::new(60, 40));

    // The child's own `stty size` is the half a layout assertion cannot reach:
    // the new pane has its own PTY, created at the geometry that layout pass
    // produced rather than at the session's full area.
    ask_size(&session.handle, "40 60").await;
}

#[tokio::test]
async fn closing_a_pane_collapses_the_layout_and_regrows_the_survivor() {
    let (root, session) = session(120, 40);

    let new_pane = session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("the split must fit");
    ask_size(&session.handle, "40 60").await;

    session
        .handle
        .close(new_pane)
        .await
        .expect("closing a pane that is not the last one must succeed");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(
        snapshot.panes.len(),
        1,
        "the parent split must have collapsed"
    );
    assert_eq!(
        snapshot.focused, root,
        "focus must land on a pane that still exists"
    );
    assert_eq!(rect_of(&snapshot, root), Size::new(120, 40));

    // The survivor's child is told about its new geometry, which is the rest of
    // ownership: the close ran a layout pass and pushed it down. This is the
    // root pane's first report of any kind, so it cannot be a stale one.
    ask_size(&session.handle, "40 120").await;
}

#[tokio::test]
async fn a_split_with_no_room_is_refused_and_changes_nothing() {
    // Two panes need 40 columns; this session has 30.
    let (root, session) = session(30, 10);

    let err = session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect_err("30 columns cannot hold two 20-column panes");
    assert!(
        matches!(err, PaneError::Layout(LayoutError::TooSmall { .. })),
        "unexpected error: {err:?}"
    );

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(
        snapshot.panes.len(),
        1,
        "a refused split must leave the layout exactly as it was"
    );
    assert_eq!(snapshot.focused, root, "focus must not have moved");
    assert_eq!(rect_of(&snapshot, root), Size::new(30, 10));

    // And the pane it refused to split still has its PTY: it answers, and at
    // the size it always had.
    ask_size(&session.handle, "10 30").await;
}

#[tokio::test]
async fn closing_the_last_pane_is_refused_and_leaves_its_child_running() {
    let (root, session) = session(80, 24);

    let err = session
        .handle
        .close(root)
        .await
        .expect_err("a session with no panes is ended, not represented");
    assert!(
        matches!(err, PaneError::Layout(LayoutError::LastPane(_))),
        "unexpected error: {err:?}"
    );

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.panes.len(), 1);

    // The refusal cost the child nothing: it is still there to be resized and
    // still answering.
    session
        .handle
        .resize(Size::new(100, 30))
        .await
        .expect("session is alive");
    ask_size(&session.handle, "30 100").await;
}

#[tokio::test]
async fn closing_an_unknown_pane_is_refused() {
    let (_, session) = session(80, 24);

    let err = session
        .handle
        .close(PaneId::new(99))
        .await
        .expect_err("pane 99 was never created");
    assert!(
        matches!(err, PaneError::Layout(LayoutError::UnknownPane(_))),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        session
            .handle
            .snapshot()
            .await
            .expect("session is alive")
            .panes
            .len(),
        1
    );
}

#[tokio::test]
async fn a_resize_is_divided_between_every_pane() {
    let (root, session) = session(120, 40);

    let new_pane = session
        .handle
        .split_even(Direction::Vertical)
        .await
        .expect("the split must fit");
    ask_size(&session.handle, "20 120").await;

    session
        .handle
        .resize(Size::new(160, 60))
        .await
        .expect("session is alive");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(rect_of(&snapshot, root), Size::new(160, 30));
    assert_eq!(rect_of(&snapshot, new_pane), Size::new(160, 30));

    // Only the focused pane's grid is observable from here — per-pane contents
    // reach the wire with M2-03 — but the rects above and this report come from
    // the same layout pass that drove both children's `TIOCSWINSZ`.
    ask_size(&session.handle, "30 160").await;
}
