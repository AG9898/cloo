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

**Every crate in the workspace, including the binary.** The workspace run is 108 unit tests
across six crates, 21 integration tests, and six doctests. This section grows as M1 lands.

Covered today in `cloo-core`, all as unit tests:

- Every layout operation, table-driven: split on both axes, nested and mixed-axis splits, close
  and parent collapse at every depth, ratio-based resize, and the flattened layout pass.
- Rectangles tiling their area exactly, asserted on an odd-sized area so rounding is exercised.
- Every rejection path leaving the layout unchanged, compared structurally against a clone taken
  before the call: minimum-size violations, zero-size areas, extreme ratios, non-finite ratios,
  unknown panes, duplicate panes, and closing the last pane.
- A shrunken area squeezing panes to a one-cell floor rather than dropping them, and a zero-size
  area resolving without a panic.
- ID allocators being monotonic, non-reusing, resumable, and saturating at `u64::MAX`.
- The emulator-cell to wire-cell conversion in `grid.rs`: every colour form and rendition flag
  crossing intact, an invisible cursor becoming "nothing to draw" rather than a hidden shape, and
  `HollowBlock` degrading to a block. One assertion compares the two crates' attribute bit values
  directly — it is the tripwire for the duplicated `CellAttrs` layouts drifting apart.

Covered today in `cloo-proto`, all as unit tests:

- Round-trip encode/decode for every `ClientMessage` and `ServerMessage` variant, and for the
  value types they carry, asserting the decode consumes exactly the frame it was given.
- Back-to-back frames decoding out of a single buffer, which is how a socket reader sees them.
- Partial buffers reading as `Incomplete` at *every* split point rather than as an error.
- An oversized length prefix rejected before allocation, and a corrupt payload surfacing as an
  error rather than a panic.
- Handshake version match and mismatch, including that the mismatch error names both versions
  and tells the user to reattach — the acceptance criterion, asserted on the rendered string.

Covered today in `cloo-term`, all as unit tests, all by feeding known byte sequences and
asserting grid state. This is the seam where an `alacritty_terminal` upgrade will break things,
so this coverage is what makes the pinned dependency safe to bump:

- Every SGR rendition flag, and named, indexed, and RGB colour. A role name (default foreground
  or background) staying `Color::Default` rather than collapsing to a palette index, since the
  role resolves in the client's theme.
- An escape sequence and a multi-byte UTF-8 character each split across two `feed` calls,
  because the PTY reactor has no control over where a read boundary falls.
- Entering and leaving the alternate screen, the primary grid surviving the round trip, and the
  alternate screen accumulating no scrollback.
- Resize reporting the new geometry and row width, shrink-then-grow preserving unwrapped
  content, and a 1x1 grid being valid — the layout pass squeezes to a one-cell floor, so the
  emulator has to survive the result.
- Scrollback growing to its configured limit and no further, a zero-scrollback grid retaining
  nothing, scrolling clamping at both ends, and the cursor reporting itself invisible once
  scrolled out of the viewport.
- Cursor position under output and absolute positioning, DECTCEM visibility, and DECSCUSR shape.
- Zero grid dimensions rejected at `TermSize::new` with the offending dimensions named.

Covered today in `cloo-server`, as integration tests in `tests/pty.rs` driving a real
pseudoterminal with a scripted `sh -c` child. The count is deliberately small — these are the
only tests in the workspace that fork a process:

- A scripted shell's output reaching the grid, with the child's exit status reaped.
- An escape sequence split across three child writes still parsing, since the reactor has no
  control over where a read boundary falls.
- `stty size` reporting the configured geometry, which proves both that `openpty` carried the
  `winsize` through and that the child acquired a controlling terminal.
- Input written to the master reaching the child, and the pty's own echo landing on the grid.
- A resize being visible to the child on its next `stty`, and the grid reporting the new size.
- EOF reported once and staying reported, with the child's exit code preserved.
- A nonexistent program failing to spawn with the program named in the error.
- A dropped `Pty` leaving no process behind — not even a zombie.

The `cloo-server` unit tests in `src/pty.rs` are pure by rule: config defaults, the `winsize`
conversion, and error conversion. Nothing that spawns.

Covered today in `cloo-client`. The renderer is a pure function into a byte buffer, so every
frame is asserted against an exact expected string rather than eyeballed — all unit tests:

- A blank frame, a styled run, and a mid-row style change, byte for byte. The mid-row case is the
  one that proves an SGR sequence leads with a reset instead of inheriting the previous cell's
  rendition.
- Rendering the same grid twice producing identical bytes, which is what catches a buffer that
  was not cleared between frames.
- Every rendition flag having a code and emitting in a fixed order, both colour selectors, and
  every cursor shape mapping to a distinct DECSCUSR sequence.
- Truecolor emitted only when the client reported it, and downsampled to the palette otherwise —
  asserted on the specific palette entries, including that true black and white take the exact
  cube entries rather than the greyscale ramp.
- The cursor hidden for the whole paint and placed, shaped, and shown only after the reset; and
  no cursor message leaving it hidden.
- Row updates rejected out of range and at the wrong width, each compared against a clone taken
  before the call to prove the grid is unchanged.
- Resize keeping the overlapping cells and blanking the rest, a zero-sized grid rendering without
  a panic, and multi-byte characters surviving the render intact.

Raw-mode behaviour needs a real tty, so it lives in `crates/cloo-client/tests/raw_mode.rs`, which
opens a pseudoterminal pair and drives the slave side. Three of the four restore paths are
asserted automatically; only the signal path is still manual, since asserting it means killing the
test process:

- Entering raw mode actually clearing `ECHO`, `ICANON`, and `ISIG`, and drop restoring the exact
  original flag words — not merely "some cooked state".
- An explicit `restore` reporting success and releasing the global slot, and the following `Drop`
  being a no-op.
- An error unwinding past a live guard, and a panic inside one, both leaving the terminal cooked.
- A second guard refused with `AlreadyActive` while leaving its own terminal untouched, so a
  collision cannot overwrite the first guard's saved state.
- A pipe refused as `NotATerminal`.

Outer-terminal capability detection is a pure function of `TERM` and `COLORTERM`, unit tested in
`src/outer.rs`: truecolor established only by an explicit signal, a `dumb` or absent `TERM`
claiming nothing at all, capabilities that need a query-and-reply staying false, and a degenerate
`winsize` falling back to 80x24 rather than rendering into a zero-sized grid.

These tests share the process-global restore slot, so each takes a module-level `Mutex` first;
Rust runs integration tests in parallel threads within one binary and two live guards would
legitimately collide. The pure `termios` transformation is unit tested in `src/raw_mode.rs`
instead, along with the restore slot's arm/disarm state machine driven on a local instance.

Covered today in the `cloo` binary, as integration tests in `tests/cli.rs`. The command-line
cases run the binary directly; the smoke-path cases run it with its stdio on a pseudoterminal
slave, because cloo refuses to start without a terminal and the master side is the only honest
stand-in for the user's screen:

- `--version`, `--help`, and an unrecognized flag exiting 64 with the flag named — never executed
  as a program name.
- Piped stdin refused with "must be run from a terminal", before any child is spawned.
- A child's output reaching the screen *inside a renderer-built frame*, asserted on the frame
  preamble rather than on the raw text, which is what distinguishes rendering from forwarding.
- Typed input on the master reaching the child, and the terminal left cooked after cloo exits.
- The child's exit code becoming cloo's exit code.

These read until an expected string appears rather than sleeping, with a deadline so a wiring
regression fails instead of hanging. Command-line parsing and the `$SHELL` fallback are unit
tested in `src/local.rs`.

The intended shape for the rest, in the order it becomes testable:

- **`cloo-core`** — keymap resolution and config parsing still to come. Like layout, both are
  pure and testable without a terminal.
- **`cloo-server`** — socket-level integration tests join the PTY ones at M1. Slower; keep the
  count deliberate.
- **`cloo-client`** — full-grid rendering and raw-mode restoration landed at M0-06. Incremental
  diffing against previous frames arrives with damage coalescing at M1-04. Only the signal
  restore path stays manual.

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
| `crates/cloo-core/src/layout.rs` | Layout tree | Split, close, collapse, resize, the layout pass, exact tiling, and every rejection leaving the tree unchanged |
| `crates/cloo-core/src/id.rs` | Session model | Monotonic non-reusing ID allocation, resume, and saturation |
| `crates/cloo-core/src/error.rs` | Session model | `LayoutError` messages naming the pane, sizes, and axis they refused |
| `crates/cloo-core/src/grid.rs` | Wire conversion | Emulator cells, colours, attributes, and cursor crossing into wire types, and the two crates' attribute bit layouts still agreeing |
| `crates/cloo-term/src/emulator.rs` | Emulation | Feed across read boundaries, every SGR flag and colour form, alternate screen, cursor position/visibility/shape, resize and reflow, scrollback growth and clamping |
| `crates/cloo-server/src/pty.rs` | PTY reactor | Pure only: config defaults and builder, `winsize` conversion, `TermError` to `PtyError` conversion |
| `crates/cloo-server/tests/pty.rs` | PTY reactor | Scripted-shell output reaching the grid, split reads, `winsize` and controlling terminal, input forwarding, resize seen by the child, EOF and exit status, spawn failure, and drop reaping the child |
| `crates/cloo-client/src/renderer.rs` | Renderer | Byte-exact frames, absolute SGR, colour downsampling, cursor placement, and grid apply/resize rejections |
| `crates/cloo-client/src/outer.rs` | Outer terminal | Capability detection from `TERM`/`COLORTERM` and the degenerate-`winsize` fallback |
| `crates/cloo-client/src/raw_mode.rs` | Raw mode | Pure `termios` transformation and the restore slot's arm/disarm state machine |
| `crates/cloo-client/tests/raw_mode.rs` | Raw mode | Entry, drop, explicit restore, error unwind, panic, second-guard refusal, and a pipe refused |
| `crates/cloo/src/local.rs` | Binary | The `$SHELL` fallback and the frame-rate cap |
| `crates/cloo/tests/cli.rs` | Binary | The command line, refusal without a terminal, and the one-pane smoke path driven over a pseudoterminal |

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
