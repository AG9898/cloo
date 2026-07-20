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

**`cloo-proto` only.** The other four libraries are still scaffolds and the binary is a
placeholder, so the workspace run is 13 tests in one crate across six targets. This section
grows as M0 lands.

Covered today, all as unit tests in `cloo-proto`:

- Round-trip encode/decode for every `ClientMessage` and `ServerMessage` variant, and for the
  value types they carry, asserting the decode consumes exactly the frame it was given.
- Back-to-back frames decoding out of a single buffer, which is how a socket reader sees them.
- Partial buffers reading as `Incomplete` at *every* split point rather than as an error.
- An oversized length prefix rejected before allocation, and a corrupt payload surfacing as an
  error rather than a panic.
- Handshake version match and mismatch, including that the mismatch error names both versions
  and tells the user to reattach — the acceptance criterion, asserted on the rendered string.

The intended shape for the rest, in the order it becomes testable:

- **`cloo-core`** — the bulk of unit coverage. Layout tree operations, keymap resolution, and
  config parsing are all pure and testable without a terminal.
- **`cloo-term`** — feed known byte sequences, assert resulting grid state. This is the seam
  where `alacritty_terminal` upgrades will break things, so coverage here is what makes the
  pinned dependency safe to bump.
- **`cloo-server`** — integration tests over a real socket with a real PTY running a scripted
  shell. Slower; keep the count deliberate.
- **`cloo-client`** — renderer diffing against a fake grid. Raw-mode and terminal-restore
  behavior is hard to assert automatically and is verified manually.

### Agent-harness compatibility

Compatibility is tested in two layers. Deterministic fixture programs emit alternate-screen,
bracketed-paste, extended-key, focus, SGR mouse, OSC 52/8, notification/title, and resize
sequences; they cover cloo's semantics without requiring a vendor login or a moving CLI release.
Manual smoke runs of installed Codex and Claude Code cover one pane, splits, zoom, resize,
detach/reattach, large paste, mouse, and attention notification. Record the harness and terminal
versions in the test result when a manual behavior changes.

The fixture suite must prove that unsupported outer-terminal effects degrade silently and that
arbitrary OSC/DCS payloads cannot bypass renderer policy. Codex terminal graphics are an optional
manual check only; their absence must not fail core compatibility.

**Not covered, by intent:** aesthetic judgment and exact animation timing. The style guide is
implemented with renderer-level assertions where practical and judged by dogfooding. Real-terminal
compatibility beyond the deterministic fixture suite is verified through the manual matrix above.

---

## Test File Inventory

| File | Domain | What It Covers |
|---|---|---|
| `crates/cloo-proto/src/frame.rs` | Wire protocol | Round-trip for every message and value type, back-to-back framing, partial and oversized frames, corrupt payloads, handshake version match/mismatch |
| `crates/cloo-proto/src/ids.rs` | Wire protocol | Newtype ID accessors, `Display` prefixes, transparent serialization |

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
- Compatibility fixtures must never depend on a live Codex or Claude account. Vendor CLIs are
  manual smoke-test targets, not deterministic test dependencies.

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
