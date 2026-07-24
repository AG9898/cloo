# cloo — Agent Working Guide

<!-- AGENTS.md is the canonical file. CLAUDE.md is a symlink to it. -->

---

## Overview

cloo is a client-server terminal multiplexer in Rust — tmux's functionality with an interface
worth looking at. It is designed first as a workspace for many concurrent coding-agent harnesses.
A daemon owns the PTYs and all session state; thin clients attach over a Unix socket and render.

**The project is pre-alpha.** Planning is complete and the design is settled. M0 is done: `cloo`
launches `$SHELL` in a single local pane, renders it, and forwards input — in-process, with no
socket and no detach. Agents here are implementing the rest of the M0–M7 roadmap in
[`docs/PRD.md`](docs/PRD.md), starting at the daemon.

The canonical task queue is [`docs/workboard.json`](docs/workboard.json), seeded with the
M0–M7 tasks.

---

## Quick Start

```bash
# Build
cargo build --workspace

# Run tests
cargo test --workspace

# Lint (must be clean)
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt            # apply
cargo fmt --check    # verify

# Run the binary
cargo run -p cloo -- --help
```

There is no server to start, no database, and no `.env` file.

---

## Build & Verification Commands

Run the fast suite before marking any task done. Never skip a fast check.

| Command | What it checks | Speed |
|---------|---------------|-------|
| `cargo fmt --check` | Formatting | fast |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lints, common bugs | fast |
| `cargo test --workspace` | Unit + integration tests | fast (today — grows with PTY integration tests) |
| `cargo build --release` | Release build with LTO | slow |

---

## Repository Structure

```
crates/
  cloo/          The binary — client-vs-server dispatch, CLI surface
  cloo-proto/    Wire types, framing, handshake version
  cloo-term/     Emulation wrapper — the ONLY crate importing alacritty_terminal
  cloo-core/     Session/tab/pane model, layout tree, keymap, config
  cloo-server/   Daemon: socket, PTY reactor, damage tracking
  cloo-client/   Attach, raw mode, renderer, theming, input encoding
docs/
  INDEX.md          Documentation navigation map
  PRD.md            Product scope, users, M0–M7 roadmap
  ARCHITECTURE.md   Topology, crate boundaries, wire protocol, layout
  CONVENTIONS.md    Rust standards and hard never/always rules
  DECISIONS.md      Decision log — resolved architecture and visual decisions
  ENV_VARS.md       Environment variable matrix
  TESTING.md        Test strategy and inventory
  STYLEGUIDE.md     Terminal chrome visual language and fallbacks
  AGENT_WORKFLOWS.md  Harness profiles, attention, and compatibility contract
  workboard.json    Canonical task queue
  workboard.schema.json  JSON Schema for the queue
  workboard.md      Workboard field definitions and usage rules
npm/
  package.json   The `clooterminal` npm package (name reservation, no bin yet)
Cargo.toml       Workspace root — shared version/edition/license metadata
```

All six crates are wired together end to end as of M0-07; the rest of their contents land across
M1–M2. Dependencies are declared in the root `[workspace.dependencies]` and are constrained by a
**layering**, not a single chain: `cloo` over {`cloo-server`, `cloo-client`} over `cloo-core` over
the leaves {`cloo-proto`, `cloo-term`}. Any crate may name any crate in a lower layer — in
particular every crate that speaks the wire names `cloo-proto` directly. Forbidden: a back-edge, a
cycle, and any edge between `cloo-server` and `cloo-client` — **including a dev-dependency**, so a
test needing both halves belongs in `crates/cloo`. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the current edge table.

Docs navigation: [`docs/INDEX.md`](docs/INDEX.md)

---

## Architecture

The constraints that matter most day to day:

- **Only `cloo-term` may import `alacritty_terminal`.** This is the load-bearing rule of the
  entire design. Emulation is a bought dependency, pinned to an exact version, and the wrapper
  boundary is what keeps it swappable.
- **The server owns all state** — PTYs, grids, scrollback, layout. Clients cache the visible
  grid and nothing else.
- **All session mutation goes through the session task** via a single `mpsc<Command>`. No
  `Mutex` on session state, ever.
- **Chrome is rendered client-side.** The server sends contents and geometry; the client decides
  what it looks like. This is why theming never touches session state.
- **The client decodes input; the server encodes it.** A paste, a focus change, and a mouse event
  cross the wire as what happened, never as bytes for a child — how they are encoded depends on
  modes the *child* set, which only the emulator sees. A mouse event the chrome owns never reaches
  the wire at all.
- **Layout stores ratios, not cell counts** — that is what survives a terminal resize. Zoom is a
  view flag over that same tree, never a reshaping of it.
- **Damage is coalesced and render rate capped (~60fps).** Architectural, not a later
  optimization. A large `cat` is the classic multiplexer killer.
- **The wire handshake is versioned.** Bump it on every protocol change.
- **Harness state is explicit.** Never infer Codex or Claude state by screen-scraping a grid.
- **Outer-terminal effects are allowlisted.** Never blindly forward OSC/DCS bytes around the
  renderer; client capability and local policy decide whether an effect is applied.

Full topology, crate responsibilities, and protocol: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)

---

## Code Style & Constraints

### Never

- Never commit secrets or credentials.
- Never bulk-rewrite `docs/workboard.json`; use targeted edits only.
- Never import `alacritty_terminal` outside `cloo-term`.
- Never use a caret/range version for `alacritty_terminal` — pin exactly.
- Never add a `Mutex` to session state.
- Never emit a render update per PTY read.
- Never await a send on an actor's own outbound channel from inside its loop — queue the event and
  select over a permit, or a slow reader becomes a deadlock.
- Never leave the terminal in raw mode on any exit path, including panic — and never leave a
  reporting mode (paste, focus, mouse) on either; register its reset with `RawMode::on_restore`.
- Never add Windows-specific code — out of scope for v1.
- Never `unwrap()` in a PTY read, socket read, or render path.

### Always

- Always run the fast verification suite before marking a task done.
- Always update relevant `docs/` files when behavior changes.
- Always write a `// SAFETY:` comment on `unsafe` blocks (expected around `libc` PTY/termios).
- Always store layout as ratios.
- Always restore terminal state on exit paths.

### Patterns

- Error handling: `Result<T, E>` with a crate-local error enum. `expect()` only in fatal
  startup paths, with a message that explains the failure.
- Concurrency: actor-shaped Tokio. One task per PTY, one session task, one per client.
- IDs: newtypes (`PaneId`, `TabId`, `SessionId`), never bare integers — they cross the wire.
- Crate metadata: inherit from the workspace with `field.workspace = true`.

Full convention guide: [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md)

---

## Maintaining Docs

Docs must stay current with the code. Update the relevant doc in the **same commit** as
the code change — never defer a doc update to a follow-up task.

| What changed | Doc to update |
|---|---|
| Topology, crate boundaries, protocol, layout | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| Coding pattern, naming rule, or never/always constraint | [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md) |
| Env var added, removed, renamed, or changed | [`docs/ENV_VARS.md`](docs/ENV_VARS.md) |
| New architectural question raised | [`docs/DECISIONS.md`](docs/DECISIONS.md) — add OPEN-XX |
| Architectural decision resolved | [`docs/DECISIONS.md`](docs/DECISIONS.md) — move to Resolved |
| Test file added, removed, or pattern changed | [`docs/TESTING.md`](docs/TESTING.md) |
| Terminal chrome, visual state, or degradation behavior changed | [`docs/STYLEGUIDE.md`](docs/STYLEGUIDE.md) |
| Harness profile, attention, or compatibility contract changed | [`docs/AGENT_WORKFLOWS.md`](docs/AGENT_WORKFLOWS.md) |
| Product scope, milestones, or success criteria changed | [`docs/PRD.md`](docs/PRD.md) |
| Any doc added, removed, renamed, or moved | [`docs/INDEX.md`](docs/INDEX.md) — always |
| Constraint or gotcha discovered during a task | This file (`AGENTS.md`) — append to Discoveries |

**Rule:** If a section in `AGENTS.md` summarizes something, and the full doc changes, update
both the summary here and the full doc in the same commit.

---

## Workboard

The canonical task queue is `docs/workboard.json`.
Schema and usage contract: [`docs/workboard.md`](docs/workboard.md).
Machine validation schema: [`docs/workboard.schema.json`](docs/workboard.schema.json).

Inspect it with the **query-workboard** skill; execute a task end-to-end with **start-task**.
Never dump the full board into context — use targeted `jq` queries.

A task is startable when:
- `status == "todo"`
- `blocked_by` is empty or missing
- all `depends_on` tasks have `status == "done"`

Targeted edit rules:
- Never rewrite the full `workboard.json`.
- Only update the status fields of the task currently being worked.
- Roll back `in_progress → todo` if blocked mid-task and unresolved.

**The board is seeded with the M0–M7 tasks.** Milestone structure lives in
[`docs/PRD.md`](docs/PRD.md) — M0 through M7, each independently runnable.

---

## Agent Workflow

Standard task cycle for this project:

1. Read this file (`AGENTS.md` / `CLAUDE.md`) at the start of every session.
2. Invoke **query-workboard** to find the next startable task.
3. Invoke **start-task** to execute it (reads docs, implements, verifies, updates board).
4. Update this file if you discovered a constraint, pattern, or pitfall worth encoding.
5. Commit changes. Summarize: what was done, what was skipped, what is next.

For multi-task runs, invoke **ralphloop** wrapping start-task with an iteration count.

### Invoking Skills

Skills live in a per-harness directory and are invoked by name with your harness's own
command prefix — `/` in Claude Code, `$` in Codex. This file deliberately names skills
without a prefix, because `AGENTS.md` and `CLAUDE.md` are the same file and cannot carry
both. Use whichever your harness expects.

Available here: **query-workboard**, **start-task**, **edit-workboard**, **project-plan**,
**ralphloop**. Sources live in the `ag.dev` repo and are rendered in by its sync script —
never edit the copies under `.claude/`, `.agents/`, or `.codex/` directly.

### Stopping Conditions

Stop and report (do not continue) when:
- No startable task exists (all are blocked or done).
- A verification command fails and the fix is not obvious.
- An irreversible action (publishing to npm or crates.io, a `git push --force`) is required and
  the task does not explicitly authorize it.
- A task would require importing `alacritty_terminal` outside `cloo-term`, or otherwise
  violating a constraint in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). Report the conflict
  rather than working around it.

---

## Debugging & Gotchas

- **Resize is a three-way race.** Grid resize, PTY `TIOCSWINSZ`, and the application's own
  `SIGWINCH` handling all interact. Serializing through the session task helps but does not
  eliminate it. This is the likeliest source of "why is vim drawing garbage."
- **A stale client attached to a rebuilt server** will happen the first time anyone rebuilds
  mid-session. That is what the versioned handshake is for — if you see inexplicable rendering
  corruption, check the handshake version before debugging the renderer.
- **A panic in a client can leave the terminal in raw mode**, which makes the shell appear
  broken afterward. `reset` restores it. Fix the exit path rather than living with it.
- **`cargo test` does not clean up stray daemons.** If integration tests fail oddly, check for
  leftover sockets in `$XDG_RUNTIME_DIR/cloo/`. The socket tests themselves bind under `$TMPDIR`
  and never touch that directory, so anything there came from a real run.
- **The npm package is `clooterminal`, not `cloo`.** npm's similarity filter rejects `cloo` at
  publish time even though the name shows as available on a registry lookup. See
  [`docs/DECISIONS.md`](docs/DECISIONS.md) RESOLVED-05.

---

## Environment Variables

cloo reads standard environment variables and owns no runtime secrets. The ones that matter for
running it locally: `XDG_RUNTIME_DIR` (socket location), `TERM` (capability detection), and
`CLOO_SOCKET` / `CLOO_CONFIG` for isolating a dev instance from a live one. The ignored
repository-root `.env` is a maintainer-only `NPM_TOKEN` for an explicitly authorized npm release;
cloo never reads it.

See [`docs/ENV_VARS.md`](docs/ENV_VARS.md) for the canonical matrix.

---

## Testing

Before marking any task done:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

`cloo-proto` has wire round-trip, framing, and handshake coverage as of M0-02. `cloo-core` has
table-driven layout tree coverage as of M0-03, plus the emulator-to-wire cell conversion as of
M0-07. `cloo-term` has grid coverage — SGR, alternate screen, cursor, resize, scrollback — as of
M0-04. `cloo-server` has PTY integration coverage in `tests/pty.rs` against a scripted `sh -c`
child as of M0-05, plus socket lifecycle coverage in `tests/socket.rs` as of M1-01, handshake
coverage in `src/conn.rs` as of M1-02, and split/close coverage in `tests/session.rs` as of M2-01,
extended at M2-02 with directional focus and a zoom cycle proved not to restart a child. `cloo-client` has byte-exact renderer coverage and raw-mode
restore coverage — normal, error, and panic paths, against a real pty in `tests/raw_mode.rs` — as
of M0-06, plus attach-handshake coverage in `src/attach.rs` as of M1-02 and `SIGWINCH` watch
coverage in `src/resize.rs` as of M1-03, and capability negotiation and fallback coverage in
`src/capabilities.rs` as of M1-06. `cloo-proto` gained framed-transport coverage in
`src/stream.rs` over a duplex pipe in M1-02. The binary has CLI and one-pane smoke coverage in
`crates/cloo/tests/cli.rs`, run over a pseudoterminal, as of M0-07 — including the `SIGWINCH`
chain end to end as of M1-03 — and end-to-end attach/detach coverage in
`crates/cloo/tests/attach.rs` as of M1-02, extended at M1-03 with a resize asserted on *both*
halves: the grid reflow and the child's own `stty size`, and at M1-07 with input routing end to
end — a paste bracketed exactly when the child asked, focus and SGR mouse reports reaching a child
that enabled them and neither reaching one that did not. `cloo-client` gained input decoding and
mouse-ownership coverage in `src/input.rs` at M1-07, `cloo-server` the matching encoders in
`src/session.rs`, and `cloo-term` one fixture per negotiated pane mode in `src/emulator.rs`. M1-09 adds default-deny outer-terminal effect policy coverage in `cloo-client/src/effects.rs` and an end-to-end typed OSC 52 effect fixture in `crates/cloo/tests/attach.rs`. That file lives in the binary crate
because it needs both halves of the wire and `cloo-server` may never name `cloo-client`. Keymap
resolution and config parsing are the next things that must get coverage as they land. M1-04 adds
row-damage tracker coverage in `cloo-server/src/damage.rs`, byte-exact incremental renderer
coverage in `cloo-client/src/renderer.rs`, and attach integration coverage that proves bounded
burst frames, lagged-client recovery, and concurrent-client fan-out. M2-03 adds pane-chrome
coverage in `cloo-client/src/chrome.rs` — focus and attention as independent signals, the fixed
width-degradation ladder at every width, and dimming with its no-dim fallback — plus positioned
chrome spans in `cloo-client/src/renderer.rs`. M2-04 adds the profile and pane-metadata models in
`cloo-core/src/profile.rs` and `cloo-core/src/pane.rs` — the built-ins proved to be data, pure
validation of names, labels, and an absolute-only cwd, and attention as state plus provenance with
its coalescing rule. M2-05 adds profile-configuration parsing coverage in `cloo-core/src/config.rs`
— a document error keeping the defaults against one bad profile dropped alone, the merge and
override rules, and the command and `min_size` surface. M2-06 adds launch coverage in
`cloo-server/src/launch.rs` and command-line coverage in `crates/cloo/src/cli.rs`, plus profile
launches through the session actor in `cloo-server/tests/session.rs` — metadata in every snapshot,
the child's own `pwd` proving the working directory, and a missing program failing with a message
that names it while the layout rolls back — and pane identity reaching a client in
`crates/cloo/tests/attach.rs`. M2-07 persists attention in the session actor (handshake v5): the
wire projection and its `ServerMessage::Attention` round-trip in `cloo-proto`, the state/source/
acknowledgment projection in `cloo-core/src/pane.rs`, attention resent only on change in
`cloo-server/src/damage.rs`, and attention through the actor in `cloo-server/tests/session.rs` —
a report reaching the next snapshot with its provenance, acknowledgment moving only the seen flag,
the coalescing rule proved through the channel, and a report for a closed pane dropped. M2-08 wires
the generic sources into that path: a coalesced bell flag in `cloo-term/src/emulator.rs`, a
non-blocking reap in `cloo-server/src/pty.rs`, and their mapping in `cloo-server/src/session.rs`
(bell → `needs_input`/`Bell`, exit → `ready`/`failed`/`Lifecycle`), proved against real children in
`cloo-server/tests/session.rs` including bait text that leaves attention `unknown`. M2-10 renders
the attention surfaces client-side in `cloo-client/src/chrome.rs`: the `AttentionQueue`'s
deterministic order and coalescing (an acknowledged state not refilling it, a lull resetting the
slate), keyboard navigation with focus and acknowledge, the per-state status-bar summary, every
state rendered text-glyph-and-colour in a queue row over the header's width ladder, and a bounded,
per-pane-coalescing `ToastDeck` — with the queue's key bindings in `cloo-client/src/input.rs`.
M4-01 adds server configuration-reload coverage in `cloo-server/src/config.rs` and
`tests/config.rs`: pure `CLOO_CONFIG`/XDG/home path precedence, valid replacement without a
restart, invalid replacement retaining the previous value, missing-file reset to built-ins, and
per-profile warnings that do not suppress valid neighbours — including a real `SIGHUP` routed
through the same reload path. The binary's `cli.rs` test also uses an isolated child environment
to prove a configured profile resolves before the local terminal is touched.
M4-03 adds pure theme coverage in `cloo-core/src/theme.rs` and `cloo-client/src/theme.rs`: every
named palette supplies every style-guide token, Storm is pinned to the handoff values, and a
terminal-palette theme emits basic ANSI semantic colours rather than RGB or 256-colour guesses.
The chrome fixture proves focus's `>` and attention's `!` remain textually and colour-wise distinct
without truecolor.
M5-02 adds the client half of copy mode in `cloo-client/src/copy_mode.rs` — selection, match, and
cursor spans painted from the grid cache with that cache asserted unchanged, role precedence with
each role distinct by attribute as well as colour, off-viewport positions dropped rather than
clamped, the status row exactly its width at every width, and a denied clipboard policy that
writes nothing and does not even send the request — plus the explicit copy through the actor in
`cloo-server/tests/session.rs` and the whole loop end to end in `crates/cloo/tests/attach.rs`.
M2-09 covers the opt-in adapter control interface (handshake v8): the permitted-state mapping and
message round trips in `cloo-proto/src/adapter.rs`, the pane's opt-in in `cloo-core/src/pane.rs`,
the derived control-socket path in `cloo-server/src/socket.rs`, the control handshake in
`cloo-server/src/conn.rs`, and the gate in `cloo-server/tests/session.rs` — an attributed report,
an adapter the profile never named refused with an observed `failed` left intact, a pane that opted
into nothing reachable by none, and a closed pane refused rather than dropped — with the whole loop
end to end in `crates/cloo/tests/attach.rs`.
M6-01 covers mouse ownership at both ends: `cloo-client/src/input.rs` hit-tests a drawn screen at
every region, proves a mis-described pane cannot swallow a chrome row and a header cannot swallow a
pane's cell, and asserts that *no* chrome region produces a wire event even under full motion
tracking; `cloo-server/tests/session.rs` proves against real children that an event reaches the pane
it names and not the focused one, that a pane which never enabled the mouse is written nothing, and
that an event naming a closed pane is dropped.
M6-03 adds the stall coverage those fixtures needed in `cloo-server/tests/session.rs`: a snapshot
answered after every pane's child has exited with nobody draining the event channel, and sixty-four
undeliverable notifications followed by a resize that still applies. Every snapshot in that file now
goes through a `snapshot_now` helper that wraps the call in a deadline — a test must never await an
actor reply without a timeout, or a wedged actor hangs the whole suite instead of failing one test.
M4-02 adds keymap resolution: chord spellings and the tmux-shaped `C-b` table in
`cloo-core/src/keymap.rs` (spellings round-tripping, each invalid one refused by its own error, the
action vocabulary with no name for an action that needs typed text, and an override replacing a
binding in place while an alias is not a conflict), the `[keys]` document surface in
`cloo-core/src/config.rs` (a chord written twice as a document error, an unspellable prefix keeping
`C-b`, and a bad line dropped alone), and the prefix state machine in `cloo-client/src/input.rs` —
one fixture per encoding a terminal sends decoded to its spelling, and every default-bound chord
still reaching the pane byte for byte when no prefix is pending.
M4-04 adds the motion model in `cloo-client/src/motion.rs` — a 120ms transition stepped frame by
frame from an injected `Instant`: an interruption settling at the end state rather than rewinding,
a bounded frame count however often the transition is sampled, reduce-motion drawing exactly one
settled frame, and a contrast ramp that keeps every character readable — plus the transition frame
in `cloo-client/src/renderer.rs`, whose settled phase is byte-identical to an ordinary span frame.
M6-02 covers the chrome's own mouse actions (handshake v9) at three layers: `cloo-core/src/layout.rs`
proves a drag changes ratios only by comparing the tree's *shape* with every ratio erased as well as
the rectangles, with the clamp tested from both ends and an undividable extent leaving the ratio
alone; `cloo-client/src/input.rs` finds a divider from the pane rectangles for both a gutter column
and a header row, emits relative deltas with nothing on the press or after the release, and maps the
wheel onto the copy-mode commands the keyboard already sends; `cloo-core/src/keymap.rs` asserts
`FocusPane` and `ResizePane` have no spelling while the four directional focus actions stay bindable;
and `cloo-server/tests/session.rs` proves against real children that a drag moves one divider without
restarting anything and that a stale or zoom-hidden click is dropped — with the wheel end to end in
`crates/cloo/tests/attach.rs`.
M3-04 adds the keyboard-first overlays in `cloo-client/src/overlay.rs` — every overlay dismissible
from every state including an empty one, navigation clamping at both ends, a confirmed launcher row
naming a profile the caller supplied with an unvalidatable profile never becoming a row, pane
details listing only what the server reported, and the shared width ladder asserted exactly at
every width and height — with the matching key bindings in `cloo-client/src/input.rs`.

Full test strategy, inventory, and patterns: [`docs/TESTING.md`](docs/TESTING.md)

---

## Deployment

cloo ships as a locally installed binary through two channels:

- **crates.io** — `cloo`, built from source via `cargo install cloo`.
- **npm** — `clooterminal`, prebuilt per-platform binaries as optional deps (esbuild/swc
  pattern). Not yet wired up; the published package is a name reservation with no `bin` entry.

**Agents must never publish to either registry.** Both are irreversible and public: npm allows
unpublishing only within 72 hours and burns the name afterward, and crates.io versions cannot
be deleted at all. Publishing is the project owner's action.

---

## Living Document

This file is a running notebook of agent discoveries. After each task cycle, update
this file if you found:

- A constraint that would have saved time if it were written here.
- A debugging tip that resolves a non-obvious failure.
- A pattern that should be followed for consistency.
- A "never do X" rule that emerged from a near-miss.

Append under `## Discoveries` below. Keep each entry to 2–3 sentences with a date.
Do not reorganize or rewrite existing entries — append only.

```
### YYYY-MM-DD — <short title>
<What you found and why future agents working here should know it.>
```

---

## Discoveries

### 2026-07-22 — A session is never empty and its active tab always exists
`cloo-core::session::Session` mirrors the layout's "at least one pane" rule one level up: it is born
with one tab, refuses to close the last (`SessionError::LastTab`), and keeps `active` on a tab that
still exists so `active_tab()` returns a reference, not an `Option`. Closing the active tab activates
the tab that slid into its index (right neighbour, or the new rightmost when it was last); `close_tab`
checks unknown-tab *before* last-tab so a bad ID is never reported as the last-tab rule. `TabName`
reuses `pane::validate_text` (now `pub(crate)`) — one validator, so a tab title cannot smuggle a
control char a pane name could not.

### 2026-07-22 — Attention is a third wire clock, projected from the same layout pass
Attention crosses as its own `ServerMessage::Attention(Vec<PaneAttention>)` rather than being
flattened into `PaneInfo`, because a state without its source is exactly the claim the chrome must
not make, and it is resent only when it changes — a rename is not a state change and vice versa, so
`DamageTracker` diffs `metas` and `attention` independently. Project both from the *same*
`Layout::resolve` pass in `Session::snapshot`, or a client can be told a pane's state without being
told who the pane is. The session actor is the one writer, so `set_attention` leans on
`Attention::set`'s coalescing instead of re-implementing it, and a report for a closed pane is a
silent no-op like a stale mouse event.

### 2026-07-20 — npm rejects `cloo` at publish time, not lookup time
`npm view cloo` returned 404 and `npm publish --dry-run` passed, but the real publish failed
with a 403 from npm's package-name similarity filter (too close to `clone`, `cli`, `clsx`,
`clui`, and others). The name is now `clooterminal`, with `cloo` preserved as the command via
the `bin` field. Registry availability is not proof a name is publishable.

### 2026-07-20 — Intra-workspace deps carry both a path and a version
The five library crates are declared once in the root `[workspace.dependencies]` with
`{ path = "…", version = "0.0.1" }` and pulled in as `cloo-core.workspace = true`. A path-only
dependency builds locally but makes the crate unpublishable to crates.io, so the version is not
optional even though nothing is published yet.

### 2026-07-20 — Postcard needs an explicit length prefix on a stream
Postcard is not self-delimiting, so a socket reader cannot know where one message ends. Framing
is a big-endian `u32` prefix, and `decode` returns bytes-consumed so a caller can drain and
re-read. A partial buffer must surface as `ProtoError::Incomplete` (read more, retry), which is
distinct from a malformed payload — conflating the two turns a normal short read into an error.

### 2026-07-20 — One ratio-to-cells function, shared by resolve and the min-size check
`cloo-core::layout::split_extent` is the only place a ratio becomes cell counts. The
minimum-size check calls it rather than reimplementing the arithmetic, because if rounding
disagrees between the check and the layout pass you can accept a split and then resolve it below
the minimum. Rejection happens at split time only — a layout pass over an area that shrank
squeezes panes to a one-cell floor instead, since a resize must always produce a drawable answer.

### 2026-07-20 — `cloo-term` duplicates the proto cell types on purpose
`Cell`, `Color`, and `CellAttrs` exist in both `cloo-term` and `cloo-proto` with identical
`CellAttrs` bit positions, because `cloo-term` has no intra-workspace dependencies and reusing
the wire types would put `cloo-proto` under the emulation wrapper. `cloo-core` owns the
conversion, and it is only a field copy as long as the bit layouts stay in sync — change one and
you must change the other in the same commit.

### 2026-07-20 — Grid line indices are absolute, not viewport-relative
`alacritty_terminal`'s `Grid[Line(n)]` indexes the buffer, not the visible rows: viewport row `r`
is `Line(r - display_offset)`, and the cursor's viewport row is `point.line.0 + display_offset`.
Getting this backwards renders the wrong rows only once scrollback is non-empty, so it survives
every test that does not scroll. Also, `\x1b[?1049h` saves the cursor rather than homing it — a
fixture that writes immediately after entering the alternate screen lands at the old column.

### 2026-07-20 — A PTY master reports EOF as `EIO`, not as a zero-length read
On Linux, reading a PTY master after the last slave descriptor closes fails with `EIO` rather
than returning `0`. `cloo-server::pty` translates that into an ordinary EOF at the boundary, so
nothing above the PTY layer has to know. If you ever see a pane "erroring" the instant its shell
exits, this is the translation being missed. The parent must also drop its own copy of the slave
right after spawning, or that EOF never arrives at all.

### 2026-07-20 — PTY restoration is by ownership, not by a shutdown call
The master is an `OwnedFd` and `Pty::drop` kills and reaps the child, because `std::process::Child`
leaves a zombie on drop by default. The master is also set close-on-exec: a child inheriting a
writable master keeps the descriptor alive after the parent closes its copy, and reads on the
parent side then never see EOF. `tests/pty.rs` asserts the reap with `kill(pid, 0)`, which still
succeeds for a zombie and so actually catches the bug.

### 2026-07-20 — A signal handler cannot borrow the raw-mode guard
Restoring the terminal on `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` means the saved `termios` has to
live in a process-global slot, not only in the RAII guard, and the handler may call only
async-signal-safe functions — `tcsetattr` qualifies, allocating or locking does not. The slot is a
three-state atomic so a handler firing mid-arm reads nothing, and only one guard may be armed per
process, which is why `cloo-client`'s pty-backed tests take a module `Mutex` before entering.

### 2026-07-20 — Render frames are asserted byte for byte, so keep them deterministic
`Renderer::render_full` returns an owned buffer instead of writing to a descriptor, which is the
only reason a fake grid is testable against an exact expected string. Two rules keep it that way:
every SGR sequence leads with a `0` reset (absolute, never a delta from the previous frame), and
no sequence is emitted for a capability the client did not report — RGB downsamples to the
256-palette rather than being sent and hoped for.

### 2026-07-20 — Enter raw mode before spawning the child, and never before checking stdin
`RawMode::stdin()` is what produces "cloo must be run from a terminal", so it has to run before
anything else that can fail on a non-tty — an earlier `TIOCGWINSZ` on stdout reports "inappropriate
ioctl" instead, which is a true but useless message. It also has to run before the PTY is spawned,
so a refusal leaves no child to clean up. A `winsize` that cannot be read is *not* a reason to
refuse: stdout is asked, then stdin, then a conventional 80x24.

### 2026-07-20 — Draw a final frame after `Pump::Eof`, not only on the frame tick
The render loop paints on a ~60fps timer, so a child that writes and exits within one tick has its
last output sitting in the grid, never drawn — `printf hello; exit` shows nothing at all. The loop
therefore renders once more after EOF if the grid is still dirty. Any future coalescing scheme
needs the same flush, and `crates/cloo/tests/cli.rs` is what catches its absence.

### 2026-07-20 — The crate graph is a layering, not a chain
Four M0 tasks each added an edge that "skipped a level" in the old `cloo → {server, client} →
core → {proto, term}` diagram, and each documented it as an exception. They were not exceptions:
`cloo-proto` is the wire vocabulary, so every crate that speaks the wire names it directly. The
rule is now stated as a layering with a real edge table in `docs/ARCHITECTURE.md` — depend
downward freely, never sideways between `cloo-server` and `cloo-client`, never upward.

### 2026-07-20 — Test the signal restore path from the binary, not the library
`cloo-client`'s own tests cannot assert the `SIGTERM` restore path, because a library test that
signals itself kills the test runner. Signal the *binary* as a child instead: `crates/cloo/tests/
cli.rs` spawns `cloo` on a pseudoterminal and asserts the terminal came back cooked. When adding
an exit path, check the assertion is not vacuous by breaking the restore and watching it fail.

### 2026-07-21 — A socket file is not proof a daemon is alive
Daemon ownership is an advisory `flock` on `<socket>.lock`, never the existence of the socket:
a `SIGKILL`ed daemon leaves a socket behind and a live one has the same file, so the file cannot
distinguish them, while the kernel drops a `flock` however the holder dies. Holding the lock is
also what makes stale cleanup safe — the unlink is only reachable once no other daemon can exist.

### 2026-07-21 — Stat a socket path with `symlink_metadata` before unlinking it
`fs::metadata` follows symlinks, so a symlink at the socket path reports the *target's* file type
and a "it's a socket, remove it" check passes on something that lives elsewhere. `cloo-server::
socket` uses `symlink_metadata` and refuses anything that is not itself a socket; `Drop` also
compares the `(device, inode)` recorded at bind, so a departing daemon cannot unlink a successor
that already claimed the path. Both cases have tests, and both pass vacuously if you use the
wrong stat call.

### 2026-07-21 — The server/client edge ban covers dev-dependencies
An end-to-end attach test naturally wants `cloo-client` in `cloo-server`'s `[dev-dependencies]`,
and that is the forbidden sideways edge just as much as a real dependency is — Cargo builds it,
and the graph now has the cycle the layering exists to prevent. `crates/cloo/tests/attach.rs` is
where a test that needs both halves goes, because the composition root already depends on both.

### 2026-07-21 — A clean close and a truncated frame are different answers
`FrameStream::recv` returns `Ok(None)` when the peer closes *between* frames and
`StreamError::Truncated` when it closes *inside* one. Collapsing the two either turns an ordinary
detach into an error the client reports, or turns a half-written frame into a silent hang-up that
looks like a normal disconnect. The read buffer's emptiness at EOF is the whole test.

### 2026-07-21 — A daemon must pump the PTY while nobody is attached
The property "detach leaves the child running" is not only about not killing the child: a daemon
that reads the PTY only while a client is connected loses every byte written in between, and a
reattaching client finds a stale grid. `Daemon::wait_for_client` therefore selects over `accept`
*and* `pump`, and the reattach test asserts on text the child wrote before anyone connected.

### 2026-07-21 — A resize test that checks one half passes with the other missing
A resize is two operations — the grid reflows and the child hears about it through `TIOCSWINSZ` —
so asserting only on the wire's row width, or only on the child's `stty size`, leaves half the
feature untested. `crates/cloo/tests/attach.rs` asserts both from one client, and each half was
confirmed non-vacuous by breaking the corresponding line of `PtyReactor::resize` and watching it
fail. Do the same to any resize assertion you add.

### 2026-07-21 — Signal and input race in a pty test unless the assertion is order-free
A test that sends `SIGWINCH` and then a keystroke, expecting the child to report the *new* size,
depends on cloo's `select!` picking the resize branch first — it passes alone and fails under a
loaded parallel run. Have the child report on a loop (`while :; do stty size; sleep 0.1; done`)
so no ordering matters. Relatedly, `read_until` in `crates/cloo/tests/cli.rs` now polls with the
time remaining: a blocking read on a pty that goes quiet ignores the deadline entirely and turns
a clean 20-second failure into a hung suite.

### 2026-07-21 — An actor handle must be the only way in, including for reads
`Daemon` used to hold the `PtyReactor` and call `snapshot()` on it directly; the session task
would have been a second path to the same state rather than the only one. It now holds a
`SessionHandle` — a sender and nothing else — and asks for snapshots over the channel like
everything else, which is what makes "no `Mutex` on session state" mean something. `SessionEvent`
splits by kind for the same reason: `Output` is a level and coalesces on a depth-one channel,
while `Exited` carries information a reader cannot recover and must be sent, not dropped.

### 2026-07-21 — Refusing on `TERM` belongs in the client, not on the wire
The server cannot enforce "an unresolvable `TERM` may not attach" by looking at the reported
`TermCaps`: an all-false set is exactly what a capable terminal reporting nothing would also send,
so inferring the refusal there would turn a legitimate attach away. `TERM` is the client's to read,
so `cloo-client::capabilities::attach_caps` refuses before the socket is touched, and
`caps_from_env` is the same detection with the refusal replaced by an all-false default for the
local pane — one function, two policies, which is what keeps the two paths from drifting.

### 2026-07-21 — Ask the layout first, then the PTY, and roll back with `close`
Split and close are atomic because of their order, not because of a transaction: the layout is the
half that can refuse, so it goes first and a refusal never costs a process, and only then is the
child spawned or its reactor dropped. A spawn that fails is undone by closing the pane just added,
which works only because collapsing a fresh split restores the previous tree *exactly* — that
exactness is now a test in `cloo-core::layout`, since the server cannot reach the failure path.

### 2026-07-21 — A child that reports on a loop makes a resize assertion vacuous
`while :; do stty size; done` leaves its old answer on the grid, so an assertion on it passes
whether or not the resize under test happened; `while read _; do stty size; done` reports only when
the test asks, and checking the *last* non-blank line is what pins it to the most recent answer.
This is the opposite of the M1-03 lesson about ordering — use a loop when a signal races input, an
on-demand reporter when a stale identical answer would pass.

### 2026-07-21 — Selecting over N PTYs needs no dependency
The session pumps a runtime-sized set of panes with a hand-rolled `select_all`: box each
`PtyReactor::pump`, poll them in a `poll_fn`, and rotate the starting index so a loud pane cannot
starve a quiet one. It is safe only because `pump` is cancel-safe — every future that loses is
dropped on each call. Box the futures as `dyn Future + Send`, or the whole session task stops being
`Send` and `tokio::spawn` rejects it with an error that points at the wrong line.

### 2026-07-20 — DESIGN.md was migrated into docs/
The root `DESIGN.md` was the original planning document and has been folded into
`docs/PRD.md` (scope, milestones), `docs/ARCHITECTURE.md` (topology, protocol, layout), and
`docs/DECISIONS.md` (the resolved/open decision log). It no longer exists — do not recreate a
root-level design doc, since `docs/INDEX.md` forbids root stubs that redirect into `docs/`.

### 2026-07-21 — Encoding input is a function of the child's modes, not the terminal's
Whether a paste is bracketed, whether a click is reported, and in which encoding are all set by
private mode sequences the *child* wrote, so only the emulator can answer and only the server sees
it. That is why `ClientMessage::Paste`/`Focus`/`Mouse` carry events rather than bytes, and why the
server reports `PaneModes` back — the client needs them to decide whether a click is the
application's or cloo's chrome's, and it cannot observe them itself.

### 2026-07-21 — A pasted terminator must be stripped, or the bracket is decorative
`\x1b[201~` inside pasted text closes the bracket early and the remainder of the paste is
interpreted as typed input — exactly the injection bracketed paste exists to prevent. `paste_bytes`
strips both delimiters from the body and normalises line endings to `\r`, because a pasted `\n`
reaches a shell as a literal newline rather than as Enter.

### 2026-07-21 — A lone Escape is a prefix of every sequence the decoder knows
Holding a partial escape sequence across reads is what makes a split paste or mouse report decode
correctly, but it also means pressing Escape is held forever waiting for bytes that never come.
`InputDecoder::flush`, called on the frame tick, is the whole answer — and it deliberately refuses
to flush mid-paste, since turning half a paste into keystrokes is the failure being prevented.

### 2026-07-21 — A scripted child needs `-icanon` to receive a report with no newline
`\x1b[I` and an SGR mouse report carry no newline, and a pty in canonical mode delivers nothing to
the reader until one arrives — an integration test asserting on them hangs to its timeout rather
than failing. `crates/cloo/tests/attach.rs` runs those children under `stty -echo -icanon` and
strips the escape byte with `tr`, which is what makes an escape sequence assertable as grid text.

### 2026-07-21 — The Kitty keyboard protocol is off by default in the emulation backend
`alacritty_terminal`'s `Config::kitty_keyboard` defaults to false, and with it off a child's
`\x1b[>1u` push is silently discarded — so `Emulator::modes` would report legacy keys forever,
which is a wrong answer rather than a missing one. cloo turns it on. Related and still open: the
emulator runs with a `VoidListener`, so any reply it wants to write back to the child (device
attributes, a keyboard-mode report) is dropped — see DECISIONS.md OPEN-02.

### 2026-07-21 — Subscribe after a resync snapshot, not before it
The daemon is the only broadcaster, so it can capture a snapshot and create a new `broadcast`
receiver without an await between them; the receiver then starts strictly after the snapshot. A
receiver created first could replay an older layout or row after the snapshot and undo the resync.

### 2026-07-21 — Empty OSC titles are reset effects in the wrapper
`alacritty_terminal` reports an empty OSC 0/1/2 title as `Event::Title("")`, not `ResetTitle`
(that event is used for configuration reload). `cloo-term` normalizes it to `ResetTitle`; its
effect listener also drops backend reply events, so no PTY reply or arbitrary control string can
be mistaken for an outer-terminal effect.

### 2026-07-21 — Zoom is a flag, and that is what makes unzoom exact
Modelling zoom as a reshaping of the tree — promote the pane, remember the old tree, restore it —
gets the ratios back only if the copy is perfect. Storing a `zoomed: Option<PaneId>` that only
`Layout::resolve` reads makes "unzoom preserves every ratio" true by construction rather than by
test, and it is also why zoom cannot restart a PTY: the only thing it can do to a child is a
`TIOCSWINSZ` from the ordinary geometry pass, and a hidden pane's child is not even sent that.

### 2026-07-21 — Directional focus must be geometric, not structural
Walking the tree for "the pane to the left" answers with a *subtree* as often as a pane, and picks
the wrong leaf whenever the sibling is itself a split. `Layout::neighbor` reads one layout pass
instead and requires the candidate to overlap on the perpendicular axis, which is what stops focus
from jumping diagonally. The test that catches a structural implementation is the asymmetric tree —
a quad passes either way.

### 2026-07-21 — Prove "no restart" with a pid the child prints once
Nothing above the PTY layer exposes a per-pane child id, so a test that a zoom did not respawn
anything has no direct handle to assert on. A child that runs `echo pid=$$` once at startup and
reports on demand afterwards leaves that line on its grid: comparing it before and after the zoom
cycle fails the moment a pane is torn down and spawned again, since both the pid and the cleared
grid would change.

### 2026-07-21 — The terminal effect queue must stay `Send` and suppressible
The session's rotating PTY pump boxes `Send` futures, so an emulator listener cannot use
`Rc<RefCell>` even though the session actor is the sole logical owner. `cloo-term` uses a bounded
non-blocking channel instead; a full queue drops a typed client-local effect, which is safe because
effects never change the grid or authoritative session state.

### 2026-07-21 — Dimming a palette colour is a guess; dimming an RGB one is arithmetic
"Contrast reduction toward the frame background, not alpha" is implementable exactly only for a
`Color::Rgb`; for a `Color::Indexed` or the terminal default, cloo does not know what the user's
palette looks like, so `chrome::dim_cell` falls back to the `DIM` attribute rather than inventing a
colour. Blending is what keeps a dimmed amber `needs input` distinguishable from a dimmed grey
`quiet` — the test that catches a lazy "just set DIM everywhere" implementation.

### 2026-07-21 — Prove "the built-ins are data" by reconstructing one
"No vendor special case" is a property no ordinary test asserts, because a profile with a hidden
branch still validates and still launches. `profile.rs` instead rebuilds `codex` from the public
constructor and compares it field for field to `Profile::codex()` — that fails the moment a
built-in gains anything a user's configured profile could not also express, which is the actual
rule. `min_size` is validated against `MIN_PANE_SIZE` for the same reason: a recommendation a split
could never honor would silently mean nothing.

### 2026-07-21 — Coalesce attention in the model, not in each source
`Attention::set` clears acknowledgment only when the state actually *changes*, so a harness
re-announcing `needs_input` every second cannot refill a queue the user just cleared. Putting that
rule in one place is what stops the bell path, the lifecycle path, and the adapter path from each
inventing their own; provenance is kept beside the state rather than folded into it, so an
adapter's advisory claim can be attributed instead of presented as fact.

### 2026-07-21 — A pure validator is proved by the path that does not exist
`cloo-core` performs no I/O, so `WorkingDir::new` checks a path's *shape* and nothing else — the
test that keeps it honest validates `/definitely/not/here/at/all` successfully. Existence and
`PATH` resolution are launch-time answers the server owns, and a directory that exists at
validation time may be gone by launch anyway. Same reasoning bars `~`, which is the shell's and
unexpanded means a directory literally named `~`.

### 2026-07-21 — A config parser takes text, and syntax and semantics fail differently
`cloo-core::config::parse` takes the *contents* of `config.toml` and never a path, which is the
only way a config loader can live in a crate that performs no I/O — the server reads the file at
M4-01. Inside it, a document error (malformed TOML or an unknown key, which `deny_unknown_fields`
turns into one) rejects everything and the caller keeps the defaults, while a single profile that
fails `Profile::validate` is dropped alone with a warning; collapsing the two either loses nine
good profiles to one typo or silently ignores a key the user believes is applied.

### 2026-07-21 — Fix the header's degradation order, or two panes disagree on one screen
A header that decides per situation which field to drop renders differently in two equally narrow
panes, and cannot be asserted against an exact string. `chrome::header_cells` spends width in one
fixed order (task label, then state text, then title truncation, glyph last) and is tested for
being *exactly* the pane width at every width from 0 to 60 — that loop, not the pretty cases, is
what catches an off-by-one in the gap arithmetic.

### 2026-07-21 — A pane is created only from a Launch, and that is what makes "no inference" a type
`cloo-server::launch::Launch` is the sole way `Session::spawn`/`split` makes a pane: a validated
profile plus the user's name, task, and cwd, built before any process exists so a bad profile
never costs a child and a missing program is the only thing left to fail at `execvp`. "cloo does
not guess a task" is enforced by the type having no constructor that takes a grid or a process
name — not by a rule someone remembers. Split the `PtyConfig` in two: `PtyConfig::session` carries
the environment and geometry, `Launch::configure` overwrites the argv and cwd, which is why a
split can launch a different profile without losing the session's `TERM`.

### 2026-07-21 — Identity is a wire message on its own clock, separate from geometry
`ServerMessage::Panes(Vec<PaneInfo>)` (handshake v4) carries profile/name/task/cwd, and the
`DamageTracker` resends it only when the metas change — a resize is not a rename, so a full-screen
drag must not drag every pane's name across the wire. It is sent whole, not per pane, so a client
replaces its map and never holds an entry for a pane that closed. `SessionSnapshot::metas` is
projected from the same `Layout::resolve` pass as the rects, so a client can never be told about a
pane it has no identity for, or vice versa.

### 2026-07-22 — A PTY EOF races the child becoming reapable
End of file on a PTY master (the translated `EIO`) fires when the kernel closes the exiting child's
descriptors, which happens a hair *before* the process becomes a zombie — so a single
`try_wait` at EOF can return `None` and misreport a crash as a clean exit. `Session::exit_status`
closes the window with a short bounded spin of `try_wait`, never a blocking `wait`, because a child
that closed its terminal but kept running (a detach) would wedge the actor forever. The reaped
status is cached in `Pty` so the shutdown `wait` returns it instead of `waitpid`-ing the same pid
twice and failing with `ECHILD`.

### 2026-07-22 — Tab switches are projections, not PTY ownership changes
The session actor holds every pane reactor while the pure session model's tab-local layouts partition
them; a snapshot projects only the active tab, but inactive PTYs must keep pumping. Tab creation starts
its child before adding the tab to the model, and closing removes the model entry before dropping exactly
its panes — a switch then applies geometry to the selected tab without restarting either child.

### 2026-07-22 — Reload configuration by replacement, not mutation
`cloo-server::config::ConfigManager` reads and validates a whole `config.toml` before assigning it
to the live value, so a malformed `SIGHUP` reload has no partial state to undo and leaves the last
valid configuration intact. The parser stays pure in `cloo-core`; path resolution and file I/O live
in the server, and a missing file is a deliberate reset to the built-ins.

### 2026-07-22 — Copy-mode history reads must not move the viewport
Copy selection and regex search need the whole retained grid, but scrolling an emulator to collect it
would move the view a client is drawing. `Emulator::scrollback_text` reads absolute grid lines without
changing `display_offset`; the session actor owns both that viewport and the copy cursor, and projects
copy state on its own wire clock so reattachment cannot turn a client cache into an authority.

### 2026-07-22 — Never snapshot scrollback for an inactive copy search
`scrollback_text` allocates every retained line, so calling it after every `Pump::Bytes` makes a burst
pay for thousands of history snapshots even when no client entered copy mode. Check for an active
search first; `crates/cloo/tests/attach.rs`'s burst fixture catches this regression by timing out
before its final marker otherwise.

### 2026-07-22 — A client cannot place a scrollback position without being told the viewport
Copy-mode positions are absolute in server-owned history while a client caches only the visible
grid, so `CopyModeState` carries `viewport_top` (handshake v7) — the retained line drawn on the
pane's first row — and every highlight is computed in retained-line coordinates rather than by
guessing an offset. Never clamp an off-screen position onto the nearest visible row: that
highlights text the user did not select, and the test that catches it selects across a line that
has already scrolled out.

### 2026-07-22 — An explicit copy is answered to one client, not broadcast
`Action::CopySelection` is handled in the *socket* task rather than the daemon coordinator, so the
resulting `ClipboardStore` reaches only the terminal whose user pressed the key — fanning it out
through the damage broadcast would store one user's selection in every attached clipboard. The
client-side gate runs before the request too: `EffectPolicy::permits_clipboard` decides whether to
ask at all, so a client that would refuse the store never makes the server put scrollback on the
wire.

### 2026-07-22 — A mouse report is hit-tested against what was drawn, not against the wire
`cloo-client::input::ScreenLayout` is the client's own description of its screen — chrome rows, pane
grid rectangles in terminal cells, and which pane is focused — because the server's `PaneRect` is a
pane's *grid* in tab coordinates and knows nothing about the header, gutter, or status bar the
client drew around it. `hit` claims the chrome rows before consulting any pane, so a wrongly
described pane cannot swallow a status-bar click, and `MouseRoute::Chrome` carries no `MouseEvent`
at all, which is what makes "a chrome event never reaches the wire" a type rather than a rule.

### 2026-07-22 — Modes are reported for the focused pane, so every other pane is chrome's
`ServerMessage::Modes` names one pane and the `DamageTracker` sends it for the focused one only, so
a client genuinely cannot know whether an unfocused pane's application tracks the mouse. Answering
"chrome" there is the honest answer *and* the one a user means by clicking an unfocused pane; the
server closes the loop by encoding a `Command::Mouse` from the **named** pane's own modes and
refusing a pane that is not visible, so a client cannot write into an arbitrary child.

### 2026-07-22 — An advisory source is narrowed by its vocabulary, not by a check
The adapter control interface is a second socket (`<session socket>.control`) speaking
`cloo-proto::adapter`, so "an adapter cannot type into a pane" and "an adapter cannot claim `quiet`"
are both facts about which enums exist rather than refusals some branch must remember. The gate that
makes it *opt-in* is the profile's `adapter` field, copied onto `PaneMeta` at launch: the server
matches the announced name against the pane's own, stamps the provenance itself, and answers every
report — a silent drop is indistinguishable from success to the shell script an adapter usually is.

### 2026-07-23 — An actor that awaits its own event channel deadlocks against its reader
The session task used to `send(SessionEvent::Exited).await` when the last pane's child exited, and
with the depth-one channel already holding a coalesced `Output` that parked the actor — which meant
it stopped answering `Command::Snapshot`, and the only reader that could drain the channel was
whoever was awaiting that snapshot. Events now leave through an outbox the task owns and the loop
selects over a channel permit, so a slow reader costs latency and never liveness. The tell is that
`exit 0` never reproduced it while `printf 'bye\n'` always did: with no output there is nothing in
the channel to block behind, which is why every M2-08 lifecycle fixture passed for two milestones.

### 2026-07-23 — A test that awaits an actor reply without a timeout hangs the whole suite
Between M6-01 and M6-03 `cargo test --workspace` never returned, because `wait_for_text` checked its
deadline only *after* `handle.snapshot().await` came back and a wedged actor meant it never did.
Every snapshot in `cloo-server/tests/session.rs` now goes through `snapshot_now`, which wraps the
call in the same `DEADLINE` — a 20-second failure naming the stall instead of an unbounded hang.
Any future fixture that awaits an actor reply needs the same wrapper; the suite is ~3 seconds, so
anything that runs long is a stall, not slow work.

### 2026-07-23 — One overlay model, and two properties that are types
`cloo-client::overlay` is a single `Overlay` — a list, a cursor, a title — for the session
switcher, the profile launcher, and pane details, because the style guide gives all three one
language and three models would drift. Two rules are enforced by construction rather than by a
branch: `LaunchRequest` has no constructor but confirming a launcher row and a launcher row has
none but a validated `Profile`, so "explicit profiles only" cannot be violated by a caller; and
`Dismiss` answers `Dismissed` from every state including an empty list, so no overlay can trap the
terminal. The rows reuse the pane header's fixed yield order, and the hint row inverts it — the
dismissal hint is written first so it is the last one standing.

### 2026-07-23 — Half-reverting `deliver_mouse` proves less than it looks
Confirming the M6-01 mouse fixtures non-vacuous means reverting to the *fully* naive implementation:
write to the focused pane with no visibility check. Reverting only the pane lookup leaves the
`is_visible` guard in place, and `a_mouse_event_for_a_closed_pane_is_dropped` then passes against a
broken implementation, because the guard drops the event before the wrong lookup is ever reached.
When checking a fixture's honesty, break the specific line that fixture is about.

### 2026-07-23 — The keyboard's ownership rule is the mirror of the mouse's
`cloo-core::keymap` owns what a chord is *called* and `cloo-client::input` owns what bytes it
arrives as, because a spelling must not depend on a terminal and an encoding does. The router's one
property is that **nothing is consumed outside a pending prefix**: pass-through is a copy of the
slice that arrived rather than a re-encoding of a decoded chord, and a sequence `decode_key` cannot
name is the pane's, exactly as a mode that was never negotiated is. Confirm any fixture here by
reverting to a router that looks a chord up without the prefix — five tests fail, and a keymap that
ate `c` in vim would otherwise ship.

### 2026-07-23 — Interruptible motion means settling, not rewinding
`cloo-client::motion` ends an in-flight transition at its *end* state when input, a resize, or a
state change arrives, so the interrupting event's own frame is the one drawn and no half-finished
ramp is left on screen; a settled `Phase` returns each cell unchanged, which makes that frame
byte-identical to a client that animates nothing (and to reduce-motion). The frame cap is the other
half: a transition is seven whole 16ms steps and `Motion::tick` answers `None` on a step it already
drew, so even a caller sampling once per PTY read costs at most eight frames.

### 2026-07-23 — A vocabulary is how a binding is stopped from naming an impossible command
`Action::RenameTab` and `Action::CopySearch` carry text a keypress does not, so they have no
configuration spelling in *either* direction — `parse_action` refuses the name and `action_name`
answers `None`. That is the same shape as `LaunchRequest` having no constructor but a confirmed
launcher row: the impossible case is absent from the vocabulary rather than rejected by a branch
someone has to remember. Relatedly, `S-a` is refused because a terminal reports a shifted `a` as
`A`, so accepting it would store a binding that could never fire.

### 2026-07-23 — A gutter drag crosses the wire in cells, never as a ratio
"Ratios never cross the wire" and "a drag lands on a column" meet in `Action::ResizePane { pane,
dir, delta: i16 }`: the client sends the cells the pointer moved and `Layout::resize` turns them into
exactly one new ratio on that pane's nearest ancestor split. A ratio on the wire would also have made
`Action` un-`Eq`, which every existing round-trip test relies on — an `f32` field is a tell that the
arithmetic belongs on the other side of the socket. The delta is signed and the pane names the
divider's *leading* side, so the server works out whether it is the split's first or second child.

### 2026-07-23 — A chrome gesture is only allowed to spend commands that already exist
`ChromeAction::commands` is the whole mouse vocabulary, and it returns `Action`s the keyboard sends,
because a gesture reachable only with a mouse would be unreachable on a terminal whose `sgr_mouse`
fallback is "keyboard-driven chrome". That is why the wheel is `FocusPane` + `EnterCopyMode` + three
`CopyMotion`s rather than a scroll command of its own, and why `FocusPane`/`ResizePane` have no
keymap spelling — they name a pane, which a pointer supplies and a chord cannot. A press on a divider
begins a drag and commands *nothing*, which is what keeps a drag from also focusing.

### 2026-07-23 — Coalesced copy-mode frames leave an end-to-end test no baseline
`crates/cloo/tests/attach.rs`'s wheel fixture sends `EnterCopyMode` and then the motions in two
separate batches, because the damage tracker sends copy state only when it changed: a client that
sent the whole list at once sees one frame, already at the final cursor, and any assertion against
"where entering put it" passes vacuously. Split the batch when a fixture needs to measure a delta
through a coalescing channel.

### 2026-07-23 — npm publishing credentials are repository-local, not runtime configuration
The repository-root `.env` holds only the maintainer's `NPM_TOKEN` for an explicitly
user-authorized `clooterminal` release. It is gitignored and must never be printed, logged,
committed, or used for any command other than that one publish; cloo itself never reads it.

### 2026-07-24 — The reconnect/resize race is a survivor-redraw property, provable without a client render loop
There is no client-side render loop wiring wire messages into a `Grid` yet (M6-04+ composition is
still to come), so M7-01's reconnect/resize race is asserted at the wire in `crates/cloo/tests/
attach.rs` using the raw `Attached` transport. The corruption to rule out is a geometry
disagreement: a departing narrower client must resize the *survivor* back up, and because a pane
whose size changed makes `DamageTracker` resend every row, a full-width row is never applied to a
stale narrow cache. Assert it by waiting for a `Damage` row of the exact expected width (40 then
80). Keep the scripted child a plain `read _; exit 0` — the grid reflow alone carries the width, so
the child never needs to print, but it must still exit or the daemon join hangs.

### 2026-07-24 — A dropped session handle makes the actor block the whole runtime on teardown
`crates/cloo-server/tests/compat.rs` hung the entire `#[tokio::test]` (current-thread) runtime when
its effects fixture destructured `SpawnedSession { mut events, .. }` and let the `handle` drop: the
actor's command channel closed, `run` hit `Step::Command(None) => break`, and the cleanup path calls
a *blocking* `reactor.wait()` on the surviving pane — a child stuck in `read _` never exits, so the
single runtime thread wedged and no other future (including the test's own drain) could run. Keep the
handle alive for the whole test, and let a child that must be drained exit on its own rather than
block on `read`. Other tests only survive this because they *finish* and let runtime-drop abort the
detached actor; a test that awaits after the handle drops does not get that reprieve.
