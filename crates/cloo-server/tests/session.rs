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

use cloo_core::pane::{PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{Profile, ProfileCommand, ProfileId};
use cloo_core::{LayoutError, Side};
use cloo_proto::{Direction, PaneId, Size};
use cloo_server::launch::Launch;
use cloo_server::pty::PtyConfig;
use cloo_server::session::{PaneError, Session, SessionHandle, SessionSnapshot};

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

/// The same reporter, prefixed by the child's own process id.
///
/// That first line is the evidence a zoom did not restart anything: a pane whose
/// PTY had been torn down and spawned again would answer with a different pid on
/// a freshly cleared grid.
const PID_THEN_REPORT: &str = "echo pid=$$; while read _; do stty size; done";

/// The session's half of a config at `cols` x `rows`: geometry and `TERM`.
fn base(cols: u16, rows: u16) -> PtyConfig {
    PtyConfig::session(Size::new(cols, rows))
        .expect("test sizes are non-zero")
        .env("TERM", "xterm-256color")
}

/// A launch running `script` under `sh`, as the `generic` profile.
///
/// Built the same way the CLI builds one for an explicitly named program: a
/// profile value with its command replaced, never a special case.
fn scripted(script: &str) -> Launch {
    launch_named(script, None, None)
}

/// The same launch under a user-supplied name and task label.
fn launch_named(script: &str, name: Option<&str>, task: Option<&str>) -> Launch {
    let mut profile = Profile::generic();
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
    };
    Launch::new(
        profile,
        name.map(|name| PaneName::new(name).expect("a valid test name")),
        task.map(|task| TaskLabel::new(task).expect("a valid test label")),
        WorkingDir::new("/").expect("absolute"),
    )
    .expect("the generic profile validates")
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
    session_running(REPORT_ON_DEMAND, cols, rows)
}

/// A session of one pane running `script` at `cols` x `rows`.
fn session_running(
    script: &str,
    cols: u16,
    rows: u16,
) -> (PaneId, cloo_server::session::SpawnedSession) {
    let root = PaneId::new(0);
    let spawned = Session::spawn(&base(cols, rows), root, scripted(script))
        .expect("a pty and an sh child must be available");
    (root, spawned)
}

/// Waits for the focused pane's grid to show a line starting with `prefix`, and
/// returns it. The identity a later assertion compares against.
async fn wait_for_line(handle: &SessionHandle, prefix: &str) -> String {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = handle.snapshot().await.expect("the session must be alive");
        let lines = text(&snapshot);
        if let Some(line) = lines.iter().find(|line| line.starts_with(prefix)) {
            return line.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no line starting with {prefix:?} appeared; the pane shows {lines:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
async fn focus_moves_between_panes_and_input_follows_it() {
    // An uneven split, so the two panes have different sizes and a report can
    // only have come from one of them.
    let (root, session) = session(120, 40);
    let right = session
        .handle
        .split(Direction::Horizontal, 0.25)
        .await
        .expect("30 and 90 columns both clear the minimum");
    assert_eq!(
        session
            .handle
            .snapshot()
            .await
            .expect("session is alive")
            .focused,
        right,
        "focus follows a split"
    );

    session
        .handle
        .move_focus(Side::Left)
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.focused, root);
    // The keystroke that asks for a report goes to whichever pane is focused,
    // so this answer is the whole proof that the move took effect.
    ask_size(&session.handle, "40 30").await;

    // Nothing is left of the leftmost pane. Asking to go there is not an error
    // and must not wrap around to the far side.
    session
        .handle
        .move_focus(Side::Left)
        .await
        .expect("session is alive");
    assert_eq!(
        session
            .handle
            .snapshot()
            .await
            .expect("session is alive")
            .focused,
        root,
        "an edge pane stays put rather than wrapping"
    );

    session
        .handle
        .move_focus(Side::Right)
        .await
        .expect("session is alive");
    assert_eq!(
        session
            .handle
            .snapshot()
            .await
            .expect("session is alive")
            .focused,
        right
    );
    ask_size(&session.handle, "40 90").await;
}

#[tokio::test]
async fn zoom_fills_the_area_and_unzoom_restores_the_split_without_a_restart() {
    let (root, session) = session_running(PID_THEN_REPORT, 120, 40);
    session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("the split must fit");
    session
        .handle
        .move_focus(Side::Left)
        .await
        .expect("session is alive");
    ask_size(&session.handle, "40 60").await;
    let pid = wait_for_line(&session.handle, "pid=").await;

    session
        .handle
        .toggle_zoom()
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.zoomed, Some(root));
    assert_eq!(
        snapshot.panes.len(),
        1,
        "a zoomed session draws the focused pane and nothing else"
    );
    assert_eq!(rect_of(&snapshot, root), Size::new(120, 40));
    // The child was resized, not replaced: it is the same process, and it says
    // so on the same grid it has been writing to all along.
    ask_size(&session.handle, "40 120").await;
    assert_eq!(
        wait_for_line(&session.handle, "pid=").await,
        pid,
        "zoom must never restart a pane's child"
    );

    session
        .handle
        .toggle_zoom()
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.zoomed, None);
    assert_eq!(
        snapshot.panes.len(),
        2,
        "unzoom restores the split that was there the whole time"
    );
    assert_eq!(
        rect_of(&snapshot, root),
        Size::new(60, 40),
        "the ratio survived the zoom untouched"
    );
    ask_size(&session.handle, "40 60").await;
    assert_eq!(
        wait_for_line(&session.handle, "pid=").await,
        pid,
        "unzoom must never restart a pane's child either"
    );
}

#[tokio::test]
async fn a_split_while_zoomed_shows_the_pane_it_created() {
    let (root, session) = session(120, 40);
    session
        .handle
        .toggle_zoom()
        .await
        .expect("session is alive");
    assert_eq!(
        session
            .handle
            .snapshot()
            .await
            .expect("session is alive")
            .zoomed,
        Some(root)
    );

    let new_pane = session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("the split must fit");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(
        snapshot.zoomed, None,
        "a split unzooms, or the pane it just made is invisible"
    );
    assert_eq!(snapshot.focused, new_pane);
    assert_eq!(snapshot.panes.len(), 2);
    ask_size(&session.handle, "40 60").await;
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

// --- Launching from an explicit profile (M2-06) -----------------------------

/// The metadata a snapshot carries for one pane.
fn info_of(snapshot: &SessionSnapshot, pane: PaneId) -> cloo_proto::PaneInfo {
    snapshot
        .metas
        .iter()
        .find(|info| info.pane == pane)
        .unwrap_or_else(|| {
            panic!(
                "no metadata for {pane:?}; snapshot has {:?}",
                snapshot.metas
            )
        })
        .clone()
}

#[tokio::test]
async fn a_launch_carries_its_metadata_into_every_snapshot() {
    let (root, session) = session(120, 40);

    let new_pane = session
        .handle
        .launch(
            Direction::Horizontal,
            0.5,
            launch_named(REPORT_ON_DEMAND, Some("api"), Some("fix the flaky test")),
        )
        .await
        .expect("a 120x40 session has room for two panes");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(
        snapshot.metas.len(),
        2,
        "every laid-out pane must have an identity"
    );

    let launched = info_of(&snapshot, new_pane);
    assert_eq!(launched.name, "api");
    assert_eq!(launched.task.as_deref(), Some("fix the flaky test"));
    assert_eq!(launched.profile, "generic");
    assert_eq!(launched.cwd, "/");

    // The pane that was split is untouched by what was launched beside it, and
    // it kept the session's own launch rather than inheriting a name.
    let original = info_of(&snapshot, root);
    assert_eq!(original.name, "shell");
    assert_eq!(
        original.task, None,
        "a task is only ever what the user said, so a pane nobody labelled has none"
    );
}

#[tokio::test]
async fn a_launched_pane_starts_in_the_directory_it_was_given() {
    // The half a metadata assertion cannot reach: the child's own `pwd`. A
    // `cwd` that only reached the snapshot would look right and run in the
    // wrong place.
    let mut profile = Profile::generic();
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            "pwd; while read _; do pwd; done".to_owned(),
        ],
    };
    let launch = Launch::new(
        profile,
        None,
        None,
        WorkingDir::new("/usr").expect("absolute"),
    )
    .expect("valid");

    let (_, session) = session(120, 40);
    let _ = session
        .handle
        .launch(Direction::Horizontal, 0.5, launch)
        .await
        .expect("the split must fit");

    assert_eq!(wait_for_line(&session.handle, "/usr").await, "/usr");
}

#[tokio::test]
async fn a_profile_naming_a_missing_program_fails_clearly_and_changes_nothing() {
    let (root, session) = session(120, 40);

    let mut profile = Profile::generic();
    profile.command = ProfileCommand::program("cloo-no-such-program-exists");
    let launch = Launch::new(profile, None, None, WorkingDir::new("/").expect("absolute"))
        .expect("the profile itself is well-formed; only the program is missing");

    let err = session
        .handle
        .launch(Direction::Horizontal, 0.5, launch)
        .await
        .expect_err("a program that is not on PATH cannot be launched");

    assert!(matches!(err, PaneError::Spawn(_)), "got {err}");
    let message = err.to_string();
    assert!(
        message.contains("cloo-no-such-program-exists"),
        "the error must name the program, got: {message}"
    );
    assert!(
        message.contains("PATH"),
        "the error must say where it was looked for, got: {message}"
    );

    // The layout was rolled back: the session is exactly what it was.
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.metas.len(), 1);
    assert_eq!(snapshot.focused, root);
    ask_size(&session.handle, "40 120").await;
}

#[tokio::test]
async fn a_plain_split_repeats_the_sessions_own_launch() {
    // The default a keybinding means by "split": the same thing again, with the
    // same identity, rather than a nameless pane.
    let root = PaneId::new(0);
    let spawned = Session::spawn(
        &base(120, 40),
        root,
        launch_named(REPORT_ON_DEMAND, Some("build"), Some("watch the tests")),
    )
    .expect("a pty and an sh child must be available");

    let new_pane = spawned
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("the split must fit");

    let snapshot = spawned.handle.snapshot().await.expect("session is alive");
    let repeated = info_of(&snapshot, new_pane);
    let original = info_of(&snapshot, root);
    assert_eq!(repeated.profile, original.profile);
    assert_eq!(repeated.name, original.name);
    assert_eq!(repeated.task, original.task);
    assert_eq!(repeated.cwd, original.cwd);
    assert_ne!(repeated.pane, original.pane, "two panes, one launch");
}

#[tokio::test]
async fn a_named_profile_reaches_the_pane_it_launched() {
    // A profile with a real ID, so the snapshot proves the *profile* travelled
    // and not just the command. `sh` stands in for a harness cloo does not
    // require to be installed.
    let mut profile = Profile::new(
        ProfileId::new("harness").expect("valid id"),
        ProfileCommand::Program {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), REPORT_ON_DEMAND.to_owned()],
        },
        "harness",
    );
    profile.min_size = cloo_core::MIN_PANE_SIZE;
    let launch =
        Launch::new(profile, None, None, WorkingDir::new("/").expect("absolute")).expect("valid");

    let (_, session) = session(120, 40);
    let new_pane = session
        .handle
        .launch(Direction::Horizontal, 0.5, launch)
        .await
        .expect("the split must fit");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    let info = info_of(&snapshot, new_pane);
    assert_eq!(info.profile, "harness");
    assert_eq!(
        info.name, "harness",
        "an unnamed pane takes the profile's default name"
    );
}

// --- Attention state through the session actor (M2-07) ----------------------

/// The attention a snapshot carries for one pane.
fn attention_of(snapshot: &SessionSnapshot, pane: PaneId) -> cloo_proto::PaneAttention {
    snapshot
        .attention
        .iter()
        .find(|att| att.pane == pane)
        .unwrap_or_else(|| {
            panic!(
                "no attention for {pane:?}; snapshot has {:?}",
                snapshot.attention
            )
        })
        .clone()
}

#[tokio::test]
async fn attention_updates_are_serialized_through_the_session_task() {
    use cloo_core::pane::{AttentionSource, AttentionState};

    let (root, session) = session(80, 24);

    // An uninstrumented child is Unknown with no source: a live PTY is not proof
    // a harness is doing anything.
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(
        attention_of(&snapshot, root),
        cloo_proto::PaneAttention {
            pane: root,
            state: cloo_proto::AttentionState::Unknown,
            source: cloo_proto::AttentionSource::None,
            acknowledged: false,
        }
    );

    // A report reaches the state through the one command channel and shows up in
    // the next snapshot, provenance and all.
    session
        .handle
        .set_attention(root, AttentionState::NeedsInput, AttentionSource::Bell)
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    let att = attention_of(&snapshot, root);
    assert_eq!(att.state, cloo_proto::AttentionState::NeedsInput);
    assert_eq!(att.source, cloo_proto::AttentionSource::Bell);
    assert!(!att.acknowledged);

    // Acknowledging is its own command and clears only the seen flag.
    session
        .handle
        .acknowledge_attention(root)
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    let att = attention_of(&snapshot, root);
    assert_eq!(att.state, cloo_proto::AttentionState::NeedsInput);
    assert!(
        att.acknowledged,
        "the state stayed, only the seen flag moved"
    );

    // Re-reporting the same state keeps the acknowledgment — the coalescing rule
    // the queue depends on, proven through the actor rather than only in the model.
    session
        .handle
        .set_attention(root, AttentionState::NeedsInput, AttentionSource::Bell)
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert!(
        attention_of(&snapshot, root).acknowledged,
        "a re-announced state must not refill a queue the user just cleared"
    );

    // A different state clears it again.
    session
        .handle
        .set_attention(root, AttentionState::Failed, AttentionSource::Lifecycle)
        .await
        .expect("session is alive");
    let snapshot = session.handle.snapshot().await.expect("session is alive");
    let att = attention_of(&snapshot, root);
    assert_eq!(att.state, cloo_proto::AttentionState::Failed);
    assert!(!att.acknowledged, "a changed state is unseen again");
}

#[tokio::test]
async fn a_report_for_a_closed_pane_is_dropped_without_disturbing_the_survivor() {
    use cloo_core::pane::{AttentionSource, AttentionState};

    let (root, session) = session(120, 40);
    let new_pane = session
        .handle
        .split_even(Direction::Horizontal)
        .await
        .expect("a 120x40 session has room for two panes");

    session
        .handle
        .close(new_pane)
        .await
        .expect("the pane closes");

    // Naming the pane that just closed is a no-op, exactly as a stale mouse
    // event is — it must not panic or touch the surviving pane.
    session
        .handle
        .set_attention(new_pane, AttentionState::Failed, AttentionSource::Lifecycle)
        .await
        .expect("session is alive");

    let snapshot = session.handle.snapshot().await.expect("session is alive");
    assert_eq!(snapshot.attention.len(), 1, "only the survivor remains");
    assert_eq!(
        attention_of(&snapshot, root).state,
        cloo_proto::AttentionState::Unknown,
        "the survivor is untouched by a report aimed at a pane that is gone"
    );
}
