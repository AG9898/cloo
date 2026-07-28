# cloo — Agent Working Guide

<!-- AGENTS.md is the canonical file. CLAUDE.md is a symlink to it. -->

---

## Overview

cloo is a client-server terminal multiplexer in Rust, designed as a workspace for concurrent
coding-agent harnesses. A daemon owns PTYs and session state; thin clients attach over a Unix
socket and render.

**Pre-alpha.** M0–M8 are complete. M9 is the high-fidelity terminal UI pass: the existing sparse
attached frame is a functional scaffold, while the eight-card handoff is the visual acceptance
contract. Plain `cloo` create-or-attaches `default`; `cloo <program>` remains an in-process one-pane
launch; `cloo server [session]` is foreground and `cloo attach [session]` joins its multipane
frame. Product scope is in [`docs/PRD.md`](docs/PRD.md); the canonical task queue is
[`docs/workboard.json`](docs/workboard.json).

---

## Quick Start & Verification

```bash
# Build and inspect the CLI
cargo build --workspace
cargo run -p cloo -- --help

# Before marking work done
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run `cargo build --release` when validating release artifacts. `cargo fmt` applies formatting.
Plain `cloo` starts or joins `default`; `cloo server [session]` exposes daemon diagnostics. cloo has
no database or runtime `.env` file.

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
  PRD.md            Product scope, users, M0–M8 roadmap
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
  package.json   The `clooterminal` npm launcher and optional native package metadata
Cargo.toml       Workspace root — shared version/edition/license metadata
```

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
| Durable cross-cutting constraint discovered | This file (`AGENTS.md`) — only when it qualifies for Durable Discoveries |

**Rule:** If a section in `AGENTS.md` summarizes something, and the full doc changes, update
both the summary here and the full doc in the same commit.

---

## Workboard and Task Workflow

[`docs/workboard.json`](docs/workboard.json) is the canonical task queue; its schema and usage
contract are [`docs/workboard.schema.json`](docs/workboard.schema.json) and
[`docs/workboard.md`](docs/workboard.md). Query it with **query-workboard** rather than loading the
whole board.

A task is startable when it is `todo`, has no `blocked_by`, and every `depends_on` task is `done`.
For one task: read this guide, use **query-workboard** to select it, then use **start-task** to
implement, document, verify, and update it. Commit with a summary of what changed, what was
skipped, and what is next. Use **ralphloop** wrapping start-task for bounded multi-task runs.

### Board edits

Never bulk-rewrite `workboard.json`. Update only the current task's status, and restore
`in_progress` to `todo` when unresolved work blocks completion. Use **edit-workboard** for targeted
field edits, including dependency-safe deletion or task splitting.

### Skills

Skills use the harness prefix — `$` in Codex and `/` in Claude Code — because `AGENTS.md` and
`CLAUDE.md` are the same file. Available project skills are **query-workboard**, **start-task**,
**edit-workboard**, **project-plan**, and **ralphloop**. They are rendered from `ag.dev`; never edit
the copies under `.claude/`, `.agents/`, or `.codex/` directly.

### Stopping Conditions

Stop and report when no task is startable; when a verification failure has no obvious fix; before an
unauthorized irreversible publish or force-push; or when a task would violate the documented crate
boundaries or terminal-emulator constraint. Do not work around those conditions.

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

Before marking any task done, run:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Keep focused unit tests with the crate that owns the behaviour. Put integration tests spanning
`cloo-server` and `cloo-client` in `crates/cloo`, never in either library crate, because the
sideways dependency remains forbidden even in test-only dependencies.

Use a real PTY whenever terminal, raw-mode, signal, resize, or child-input behaviour matters.
Bound every actor reply and external wait with the fixture deadline, and prove an assertion is
non-vacuous by making its relevant implementation change fail the test. Fixtures that create a
daemon must clean it up; `cargo test` does not clean up real runtime sockets.

The complete test inventory, ownership, and patterns live in [`docs/TESTING.md`](docs/TESTING.md).

---

## Deployment

cloo ships as a locally installed binary through two channels:

- **crates.io** — `cloo`, built from source via `cargo install cloo`.
- **npm** — `clooterminal`, a `cloo` launcher with the Linux x64 native binary as an optional
  dependency. A maintainer builds and publishes the two tarballs directly from the terminal.

**Agents must never publish to either registry.** Both are irreversible and public: npm allows
unpublishing only within 72 hours and burns the name afterward, and crates.io versions cannot
be deleted at all. Publishing is the project owner's action.

---

## Living Document

`AGENTS.md` is an operational briefing, not an implementation diary. The canonical docs and
well-named tests hold the detailed design; Git history preserves the reasoning that led there.

Keep `## Durable Discoveries` deliberately small. An entry belongs there only when it is a
cross-cutting, non-obvious constraint that would change how a future agent approaches work and is
not already stated adequately in the architecture, conventions, testing guide, or a nearby API.
Do not add task-specific debugging history, completed-milestone narration, or facts recoverable
from a targeted code search. Rewrite the canonical documentation instead when that is the better
home.

The section has a hard limit of 18 entries. Adding one requires deleting or merging another; each
entry should be a compact explanation of both the constraint and the failure it prevents.

---

## Durable Discoveries

### Session and tab invariants are construction-time guarantees
A session always has an active tab and a tab always has at least one pane. Keep those facts in the
model, not at every call site: reject the last-tab close, validate an unknown tab before applying
that rule, and update the active index when a tab closes. `TabName` and pane text share one
validator so a title cannot admit characters that a pane name refuses.

### Stream framing distinguishes normal detach from corruption
Postcard is not self-delimiting, so the transport uses a big-endian `u32` length prefix and treats
an incomplete buffer as a request to read more. EOF between frames is an ordinary detach;
EOF inside a frame is a truncated-frame error. Collapsing those cases either reports a harmless
detach as a failure or hides a damaged message.

### Resolve and min-size validation share the ratio-to-cells conversion
`cloo-core::layout::split_extent` is the one place a stored ratio becomes cell counts. The
minimum-size test must use it too, otherwise rounding can accept a split that the later layout pass
renders below minimum. Resizes still resolve to drawable one-cell floors rather than rejecting a
smaller terminal.

### Terminal and wire cell types intentionally remain separate
`cloo-term` duplicates the wire cell, colour, and attribute representations so the emulator wrapper
stays independent of the protocol crate. `cloo-core` owns the field-by-field conversion. Keep the
attribute-bit layouts aligned whenever either side changes, and do not solve this by adding an
intra-workspace dependency to `cloo-term`.

### Emulator grid coordinates are buffer coordinates
The terminal backend indexes its grid against the whole buffer, not the visible viewport. Convert
visible row `r` using the display offset before reading it, and account for that offset when locating
the cursor. This only fails once scrollback exists, so a no-scrollback fixture is not sufficient.

### PTY EOF and cleanup are ownership details
Linux returns `EIO`, not a zero-byte read, when the final PTY slave closes; translate it to EOF at
the PTY boundary. Drop the parent copy of the slave immediately after spawning or EOF never arrives.
The master owns child cleanup, so its drop path must reap rather than leave a zombie behind.

### Terminal restoration has signal-safety constraints
The raw-mode guard owns normal restoration, while signal restoration needs an independently armed,
process-global termios slot because a handler cannot borrow the guard. The signal path may use only
async-signal-safe operations and must restore every enabled reporting mode as well as raw mode.
Library tests cannot safely signal their own runner; prove that path through the binary on a PTY.

### Renderer tests require deterministic frames and an EOF flush
Frames are byte-tested, so reset styles absolutely rather than as a delta from a previous render and
emit only capabilities the client negotiated. The rate-capped loop must also render once after PTY
EOF when the grid is dirty; otherwise a short-lived `printf` child can exit before its only output
is painted.

### Socket paths are neither liveness nor permission to unlink
A socket file does not establish that a daemon is alive: connect/bind plus the advisory lock settle
that question. Inspect paths with `symlink_metadata`, refuse anything that is not itself a socket,
and record the bound identity so teardown cannot unlink a successor. These checks protect both
stale recovery and a live daemon from destructive cleanup.

### Bare-workspace startup is a handoff race, and the daemon has no client stdio
Concurrent bare `cloo` invocations may each start a server, but only the lock winner serves; a loser
must keep probing briefly before deciding startup failed. Start the background daemon with stdin,
stdout, and stderr redirected to `/dev/null`, since shared terminal output corrupts the attached
client and a dropped stderr pipe can later kill the daemon. Use `cloo server <session>` for visible
diagnostics.

### The crate graph forbids sideways tests as well as production dependencies
Crates may depend downward through the documented layers, but `cloo-server` and `cloo-client` may
never depend on one another, including in `[dev-dependencies]`. A test needing both halves belongs
in the `cloo` binary crate, which is the composition root. This preserves the server/client
separation instead of creating an apparently harmless test-only cycle.

### A daemon keeps pumping after detach and snapshots precede subscriptions
Detaching must not stop PTY reads: otherwise a reattached client sees an obsolete grid and an active
child can block on a full PTY buffer. On attachment, capture the resync snapshot before creating the
broadcast receiver, with no await between those actions. Reversing that order permits a stale event
to arrive after the authoritative snapshot.

### Resize is three coupled behaviours, not one assertion
A resize updates the cached grid, changes the PTY window size, and prompts the child to handle
`SIGWINCH`. Integration coverage must assert both the resulting grid geometry and the child-observed
`stty size`; test scripts must avoid treating a stale, periodically printed size as fresh evidence.
Treat signal/input ordering as nondeterministic unless the fixture explicitly removes that race.

### Input crosses the wire as events and is encoded from child modes
The emulator, not the outer terminal, determines bracketed paste, focus, and mouse encoding, so the
wire transports typed events and the server encodes bytes for the named pane's modes. Strip paste
terminators out of the body before adding bracketed-paste delimiters, or pasted text can close its
own bracket. A lone Escape needs a frame-tick flush, but never flush a partial paste as keystrokes.

### Attention has a clock separate from pane metadata and geometry
Attention travels as its own projection because state without provenance is not sufficient for the
chrome to describe a pane. Produce pane metadata, attention, and layout geometry from the same
layout resolution, then diff attention independently so unrelated metadata changes do not resend it.
The session actor is the only writer and silently ignores reports for panes that have closed.

### Draw and hit-test from the same client geometry
Server pane rectangles describe grids, while client `PaneArea` also accounts for headers, gutters,
and status chrome. Frame composition and mouse hit-testing must use that one client geometry source;
otherwise a click can resolve somewhere other than what the user sees. Claim chrome before panes so
no chrome event can reach an application as a wire mouse event.

### Session actors must never await their own outbound channel
A coalesced output event can fill the actor's small event channel while a lifecycle event waits
behind it, leaving the actor unable to answer the snapshot that would let its reader make progress.
Use an actor-owned outbox and select for a permit instead. Every test awaiting an actor reply must
wrap it in its deadline, or a liveness regression hangs the entire suite instead of failing clearly.

### Client launch requests name configured profiles, never commands
`Action::LaunchProfile` carries a profile identifier only; no client-controlled argv has a path to a
PTY spawn. Resolve the identifier against daemon configuration before mutating the session so an
unknown profile, empty identifier, or shell-looking string creates neither a pane nor a child.
This is the type-level boundary that keeps the launcher declarative and safe.
