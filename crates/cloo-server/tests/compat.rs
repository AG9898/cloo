//! The deterministic terminal-compatibility fixture suite.
//!
//! This is the automated gate the compatibility contract in
//! [`docs/AGENT_WORKFLOWS.md`](../../../docs/AGENT_WORKFLOWS.md) refers to. Every
//! fixture drives a scripted `sh -c` child that emits — or is sent — one required
//! or negotiated terminal capability sequence, and asserts cloo's server-side
//! semantics through the session actor. There is one fixture per category the
//! contract names: **screens** (alternate screen), **paste** (bracketed paste and
//! its fallback), **keys** (an extended-key sequence reaching a child verbatim),
//! **focus** (focus reporting and its fallback), **mouse** (SGR mouse and its
//! fallback), **effects** (typed effects crossing while arbitrary escape payloads
//! are dropped), and **resize** (both the grid and the child hearing it).
//!
//! Nothing here needs a vendor CLI, a login, or a moving release. A real Codex or
//! Claude Code smoke run is versioned manual evidence, per `docs/TESTING.md`, not
//! a dependency of this suite. The end-to-end wire versions of the input paths
//! live in `crates/cloo/tests/attach.rs`, because those need both halves of the
//! wire and `cloo-server` may never name `cloo-client`; this file proves the same
//! semantics one layer down, at the session actor, where the escape sequences a
//! harness actually emits are processed.

use std::time::Duration;

use cloo_core::pane::{PaneName, TaskLabel, WorkingDir};
use cloo_core::profile::{Profile, ProfileCommand};
use cloo_proto::{
    MouseButton, MouseEvent, MouseKind, MouseMods, OuterTerminalEffect, PaneId, PaneModes, Point,
    Size,
};
use cloo_server::launch::Launch;
use cloo_server::pty::PtyConfig;
use cloo_server::session::{Session, SessionEvent, SessionHandle, SessionSnapshot, SpawnedSession};
use tokio::sync::mpsc;

/// How long a fixture waits for a child before it gives up. A failure deadline,
/// not a delay: every wait polls and returns the instant its condition holds.
const DEADLINE: Duration = Duration::from_secs(20);

/// The session's half of a config at `cols` x `rows`: geometry and a resolvable
/// `TERM`, which is the baseline every capability negotiates from.
fn base(cols: u16, rows: u16) -> PtyConfig {
    PtyConfig::session(Size::new(cols, rows))
        .expect("test sizes are non-zero")
        .env("TERM", "xterm-256color")
}

/// A launch running `script` under `sh`, as the `generic` profile.
///
/// Built exactly the way the CLI builds one for an explicitly named program: a
/// profile value with its command replaced, never a vendor special case.
fn scripted(script: &str) -> Launch {
    let mut profile = Profile::generic();
    profile.command = ProfileCommand::Program {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
    };
    Launch::new(
        profile,
        Some(PaneName::new("compat").expect("a valid test name")),
        Some(TaskLabel::new("fixture").expect("a valid test label")),
        WorkingDir::new("/").expect("absolute"),
    )
    .expect("the generic profile validates")
}

/// A session of one pane running `script` at `cols` x `rows`.
fn session_running(script: &str, cols: u16, rows: u16) -> SpawnedSession {
    let root = PaneId::new(0);
    Session::spawn(&base(cols, rows), root, scripted(script))
        .expect("a pty and an sh child must be available")
}

/// A child that echoes what it is sent, with escape bytes stripped so the result
/// is readable on the grid.
///
/// `-echo` keeps the pty's own echo out of the rows and `-icanon` is what lets a
/// report with no newline in it be read at all; `tr` is what turns an escape
/// sequence into assertable text. It prints `ready` first, so a test knows the
/// mode it negotiated is live before it sends the event that mode gates.
fn echoing(enable: &str, bytes: usize) -> String {
    format!(
        "stty -echo -icanon; printf '{enable}'; printf 'ready\\n'; \
         head -c {bytes} | tr -d '\\033'"
    )
}

/// The current picture, or a failure if the actor stopped answering.
///
/// Every snapshot goes through here rather than awaiting the handle directly:
/// an unbounded await turns a wedged actor into a suite that hangs forever
/// instead of a test that fails and names the stall.
async fn snapshot_now(handle: &SessionHandle) -> SessionSnapshot {
    tokio::time::timeout(DEADLINE, handle.snapshot())
        .await
        .expect("the session actor must answer a snapshot rather than block")
        .expect("the session must be alive")
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

/// Waits for the focused pane's grid to contain a line exactly equal to `line`.
async fn wait_for_exact(handle: &SessionHandle, line: &str) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = snapshot_now(handle).await;
        let lines = text(&snapshot);
        if lines.iter().any(|shown| shown == line) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the line {line:?} never appeared; the pane shows {lines:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the focused pane's negotiated modes satisfy `ready`.
async fn wait_for_modes(handle: &SessionHandle, ready: impl Fn(PaneModes) -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = snapshot_now(handle).await;
        if ready(snapshot.modes) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pane never negotiated the expected modes; it reports {:?}",
            snapshot.modes
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sends a newline and waits for the focused pane's child to report the size it
/// prints with `stty size`, which is `rows cols`. The report must be the last
/// non-blank line, so an older identical answer cannot satisfy it.
async fn ask_size(handle: &SessionHandle, expected: &str) {
    handle
        .input(b"\n".to_vec())
        .await
        .expect("the session must be alive");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = snapshot_now(handle).await;
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

// --- Required tier -------------------------------------------------------

#[tokio::test]
async fn the_alternate_screen_round_trips_and_preserves_the_primary_grid() {
    // A full-screen harness (Codex, Claude Code, vim) lives on the alternate
    // screen. The contract is that entering it hides — but keeps — the primary
    // grid, and leaving it restores that grid untouched. The child homes the
    // cursor after switching so the assertion has a known position.
    let script = "printf 'primary'; printf '\\033[?1049h\\033[H'; \
                  printf 'alt-screen'; read _; printf '\\033[?1049l'; read _";
    let SpawnedSession { handle, .. } = session_running(script, 80, 24);

    // On the alternate screen the primary content is gone from view.
    wait_for_exact(&handle, "alt-screen").await;
    let on_alt = text(&snapshot_now(&handle).await);
    assert!(
        !on_alt.iter().any(|line| line == "primary"),
        "the primary grid must be hidden on the alternate screen; saw {on_alt:?}"
    );

    // Leaving the alternate screen brings the primary grid back exactly.
    handle
        .input(b"\n".to_vec())
        .await
        .expect("the session must be alive");
    wait_for_exact(&handle, "primary").await;
    let restored = text(&snapshot_now(&handle).await);
    assert!(
        !restored.iter().any(|line| line == "alt-screen"),
        "the alternate screen must not survive the switch back; saw {restored:?}"
    );
}

#[tokio::test]
async fn a_paste_is_bracketed_only_when_the_child_negotiated_it() {
    // `\x1b[200~hi\x1b[201~` is 14 raw bytes; stripped of its escape bytes the
    // brackets themselves are what the grid shows.
    let SpawnedSession { handle, .. } = session_running(&echoing("\\033[?2004h", 14), 80, 24);

    wait_for_exact(&handle, "ready").await;
    wait_for_modes(&handle, |modes| modes.bracketed_paste).await;

    handle
        .paste(b"hi".to_vec())
        .await
        .expect("the paste must reach the session");
    wait_for_exact(&handle, "[200~hi[201~").await;
}

#[tokio::test]
async fn a_paste_to_a_child_that_did_not_negotiate_it_is_plain_typing() {
    // The documented fallback: with no bracketed-paste mode set, pasted text
    // arrives as ordinary typing, delimiter-free.
    let SpawnedSession { handle, .. } = session_running(&echoing("", 2), 80, 24);

    wait_for_exact(&handle, "ready").await;
    handle
        .paste(b"hi".to_vec())
        .await
        .expect("the paste must reach the session");
    wait_for_exact(&handle, "hi").await;
}

#[tokio::test]
async fn an_extended_key_sequence_reaches_the_child_verbatim() {
    // Ctrl-Up is `\x1b[1;5A` — six raw bytes carrying a modifier parameter. cloo
    // forwards keyboard bytes untouched, so a harness that reads extended keys
    // sees exactly what the terminal sent.
    let SpawnedSession { handle, .. } = session_running(&echoing("", 6), 80, 24);

    wait_for_exact(&handle, "ready").await;
    handle
        .input(b"\x1b[1;5A".to_vec())
        .await
        .expect("the keystroke must reach the session");
    wait_for_exact(&handle, "[1;5A").await;
}

#[tokio::test]
async fn focus_reporting_is_negotiated_and_delivered() {
    // `\x1b[I` (focus in) is three raw bytes, two of them printable.
    let SpawnedSession { handle, .. } = session_running(&echoing("\\033[?1004h", 3), 80, 24);

    wait_for_exact(&handle, "ready").await;
    wait_for_modes(&handle, |modes| modes.focus_events).await;

    handle
        .focus(true)
        .await
        .expect("the focus report must reach the session");
    wait_for_exact(&handle, "[I").await;
}

#[tokio::test]
async fn focus_to_a_child_that_did_not_negotiate_it_is_silent() {
    // A child that never enabled focus reporting must receive no bytes from a
    // focus change. The typed `done` that follows is the proof: had a report been
    // forwarded, the four bytes the child reads would start with it instead.
    let SpawnedSession { handle, .. } = session_running(&echoing("", 4), 80, 24);

    wait_for_exact(&handle, "ready").await;
    handle.focus(true).await.expect("focus must send");
    handle
        .input(b"done".to_vec())
        .await
        .expect("input must reach the child");
    wait_for_exact(&handle, "done").await;
}

#[tokio::test]
async fn an_sgr_mouse_report_is_negotiated_and_delivered() {
    // `\x1b[?1000h` asks for click tracking and `\x1b[?1006h` for the SGR
    // encoding. A left press at cell (0,0) then encodes as `\x1b[<0;1;1M` —
    // one-based — nine raw bytes.
    let SpawnedSession { handle, .. } =
        session_running(&echoing("\\033[?1000h\\033[?1006h", 9), 80, 24);

    wait_for_exact(&handle, "ready").await;
    wait_for_modes(&handle, |modes| {
        modes.sgr_mouse && modes.mouse != cloo_proto::MouseTracking::Off
    })
    .await;

    handle
        .mouse(MouseEvent {
            pane: PaneId::new(0),
            at: Point::new(0, 0),
            kind: MouseKind::Press(MouseButton::Left),
            mods: MouseMods::NONE,
        })
        .await
        .expect("the mouse event must reach the session");
    wait_for_exact(&handle, "[<0;1;1M").await;
}

#[tokio::test]
async fn a_mouse_event_to_a_child_that_did_not_negotiate_it_is_silent() {
    // With no mouse mode set, a mouse event must put nothing into the child's
    // input; the typed `done` reads back clean.
    let SpawnedSession { handle, .. } = session_running(&echoing("", 4), 80, 24);

    wait_for_exact(&handle, "ready").await;
    handle
        .mouse(MouseEvent {
            pane: PaneId::new(0),
            at: Point::new(3, 5),
            kind: MouseKind::Press(MouseButton::Left),
            mods: MouseMods::NONE,
        })
        .await
        .expect("the mouse event must send");
    handle
        .input(b"done".to_vec())
        .await
        .expect("input must reach the child");
    wait_for_exact(&handle, "done").await;
}

#[tokio::test]
async fn a_resize_reaches_both_the_grid_and_the_child() {
    // Resize is two operations that a single assertion would let pass with the
    // other missing: the grid reflows and the child is told through `TIOCSWINSZ`.
    // Both halves are asserted from the same session.
    let SpawnedSession { handle, .. } = session_running("while read _; do stty size; done", 80, 24);

    ask_size(&handle, "24 80").await;

    handle
        .resize(Size::new(60, 20))
        .await
        .expect("the session must be alive");

    // The grid half: the sole pane's rectangle is the new area.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = snapshot_now(&handle).await;
        let rect = snapshot
            .panes
            .iter()
            .find(|rect| rect.pane == snapshot.focused)
            .expect("the focused pane is in the layout");
        if rect.size == Size::new(60, 20) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the grid never reflowed to 60x20; it is {:?}",
            rect.size
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The PTY half: the child's own `stty size` reports the new geometry.
    ask_size(&handle, "20 60").await;
}

// --- Negotiated tier and renderer-bypass safety --------------------------

/// Drains the session's event channel and returns the typed effects it carried.
///
/// The actor never blocks on this channel — a full one parks the event in an
/// outbox, not the actor — so draining is what lets the queued effects flow. The
/// loop returns once the channel is quiet for a beat, which is also what proves a
/// dropped sequence produced *no* effect: nothing more ever arrives.
async fn collect_effects(events: &mut mpsc::Receiver<SessionEvent>) -> Vec<OuterTerminalEffect> {
    let mut effects = Vec::new();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout(Duration::from_millis(300), events.recv()).await {
            Ok(Some(SessionEvent::Effect { effect, .. })) => effects.push(effect),
            // Output is coalesced and Exited is terminal; neither is what this
            // fixture measures, so keep draining.
            Ok(Some(_)) => {}
            // The channel closed, or it has been quiet long enough that no
            // further effect is coming.
            Ok(None) | Err(_) => break,
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the effect stream never went quiet"
        );
    }
    effects
}

#[tokio::test]
async fn typed_effects_cross_while_arbitrary_escape_payloads_are_dropped() {
    // In one ordered burst the child emits: a title change (OSC 2), a sixel DCS,
    // an iTerm-style notification (OSC 9), and a clipboard store (OSC 52, base64
    // `hi`), then exits. Only the two allowlisted, typed effects may reach the
    // event stream. The sixel and the OSC 9 must vanish — an arbitrary OSC or DCS
    // payload cannot become an effect and so cannot bypass the renderer. The
    // whole `spawned` is kept so its `handle` outlives the drain: were it
    // dropped, the actor would break and block on the child before flushing.
    let script = "printf '\\033]2;agent task\\007'; \
                  printf '\\033Pq\\033\\\\'; \
                  printf '\\033]9;ping\\007'; \
                  printf '\\033]52;c;aGk=\\007'";
    let mut spawned = session_running(script, 80, 24);

    let effects = collect_effects(&mut spawned.events).await;

    assert_eq!(
        effects,
        vec![
            OuterTerminalEffect::SetTitle("agent task".to_owned()),
            OuterTerminalEffect::ClipboardStore {
                target: cloo_proto::ClipboardTarget::Clipboard,
                text: "hi".to_owned(),
            },
        ],
        "only the two typed effects may cross; the sixel and OSC 9 must be dropped"
    );
}
