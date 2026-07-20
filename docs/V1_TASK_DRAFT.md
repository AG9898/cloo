# V1 CLI Task Draft

> This is a workboard-compatible draft, not the active workboard. `docs/workboard.json` remains
> unchanged until the project owner explicitly asks to seed it.

## Sizing Rule

Every task below is deliberately limited to one primary crate or one renderer/product surface.
An agent starting with a fresh context must be able to finish it with at least 25% context left.
If investigation reveals a second independent surface, the agent must stop and split the task
before implementing it. Unless an entry says otherwise, `status` is `todo`, `blocked_by` is `[]`,
and the verification command is the fast suite:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

IDs and groups follow the approved M0–M7 roadmap. They are ready to be materialized with the
`edit-workboard` workflow after final owner approval.

## M0 — Workspace and terminal foundation

### M0-01 — Scaffold the planned crate workspace

**Priority:** critical
**Description:** Create the five planned library crates with inherited workspace metadata and only
the dependencies needed by their initial public APIs. Keep `cloo` a thin binary.
**Depends on:** none
**Acceptance:** `cargo build --workspace` succeeds; all six crates exist; dependency directions
match the architecture; docs identify the created crates.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `Cargo.toml`, `crates/*/Cargo.toml`, `crates/*/src/lib.rs`

### M0-02 — Define protocol IDs, framing, and version handshake

**Priority:** critical
**Description:** Implement newtype IDs, postcard length framing, and the versioned handshake in
`cloo-proto`, including mismatch errors.
**Depends on:** M0-01
**Acceptance:** every wire type round-trips; incompatible versions fail cleanly; no PTY or
renderer type enters the crate.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-proto/src/`

### M0-03 — Implement the pure layout tree

**Priority:** critical
**Description:** Add pane/tab/session IDs and the ratio-based binary layout tree with split,
close, collapse, rectangle assignment, and minimum-size rejection.
**Depends on:** M0-01
**Acceptance:** table-driven unit tests cover each operation and reject zero-size PTYs; no I/O is
introduced in `cloo-core`.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/layout.rs`, `crates/cloo-core/src/id.rs`

### M0-04 — Wrap terminal emulation behind cloo-term

**Priority:** critical
**Description:** Add the exact-pinned terminal dependency only to `cloo-term` and expose byte
feeding, cell reads, resize, and scrollback through cloo-owned types.
**Depends on:** M0-01
**Acceptance:** known SGR, alternate-screen, and resize sequences update the wrapper grid in
tests; no other crate imports `alacritty_terminal`.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `Cargo.toml`, `crates/cloo-term/`

### M0-05 — Add a single-pane PTY reactor

**Priority:** critical
**Description:** Spawn a Unix PTY, read without panicking, route bytes through `cloo-term`, and
restore PTY resources on exit.
**Depends on:** M0-04
**Acceptance:** an integration test runs a scripted shell and observes its grid output; unsafe
blocks have `SAFETY` comments; read errors are returned.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `crates/cloo-server/src/pty.rs`, `crates/cloo-server/tests/pty.rs`

### M0-06 — Render a grid and restore client terminal state

**Priority:** critical
**Description:** Implement client raw mode guard, full-grid rendering, and restoration on normal,
error, panic, and signal exits.
**Depends on:** M0-02
**Acceptance:** a fake-grid renderer has deterministic output tests; every raw-mode exit path is
guarded; no ad-hoc escape printing exists.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/raw_mode.rs`, `crates/cloo-client/src/renderer.rs`

### M0-07 — Ship the one-pane local CLI smoke path

**Priority:** high
**Description:** Wire the binary, PTY reactor, and renderer into one foreground local shell that
can feed a grid and forward ordinary input.
**Depends on:** M0-05, M0-06
**Acceptance:** `cloo` launches a shell and renders its output; `--help` remains useful; no daemon
or socket behavior is added here.
**Docs:** `PRD.md`, `TESTING.md`
**Files:** `crates/cloo/src/main.rs`, `crates/cloo-server/src/`, `crates/cloo-client/src/`

## M1 — Durable session and compatibility baseline

### M1-01 — Create the daemon socket lifecycle

**Priority:** critical
**Description:** Add session socket path creation, daemon startup/ownership, and stale-socket
cleanup scoped to the configured runtime directory.
**Depends on:** M0-07
**Acceptance:** a daemon starts once per session, reports clear startup errors, and cleanup tests
do not touch unrelated sockets.
**Docs:** `ARCHITECTURE.md`, `ENV_VARS.md`, `TESTING.md`
**Files:** `crates/cloo-server/src/socket.rs`, `crates/cloo/src/main.rs`

### M1-02 — Implement attach, hello, and detach

**Priority:** critical
**Description:** Connect a client over the Unix socket, validate the handshake, deliver the
initial session snapshot, and detach without killing the PTY.
**Depends on:** M1-01, M0-02
**Acceptance:** a client can attach after server launch; a version mismatch instructs reattach;
detaching leaves the child running.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-proto/src/`, `crates/cloo-server/src/socket.rs`, `crates/cloo-client/src/attach.rs`

### M1-03 — Serialize input and resize through the session task

**Priority:** critical
**Description:** Introduce `mpsc<Command>` session ownership for input and `SIGWINCH` resize,
including PTY `TIOCSWINSZ` and a fresh layout pass.
**Depends on:** M1-02, M0-03
**Acceptance:** no session mutex exists; scripted resize reaches both grid and PTY; ordering is
covered by an integration test.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `crates/cloo-server/src/session.rs`, `crates/cloo-server/src/pty.rs`

### M1-04 — Coalesce damage and client grid diffs

**Priority:** high
**Description:** Fan out coalesced row damage on a bounded frame cadence and make clients redraw
only changed cells.
**Depends on:** M1-02, M0-06
**Acceptance:** a burst fixture produces bounded updates; lagging clients resync rather than stall
the session task; renderer-diff tests pass.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `TESTING.md`
**Files:** `crates/cloo-server/src/damage.rs`, `crates/cloo-client/src/renderer.rs`

### M1-05 — Prove detach and reattach survival

**Priority:** high
**Description:** Add the end-to-end session-survival test and user-facing reconnect flow for a
client that exits while its shell remains active.
**Depends on:** M1-03, M1-04
**Acceptance:** a scripted child remains alive through client disconnect/reconnect and retains
scrollback; test cleanup removes its socket.
**Docs:** `PRD.md`, `TESTING.md`
**Files:** `crates/cloo-server/tests/reattach.rs`, `crates/cloo-client/src/attach.rs`

### M1-06 — Negotiate baseline terminal capabilities

**Priority:** critical
**Description:** Define `TermCaps` and attach-time validation for UTF-8/color, alternate screen,
bracketed paste, extended keys, focus events, SGR mouse, and resize fallback behavior.
**Depends on:** M1-02
**Acceptance:** capability types round-trip; unsupported capability combinations choose a
documented fallback; handshake version changes with the wire addition.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-proto/src/`, `crates/cloo-client/src/capabilities.rs`

### M1-07 — Route extended input, paste, focus, and mouse modes

**Priority:** critical
**Description:** Encode/decode the negotiated interactive input modes without leaking terminal
control bytes into a pane's ordinary text input.
**Depends on:** M1-06, M1-03
**Acceptance:** fixture tests cover bracketed paste, extended keys, focus, and SGR mouse;
application-requested mouse mode and cloo chrome ownership are distinguishable.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/input.rs`, `crates/cloo-proto/src/`, `crates/cloo-server/src/session.rs`

### M1-08 — Model typed outer-terminal effects

**Priority:** high
**Description:** Add typed, versioned protocol and terminal-wrapper representations for the
allowlisted outer-terminal effects; do not yet implement each client policy.
**Depends on:** M0-02, M0-04
**Acceptance:** arbitrary OSC/DCS is not represented as a raw passthrough; effect types have
round-trip tests; unsupported graphics can be represented as unavailable.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `AGENT_WORKFLOWS.md`
**Files:** `crates/cloo-term/src/`, `crates/cloo-proto/src/`

### M1-09 — Apply client policy to outer-terminal effects

**Priority:** high
**Description:** Implement per-client allowlist and capability checks for supported effects,
including safe suppression and terminal restoration.
**Depends on:** M1-06, M1-08
**Acceptance:** allowed effects reach a capable client once; denied effects do not corrupt a grid;
two clients with different capabilities behave safely.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/effects.rs`, `crates/cloo-server/src/session.rs`

## M2 — Splits, profiles, and attention

### M2-01 — Add session commands for split and close

**Priority:** critical
**Description:** Connect pure layout operations to session commands that create/close PTYs and
collapse the layout correctly.
**Depends on:** M1-03, M0-03, M0-05
**Acceptance:** split/close commands change layout and PTY ownership atomically; minimum-size
rejection leaves the existing layout untouched.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/layout.rs`, `crates/cloo-server/src/session.rs`

### M2-02 — Add directional focus and pane zoom

**Priority:** high
**Description:** Implement pure directional focus selection and a reversible zoom state for the
focused pane.
**Depends on:** M2-01
**Acceptance:** focus traversal and zoom/unzoom are unit tested; zoom preserves the stored split
ratios and does not restart a PTY.
**Docs:** `ARCHITECTURE.md`, `STYLEGUIDE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/session.rs`, `crates/cloo-core/src/layout.rs`

### M2-03 — Render pane headers and approved focus treatment

**Priority:** high
**Description:** Render one-row pane headers, accent focus border, dimmed neighbors, compact
truncation, and accessible no-dim fallback from server-provided metadata.
**Depends on:** M2-01, M0-06
**Acceptance:** renderer tests cover focus/attention distinction and narrow panes; chrome uses no
terminal-cell screen scraping.
**Docs:** `STYLEGUIDE.md`, `DECISIONS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/renderer.rs`, `crates/cloo-client/src/chrome.rs`

### M2-04 — Define profile and pane-metadata models

**Priority:** high
**Description:** Add validated profile, pane-name, task-label, cwd, minimum-dimension, attention
state, and provenance types to `cloo-core`.
**Depends on:** M0-01
**Acceptance:** model validation is pure and tested; generic, Codex, and Claude built-ins are data
only; no vendor package or cloud API dependency is added.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/profile.rs`, `crates/cloo-core/src/pane.rs`

### M2-05 — Load and validate profile configuration

**Priority:** high
**Description:** Parse local profile definitions and merge them with built-ins without yet adding
live reload or unrelated theme configuration.
**Depends on:** M2-04
**Acceptance:** invalid profile config warns and falls back; profile command templates and size
recommendations are tested; secrets are not logged.
**Docs:** `AGENT_WORKFLOWS.md`, `ENV_VARS.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/config.rs`, `crates/cloo-core/src/profile.rs`

### M2-06 — Launch panes from an explicit profile

**Priority:** high
**Description:** Add the CLI/session command path that launches a profile with user-provided name,
task label, and working directory.
**Depends on:** M2-04, M2-05, M2-01
**Acceptance:** generic, Codex, and Claude commands can be selected without process inference;
metadata is visible in the session snapshot; a missing executable fails clearly.
**Docs:** `PRD.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo/src/`, `crates/cloo-server/src/session.rs`, `crates/cloo-proto/src/`

### M2-07 — Persist explicit attention state in the session actor

**Priority:** high
**Description:** Add session commands and protocol snapshots for pane attention state and source,
including acknowledge semantics.
**Depends on:** M2-04, M1-02
**Acceptance:** state updates are serialized through the session task; wire round-trips preserve
source; `unknown` is the default for uninstrumented live children.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/pane.rs`, `crates/cloo-proto/src/`, `crates/cloo-server/src/session.rs`

### M2-08 — Connect generic attention sources

**Priority:** high
**Description:** Map bell, child exit, and explicit user mark events to provenance-aware attention
updates without interpreting child output.
**Depends on:** M2-07, M0-05
**Acceptance:** bell and lifecycle fixtures set the specified states once; duplicate events
coalesce; no transcript or process-name matcher is added.
**Docs:** `ARCHITECTURE.md`, `CONVENTIONS.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-server/src/session.rs`, `crates/cloo-server/src/pty.rs`

### M2-09 — Expose a local adapter control interface

**Priority:** medium
**Description:** Add a small local authenticated-by-socket command that an opt-in wrapper can use
to set a pane's advisory attention state and source.
**Depends on:** M2-07
**Acceptance:** adapters can set only their permitted pane state; invalid state is rejected;
adapter events are visibly distinguished from generic sources.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-proto/src/`, `crates/cloo-server/src/socket.rs`, `crates/cloo-server/src/session.rs`

### M2-10 — Render attention summary and queue

**Priority:** high
**Description:** Add a compact attention count, keyboard queue, focus/acknowledge actions, and
bounded toast coalescing to the client chrome.
**Depends on:** M2-03, M2-08
**Acceptance:** queue order is deterministic, repeats coalesce per pane, and every state has text
plus glyph/color in the renderer tests.
**Docs:** `STYLEGUIDE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/chrome.rs`, `crates/cloo-client/src/input.rs`

## M3 — Tabs and persistent orientation

### M3-01 — Implement tab lifecycle in cloo-core

**Priority:** high
**Description:** Add named tabs, active-tab selection, and tab-local layout/pane ownership to the
pure session model.
**Depends on:** M2-01
**Acceptance:** create, rename, select, and close are unit tested; closing a tab has defined
active-tab behavior.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/session.rs`, `crates/cloo-core/src/tab.rs`

### M3-02 — Synchronize and render tabs

**Priority:** high
**Description:** Carry tab snapshots and tab commands over the protocol and render the top tab
row with compact truncation.
**Depends on:** M3-01, M1-02
**Acceptance:** tab wire round-trips pass; active-tab rendering remains legible at narrow widths;
switching does not restart pane PTYs.
**Docs:** `ARCHITECTURE.md`, `STYLEGUIDE.md`, `TESTING.md`
**Files:** `crates/cloo-proto/src/`, `crates/cloo-client/src/chrome.rs`

### M3-03 — Implement the always-on minimal status bar

**Priority:** high
**Description:** Render the required one-row minimal status bar with session, active tab,
attention count, and prefix hint; make optional segments yield by width.
**Depends on:** M3-02, M2-10
**Acceptance:** status output has 16-color and ASCII-glyph fallbacks; no segment overwrites pane
content; narrow-width tests preserve the required fields.
**Docs:** `STYLEGUIDE.md`, `DECISIONS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/chrome.rs`, `crates/cloo-client/src/renderer.rs`

### M3-04 — Add session switcher and profile launcher overlays

**Priority:** medium
**Description:** Implement keyboard-first overlays for session attach, profile launch, and pane
details using the approved shared overlay language.
**Depends on:** M3-03, M2-06
**Acceptance:** overlays are dismissible, keyboard navigable, and use a bounded render surface;
launch invokes explicit profiles only.
**Docs:** `STYLEGUIDE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/overlay.rs`, `crates/cloo-client/src/input.rs`

## M4 — Configuration, themes, and motion

### M4-01 — Implement validated configuration and SIGHUP reload

**Priority:** high
**Description:** Load the full config model, retain the profile parser, and apply atomic validated
reloads through the appropriate session/client boundaries.
**Depends on:** M2-05
**Acceptance:** invalid reload keeps the prior valid config and reports a warning; valid reload
updates without restart; `cloo-core` performs no file I/O.
**Docs:** `CONVENTIONS.md`, `ENV_VARS.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/config.rs`, `crates/cloo-server/src/config.rs`, `crates/cloo-client/src/config.rs`

### M4-02 — Add configurable keymap resolution

**Priority:** medium
**Description:** Parse key bindings into the `Action` enum and resolve conflicts while preserving
the default `C-b` prefix behavior.
**Depends on:** M4-01
**Acceptance:** defaults, overrides, conflicts, and invalid bindings are unit tested; mappings do
not consume application input outside a recognized prefix sequence.
**Docs:** `CONVENTIONS.md`, `STYLEGUIDE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/keymap.rs`, `crates/cloo-client/src/input.rs`

### M4-03 — Render theme tokens and 16-color fallbacks

**Priority:** high
**Description:** Implement Storm and named theme tokens, terminal-palette inheritance, and
semantic 16-color/ASCII fallbacks in the renderer.
**Depends on:** M4-01, M2-03
**Acceptance:** all guide tokens map deterministically; focus and attention remain distinguishable
without true color; renderer tests cover fallback output.
**Docs:** `STYLEGUIDE.md`, `DECISIONS.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/theme.rs`, `crates/cloo-client/src/theme.rs`

### M4-04 — Add interruptible reduced-motion transitions

**Priority:** medium
**Description:** Implement the 120ms focus, split, close, and overlay transition scheduler behind
the 60fps damage cap and reduce-motion configuration.
**Depends on:** M4-01, M2-03
**Acceptance:** a new input, resize, or state change interrupts prior motion; reduce-motion skips
transitions; no animation emits an update per PTY read.
**Docs:** `STYLEGUIDE.md`, `DECISIONS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/motion.rs`, `crates/cloo-client/src/renderer.rs`

## M5 — Copy mode and search

### M5-01 — Implement server-side copy and search state

**Priority:** high
**Description:** Add scrollback selection, vim-like navigation, and regex search as session-owned
state independent of client rendering.
**Depends on:** M1-05
**Acceptance:** selection/search operations are deterministic and tested; changing clients does
not lose copy state; regex errors are returned cleanly.
**Docs:** `ARCHITECTURE.md`, `TESTING.md`
**Files:** `crates/cloo-core/src/copy_mode.rs`, `crates/cloo-server/src/session.rs`

### M5-02 — Render copy mode and perform explicit OSC 52 copy

**Priority:** high
**Description:** Render copy/search highlights client-side and send selected text through the
typed clipboard effect only when client policy allows it.
**Depends on:** M5-01, M1-09
**Acceptance:** selection is visible without mutating the authoritative grid; denied clipboard
policy is safe; OSC 52 tests use fixture clients.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/copy_mode.rs`, `crates/cloo-client/src/effects.rs`

## M6 — Mouse interaction

### M6-01 — Route mouse ownership between chrome and child apps

**Priority:** high
**Description:** Decide per event whether a mouse action is cloo chrome input or an
application-requested SGR mouse event and preserve that choice through focus changes.
**Depends on:** M1-07, M2-01
**Acceptance:** fixture tests prove app mouse events are not stolen and chrome events do not leak
into pane input; state transitions disable stale modes.
**Docs:** `ARCHITECTURE.md`, `AGENT_WORKFLOWS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/input.rs`, `crates/cloo-server/src/session.rs`

### M6-02 — Add click focus, gutter drag, and scrollback wheel actions

**Priority:** medium
**Description:** Implement chrome mouse actions using ratio-based resize and the existing focus or
copy-mode commands.
**Depends on:** M6-01, M2-02, M5-01
**Acceptance:** gutter drag changes ratios only; click focus and wheel scroll have keyboard
equivalents; active gutter styling clears reliably.
**Docs:** `STYLEGUIDE.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/input.rs`, `crates/cloo-client/src/chrome.rs`

## M7 — Hardening, matrix, and distribution

### M7-01 — Harden terminal detection and reconnect races

**Priority:** critical
**Description:** Finish `$TERM`/terminfo detection, true-color behavior, stale-client recovery,
and reconnect/resize race handling without widening the terminal wrapper boundary.
**Depends on:** M1-09, M6-02
**Acceptance:** capability failures have actionable errors; reconnect race fixtures do not corrupt
grids; all protocol changes bump the handshake.
**Docs:** `ARCHITECTURE.md`, `ENV_VARS.md`, `TESTING.md`
**Files:** `crates/cloo-client/src/capabilities.rs`, `crates/cloo-server/src/session.rs`

### M7-02 — Build the deterministic compatibility fixture suite

**Priority:** high
**Description:** Add the fixture programs and integration harness for all required/negotiated
terminal capability sequences described in the compatibility contract.
**Depends on:** M7-01
**Acceptance:** fixtures cover alternate screen, paste, keys, focus, mouse, outer effects, and
resize; no fixture needs a vendor CLI or account; failures identify the sequence class.
**Docs:** `TESTING.md`, `AGENT_WORKFLOWS.md`
**Files:** `crates/cloo-server/tests/`, `crates/cloo-client/tests/`

### M7-03 — Record Codex and Claude compatibility smoke matrix

**Priority:** high
**Description:** Execute and document the manual matrix for installed Codex and Claude Code in
each supported outer terminal, including known optional graphics degradation.
**Depends on:** M7-02
**Acceptance:** results record harness/terminal versions and every matrix case; unsupported
features are documented with fallback behavior; no credentials enter the repository.
**Docs:** `TESTING.md`, `AGENT_WORKFLOWS.md`
**Files:** `docs/TESTING.md`, `docs/AGENT_WORKFLOWS.md`

### M7-04 — Package the working CLI for supported targets

**Priority:** high
**Description:** Build release artifacts and complete npm optional-dependency packaging around the
already working `cloo` binary without publishing either registry.
**Depends on:** M7-03
**Acceptance:** release builds for all four targets are reproducible; npm package exposes `cloo`;
publish commands are absent from automation.
**Docs:** `ARCHITECTURE.md`, `PRD.md`, `TESTING.md`
**Files:** `npm/`, `Cargo.toml`, release automation files
