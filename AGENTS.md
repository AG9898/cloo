# cloo — Agent Working Guide

<!-- AGENTS.md is the canonical file. CLAUDE.md is a symlink to it. -->

---

## Overview

cloo is a client-server terminal multiplexer in Rust — tmux's functionality with an interface
worth looking at. It is designed first as a workspace for many concurrent coding-agent harnesses.
A daemon owns the PTYs and all session state; thin clients attach over a Unix socket and render.

**The project is pre-alpha and mostly unwritten.** Planning is complete and the design is
settled; the only code is a placeholder binary that prints help and exits. Agents here are
implementing the M0–M7 roadmap in [`docs/PRD.md`](docs/PRD.md), starting from a bare workspace.

The canonical task queue is [`docs/workboard.json`](docs/workboard.json). It is currently empty.

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
  cloo/          The binary — currently the only crate that exists
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

Five more crates are planned and land across M0–M2: `cloo-proto`, `cloo-term`, `cloo-core`,
`cloo-server`, `cloo-client`. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

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
- **Layout stores ratios, not cell counts** — that is what survives a terminal resize.
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
- Never leave the terminal in raw mode on any exit path, including panic.
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

**The board is currently empty.** Tasks are seeded by the project owner. Milestone structure
lives in [`docs/PRD.md`](docs/PRD.md) — M0 through M7, each independently runnable.

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
- No startable task exists (all are blocked or done) — currently true, the board is empty.
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
  leftover sockets in `$XDG_RUNTIME_DIR/cloo/`.
- **The npm package is `clooterminal`, not `cloo`.** npm's similarity filter rejects `cloo` at
  publish time even though the name shows as available on a registry lookup. See
  [`docs/DECISIONS.md`](docs/DECISIONS.md) RESOLVED-05.

---

## Environment Variables

cloo reads standard environment variables and owns no secrets. The ones that matter for running
it locally: `XDG_RUNTIME_DIR` (socket location), `TERM` (capability detection), and
`CLOO_SOCKET` / `CLOO_CONFIG` for isolating a dev instance from a live one.

See [`docs/ENV_VARS.md`](docs/ENV_VARS.md) for the canonical matrix.

---

## Testing

Before marking any task done:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

There are no tests yet. Layout tree operations and wire round-trips are the first things that
must get coverage as they land.

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

### 2026-07-20 — npm rejects `cloo` at publish time, not lookup time
`npm view cloo` returned 404 and `npm publish --dry-run` passed, but the real publish failed
with a 403 from npm's package-name similarity filter (too close to `clone`, `cli`, `clsx`,
`clui`, and others). The name is now `clooterminal`, with `cloo` preserved as the command via
the `bin` field. Registry availability is not proof a name is publishable.

### 2026-07-20 — DESIGN.md was migrated into docs/
The root `DESIGN.md` was the original planning document and has been folded into
`docs/PRD.md` (scope, milestones), `docs/ARCHITECTURE.md` (topology, protocol, layout), and
`docs/DECISIONS.md` (the resolved/open decision log). It no longer exists — do not recreate a
root-level design doc, since `docs/INDEX.md` forbids root stubs that redirect into `docs/`.
