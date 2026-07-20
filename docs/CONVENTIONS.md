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

- Dependencies flow one way: `cloo` → {`cloo-server`, `cloo-client`} → `cloo-core` →
  {`cloo-proto`, `cloo-term`}. Never introduce a cycle or a back-edge.
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
- Damage goes out over `broadcast`. Client tasks that lag are dropped and told to resync, never
  allowed to stall the session task.
- Anything in the render path is frame-budgeted. Coalesce; never emit one update per PTY read.

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
- Raw mode and termios changes must be restored on **every** exit path, including panic and
  signal. A client that leaves the user's terminal in raw mode is a critical bug.
- Escape sequences are emitted through the renderer, never printed ad hoc.
- Outer-terminal effects use typed, capability-gated renderer APIs. Do not forward arbitrary
  OSC/DCS bytes from a pane to the user's terminal.
- Pane attention changes come from explicit commands, lifecycle events, bells, or opt-in adapters.
  Never infer agent state by matching text in a rendered grid or transcript.

### Configuration

- TOML at `~/.config/cloo/config.toml`, live-reloaded on `SIGHUP`.
- Config parsing lives in `cloo-core` and produces a validated struct. Invalid config warns and
  falls back to defaults — it never panics and never silently ignores a bad key.

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
