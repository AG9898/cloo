# TESTING.md — Test Suite Reference

> Canonical source for how to run tests, what is covered, and how to write new tests.
> Read before adding any new test file or modifying an existing one.
> Code conventions that affect test structure live in [`CONVENTIONS.md`](CONVENTIONS.md).

---

## Quick Start

```bash
# Run all tests
cargo test --workspace

# Run tests for one crate
cargo test -p cloo-core

# Run a single test by name
cargo test --workspace layout_split_collapses_parent

# Show output from passing tests
cargo test --workspace -- --nocapture

# Format + lint (run these too — they are part of the fast suite)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

---

## Test Stacks

| Stack | Tool | Location | Run Command |
|---|---|---|---|
| Unit | built-in `#[test]` | `#[cfg(test)]` modules, collocated in `src/` | `cargo test --workspace` |
| Integration | built-in harness | `crates/<crate>/tests/` | `cargo test --workspace` |

No external test framework. If a property-testing or snapshot crate becomes warranted, add it
here and record why in [`DECISIONS.md`](DECISIONS.md).

---

## What Is Covered

**Nothing yet.** The workspace contains a single placeholder binary with no functionality and
no tests. This section gets rewritten as M0 lands.

The intended shape, in the order it becomes testable:

- **`cloo-core`** — the bulk of unit coverage. Layout tree operations, keymap resolution, and
  config parsing are all pure and testable without a terminal.
- **`cloo-proto`** — round-trip encode/decode for every wire message, plus handshake version
  mismatch handling.
- **`cloo-term`** — feed known byte sequences, assert resulting grid state. This is the seam
  where `alacritty_terminal` upgrades will break things, so coverage here is what makes the
  pinned dependency safe to bump.
- **`cloo-server`** — integration tests over a real socket with a real PTY running a scripted
  shell. Slower; keep the count deliberate.
- **`cloo-client`** — renderer diffing against a fake grid. Raw-mode and terminal-restore
  behavior is hard to assert automatically and is verified manually.

**Not covered, by intent:** visual/aesthetic output, animation timing, and real-terminal
compatibility across emulators. Those are verified by using cloo (see the dogfooding success
criterion in [`PRD.md`](PRD.md)).

---

## Test File Inventory

*(No test files yet — add a row here when adding one.)*

| File | Domain | What It Covers |
|---|---|---|
| — | — | — |

---

## Writing New Tests

### Rules

- **Layout tree operations must be unit tested.** Split, close, collapse, and resize are pure
  tree manipulation — there is no excuse for them to be untested.
- **Every wire type gets a round-trip test.** Encode, decode, assert equality. Protocol desync
  presents as a rendering bug and is miserable to debug from the symptom.
- **Unit tests never spawn a PTY.** If a test needs a real PTY or a real socket, it is an
  integration test and belongs in `tests/`.
- **A `cloo-term` upgrade requires the grid tests to pass unchanged.** If they need editing to
  accommodate a new `alacritty_terminal` version, that is a behavior change and needs a note in
  the commit.
- Tests must not leave stray daemons or sockets behind. Integration tests clean up
  `$XDG_RUNTIME_DIR/cloo/` entries they create.

### Patterns

- **Table-driven layout tests.** Build a tree, apply an operation, assert the resulting shape
  and each leaf's `Rect`. Compare structurally, not via `Debug` strings.
- **Grid assertions by row.** When asserting `cloo-term` state, compare a single row's rendered
  text rather than the whole grid — failures stay readable.
- **Scripted shells for integration.** Drive `sh -c` with a fixed command sequence rather than
  an interactive shell, so timing is deterministic.
- **No sleeps for synchronization.** Await a condition or a channel message. A `sleep` in a test
  is a future flake.

### Adding a New Test File

1. Unit tests go in a `#[cfg(test)] mod tests` block in the file under test.
2. Integration tests go in `crates/<crate>/tests/<area>.rs`.
3. Add a row to the Test File Inventory table above.
4. Run `cargo test --workspace` to confirm no regressions before committing.
