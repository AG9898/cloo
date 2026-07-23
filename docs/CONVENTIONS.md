# CONVENTIONS.md — Code Style and Patterns

> Normative guide for all code in this project.
> Read before writing any new file.
> This is not a log — it always reflects the current standard.
> When a new pattern is established during implementation, update this file, not a task note.

---

## Universal Rules

- **No secrets in source.** cloo has no credentials today; if that changes, values come from
  the environment only. See [`ENV_VARS.md`](ENV_VARS.md).
- **No orphaned code.** Dead code is removed — not commented out, not left behind a flag.
- **No `unsafe` without a `// SAFETY:` comment** stating the invariant that makes it sound.
  Raw `libc` calls for PTY and termios work are the expected and near-only legitimate use.
- **Docs move with code.** A change to a public interface or an invariant updates the relevant
  file in `docs/` in the same commit.

---

## Rust

### Edition and Toolchain

- Edition 2024, `rust-version = "1.85"`. Do not use features that raise the MSRV without
  updating the workspace manifest and saying so in the commit.
- Version, edition, license, and repository metadata are inherited from the workspace via
  `field.workspace = true`. Never hardcode them in a member crate.

### Language and Types

- `clippy` must be clean at `-D warnings`. Do not `#[allow]` a lint without a comment
  explaining why.
- `rustfmt` defaults, no custom config. Run before committing.
- Prefer `Result<T, E>` with a crate-local error enum over `unwrap()`/`expect()` in library
  code. `expect()` is acceptable in `main.rs` startup paths where failure is genuinely fatal
  and the message explains what went wrong.
- No `unwrap()` on anything reachable from a PTY read, socket read, or render loop. Those run
  in tasks where a panic takes down a session.
- Newtype IDs rather than bare integers — `PaneId`, `TabId`, `SessionId`. They cross the wire
  and get mixed up otherwise.

### Workspace and Module Organization

Six crates, each with one job. Full responsibility table in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

```
crates/
  cloo-proto/    wire types + framing
  cloo-term/     alacritty_terminal wrapper — the ONLY crate importing it
  cloo-core/     session/tab/pane model, layout tree, keymap, config
  cloo-server/   daemon: socket, PTY reactor, damage tracking
  cloo-client/   attach, raw mode, renderer, theming, input encoding
  cloo/          the binary — client-vs-server dispatch, CLI surface
```

- Dependencies are constrained by a layering: `cloo` over {`cloo-server`, `cloo-client`} over
  `cloo-core` over the leaves {`cloo-proto`, `cloo-term`}. A crate may depend on any crate in a
  lower layer; naming a leaf directly is ordinary, not an exception, and needs no justifying
  comment. Never introduce a back-edge, a cycle, or an edge between `cloo-server` and
  `cloo-client` — the two halves stay independent. The current edges are tabulated in
  [ARCHITECTURE.md](ARCHITECTURE.md); update that table when you add one.
- Intra-workspace dependencies are declared once in the root `[workspace.dependencies]` and
  pulled into members with `cloo-core.workspace = true`. Never write a bare `path = "../…"`
  dependency in a member crate — a published crate needs the version alongside the path.
- `cloo-core` performs no I/O. If a change wants to read a file or a socket there, it belongs
  in `cloo-server` or `cloo-client` instead.
- The binary crate stays thin. Logic that could be tested without a terminal belongs in a
  library crate.

### Naming

- Crates and files: `kebab-case` for crate names, `snake_case` for module files.
- Types and traits: `PascalCase`. Functions, methods, locals: `snake_case`.
- Constants and statics: `UPPER_SNAKE_CASE`.
- Wire message variants are nouns or past-tense events (`Attach`, `Damage`, `Detached`), not
  imperative verbs. Actions in the keymap `Action` enum are imperative (`SplitVertical`,
  `FocusLeft`).

### Async Patterns

- Tokio, actor-shaped. **No `Mutex` on session state.** All mutation funnels through the
  session task via `mpsc<Command>`.
- One task per PTY, one session task, one task per attached client. A new long-lived task needs
  a justification in the PR description.
- Damage goes out over a bounded `broadcast` channel as one ordered frame batch. A client task
  that lags drops its partial backlog, subscribes after a fresh session snapshot, and resyncs;
  it is never allowed to stall the session task or the damage coordinator.
- Anything in the render path is frame-budgeted. Coalesce; never emit one update per PTY read.
- A handle to an actor is a **sender and nothing more**. Never hand out a reference to the state
  behind it, and never keep a second path to that state "just for reads" — the point of the
  channel is that arrival order is the only order.
- A notification a reader acts on by *looking at current state* is a level, not an edge: queue at
  most one and let it coalesce. A notification that carries information the reader cannot recover
  — the child exited — must be delivered, not dropped.
- **An actor loop never awaits a send on its own outbound channel.** Delivering has to be
  something the loop *tries*, not something it waits for: park the value in a queue the task owns
  and select over a permit alongside the loop's other branches. Awaiting instead makes a slow
  reader indistinguishable from a wedged session, and where the reader drains the channel only
  between requests it is a deadlock — it is waiting for a reply the parked actor will never send.
- Every branch of a `select!` must be cancel-safe, and the reason must be stated in a comment
  where it is not obvious. The usual proof is that the branch's only suspension point is itself
  cancel-safe and that nothing is consumed or recorded after it.

### Terminal and PTY Code

- `cloo-term` exposes only: feed bytes, read cells, resize, scrollback access. If a caller
  needs an `alacritty_terminal` type, widen `cloo-term`'s own API instead of leaking the
  dependency.
- `cloo-term` owns its own `Cell`, `Color`, and `CellAttrs` types rather than reusing the
  `cloo-proto` ones — it has no intra-workspace dependencies, and that is what keeps the
  emulation backend swappable without touching the wire. The two `CellAttrs` bit layouts are
  identical on purpose, so the conversion `cloo-core` owns stays a field copy. **Changing a bit
  position in one requires changing it in the other in the same commit.**
- A grid dimension of zero never reaches the backend. `TermSize::new` is the single validation
  point and returns `TermError::ZeroSize`, which is why `Emulator::new` and `Emulator::resize`
  are infallible.
- PTY resources are restored by ownership, not by a shutdown call. A descriptor is an `OwnedFd`
  the moment the `libc` call returns it, and the type that owns a child process reaps it in
  `Drop`. Never rely on a caller remembering to close or wait — a closed pane must not be able
  to leak a descriptor or leave a zombie.
- The PTY master is non-blocking and close-on-exec. A child inheriting a writable master would
  keep the descriptor alive after the parent closes its copy, and reads would never see EOF.
- A read on a PTY master whose slave has closed fails with `EIO` on Linux rather than returning
  zero. Translate that to an ordinary EOF at the PTY boundary; do not make every caller know it.
- A resize is two operations — grid then `TIOCSWINSZ` — and must be issued in that order behind
  one function. Never expose a path that does only one of them.
- A resize's geometry comes from **one** `Layout::resolve` per resize. The rect a client is told
  about and the `winsize` its child is given must be the same computation, not two that agree
  today.
- `SIGWINCH` carries no geometry; it is always paired with a `TIOCGWINSZ`. A signal whose new
  size matches the old one is swallowed rather than turned into a resize — a child redrawing for
  a size it already had is visible flicker.
- A degenerate outer size is ignored, never refused. Terminals report zero rows mid-drag, and a
  session that dies over it is worse than one that keeps its last usable geometry.
- Raw mode and termios changes must be restored on **every** exit path, including panic and
  signal. A client that leaves the user's terminal in raw mode is a critical bug. Restoration is
  by ownership — an RAII guard whose `Drop` restores — plus a panic hook and signal handlers
  reading a process-global slot the guard arms. A signal handler may call only async-signal-safe
  functions: no allocation, no locking, and never a `Mutex`.
- Escape sequences are emitted through the renderer, never printed ad hoc. Rendering produces an
  owned byte buffer rather than writing to a descriptor, which is what makes a frame assertable
  against an exact expected string.
- An SGR sequence always leads with a `0` reset so it describes the target rendition absolutely.
  Never emit a rendition as a delta from whatever the previous frame left behind.
- Never emit a sequence for a capability the client did not report. Pick the documented fallback
  instead — a `Color::Rgb` without `truecolor` downsamples to the 256-colour palette.
- Outer-terminal effects use typed, capability-gated renderer APIs. The effect vocabulary names
  title, clipboard, hyperlink, notification, progress, and explicitly unavailable graphics; it
  has no raw OSC, DCS, or graphics-payload variant. Every change to that wire type bumps the
  handshake version. Do not forward arbitrary control bytes from a pane to the user's terminal.
- Pane attention changes come from explicit commands, lifecycle events, bells, or opt-in adapters.
  Never infer agent state by matching text in a rendered grid or transcript.

### Configuration

- TOML at `~/.config/cloo/config.toml`, live-reloaded on `SIGHUP`.
- Config parsing lives in `cloo-core` and produces a validated struct. Invalid config warns and
  falls back to defaults — it never panics and never silently ignores a bad key.
- A parser takes configuration *text*, never a path. Reading the file is the server's, which is
  what keeps `cloo-core` free of I/O.
- `cloo-server::config` resolves `CLOO_CONFIG`, then `XDG_CONFIG_HOME`, then the `HOME/.config`
  fallback. Its `ConfigManager` replaces the live configuration only after the whole file has
  read and parsed successfully; a bad `SIGHUP` reload keeps the prior valid value intact.
- Keep the two failure modes apart. A document error (malformed TOML, unknown key) rejects the
  whole document: startup warns and uses defaults, while a reload keeps the previous valid
  configuration. One well-formed entry that fails validation is dropped on its own with a warning
  naming it, and its neighbours still load.
  Never coerce a rejected value into a nearby valid one — a clamped setting is a setting the user
  never wrote.
- A setting whose only fallback would leave cloo unusable keeps its default explicitly. An
  unspellable key `prefix` is the case that exists today: the binding is dropped, `C-b` stays, and
  the user is warned — a prefix nobody can press is a session with no way out.

### Keys

- A chord's *spelling* lives in `cloo-core::keymap` and its *bytes* live in `cloo-client::input`.
  Neither crate does the other's half: what a terminal sends depends on the terminal, and what a
  key is called in `config.toml` must not.
- A binding names an `Action`, never bytes for a child. An action that needs text a chord cannot
  carry has no configuration spelling at all, so the vocabulary itself is what stops a binding from
  naming a command it could not supply an argument for.
- **Nothing is cloo's until the prefix is pressed.** A key run outside a pending prefix is passed
  through as the *same bytes that arrived*, never re-encoded from a decoded chord, and a chord in
  the keymap means nothing until the prefix precedes it. A sequence the client cannot name is the
  pane's too — decoding must answer "not a chord I know" rather than guess.
- After the prefix, exactly one chord is consumed. An unbound one is swallowed rather than
  delivered: the user was talking to cloo, and passing it on is how a mistyped command ends up in
  a shell.

---

## Testing

Rules that affect how code is written:

- Layout tree operations (split, close, collapse, resize) are pure and must be unit tested
  without a PTY.
- Wire types get round-trip tests — encode, decode, assert equality.
- Anything requiring a real PTY belongs in an integration test under `tests/`, not a unit test.

Full testing guide: [`TESTING.md`](TESTING.md)

---

## Never

- Never import `alacritty_terminal` outside `cloo-term`. This is the load-bearing rule of the
  whole design — see [`DECISIONS.md`](DECISIONS.md) RESOLVED-02.
- Never depend on `alacritty_terminal` with a caret/range version. Pin exactly.
- Never add a `Mutex` to session state.
- Never emit a render update per PTY read — coalesce and cap.
- Never change a wire type without bumping the handshake version.
- Never leave the terminal in raw mode on any exit path.
- Never screen-scrape a harness TUI to infer its state.
- Never bypass renderer capability checks by forwarding arbitrary OSC/DCS bytes.
- Never add Windows-specific code. Out of scope for v1.
- Never bulk-rewrite `docs/workboard.json` — targeted edits only.
- Never commit secrets or credentials.

## Always

- Always run `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, and
  `cargo test --workspace` before marking a task done.
- Always restore terminal state on exit paths, including panics.
- Always write `// SAFETY:` on `unsafe` blocks.
- Always update `docs/` when public behavior, interfaces, or invariants change.
- Always store layout as ratios, never as cell counts.
