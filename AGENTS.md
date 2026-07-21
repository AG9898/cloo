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

`cloo-proto` has wire round-trip, framing, and handshake coverage as of M0-02. `cloo-core` has
table-driven layout tree coverage as of M0-03, plus the emulator-to-wire cell conversion as of
M0-07. `cloo-term` has grid coverage — SGR, alternate screen, cursor, resize, scrollback — as of
M0-04. `cloo-server` has PTY integration coverage in `tests/pty.rs` against a scripted `sh -c`
child as of M0-05, plus socket lifecycle coverage in `tests/socket.rs` as of M1-01 and handshake
coverage in `src/conn.rs` as of M1-02. `cloo-client` has byte-exact renderer coverage and raw-mode
restore coverage — normal, error, and panic paths, against a real pty in `tests/raw_mode.rs` — as
of M0-06, plus attach-handshake coverage in `src/attach.rs` as of M1-02 and `SIGWINCH` watch
coverage in `src/resize.rs` as of M1-03. `cloo-proto` gained framed-transport coverage in
`src/stream.rs` over a duplex pipe in M1-02. The binary has CLI and one-pane smoke coverage in
`crates/cloo/tests/cli.rs`, run over a pseudoterminal, as of M0-07 — including the `SIGWINCH`
chain end to end as of M1-03 — and end-to-end attach/detach coverage in
`crates/cloo/tests/attach.rs` as of M1-02, extended at M1-03 with a resize asserted on *both*
halves: the grid reflow and the child's own `stty size`. That file lives in the binary crate
because it needs both halves of the wire and `cloo-server` may never name `cloo-client`. Keymap
resolution and config parsing are the next things that must get coverage as they land.

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

### 2026-07-20 — DESIGN.md was migrated into docs/
The root `DESIGN.md` was the original planning document and has been folded into
`docs/PRD.md` (scope, milestones), `docs/ARCHITECTURE.md` (topology, protocol, layout), and
`docs/DECISIONS.md` (the resolved/open decision log). It no longer exists — do not recreate a
root-level design doc, since `docs/INDEX.md` forbids root stubs that redirect into `docs/`.
