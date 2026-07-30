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

## Release Package Verification

The maintainer release path builds the Linux x64 native package locally, then packs the JavaScript
launcher without publishing either package:

```bash
cargo build --locked --release --package cloo --target x86_64-unknown-linux-gnu
scripts/package-npm.sh x86_64-unknown-linux-gnu
```

On the supported Linux x64 development host, pack the JavaScript launcher with:

```bash
mkdir -p dist/npm
(cd npm && npm pack --ignore-scripts --pack-destination ../dist/npm)
```

These commands create only ignored `dist/` artifacts. The maintainer then follows
[`RELEASING.md`](RELEASING.md) to publish the two inspected tarballs from the terminal.

---

## Test Stacks

| Stack | Tool | Location | Run Command |
|---|---|---|---|
| Unit | built-in `#[test]` | `#[cfg(test)]` modules, collocated in `src/` | `cargo test --workspace` |
| Integration | built-in harness | `crates/<crate>/tests/` | `cargo test --workspace` |
| Visual golden | deterministic cell fixtures | `crates/cloo-client/tests/visual/` | `cargo test -p cloo-client --test visual` |
| Live visual PTY | binary integration harness | `crates/cloo/tests/visual_attach.rs` | `cargo test -p cloo --test visual_attach` |

No external test framework is required for cell goldens: fixtures are explicit rows of characters
and semantic styles, reviewed as text. If a snapshot or terminal-screen crate becomes warranted,
add it here and record why in [`DECISIONS.md`](DECISIONS.md).

---

## M9 Visual Acceptance

The handoff's eight cards are acceptance fixtures, not inspiration checked by eye after
implementation. Each card has a deterministic cell-level scene at its documented reference size
and capability tier. The fixture asserts the complete composed frame — geometry, characters,
semantic foreground/background roles, and attributes — rather than testing one helper at a time.

Required coverage:

- Truecolor reference frames for cards 01–08.
- A 16-color frame for every distinct surface family: daily workspace, overlay, status variant,
  notification, and active resize.
- Narrow-width ladders proving optional detail yields before identity, focus, attention,
  selection, or dismissal controls.
- Named-theme default-cell mapping and the terminal-inheritance opt-out, with explicit child
  colors unchanged and the cached grid byte-for-byte equal before and after rendering.
- Real outer-PTY captures for the one-pane workspace, nested split, command palette, session
  switcher, notification, and resize states. These drive `cloo attach` or bare `cloo`, so a pure
  helper that is never connected to `LiveState` cannot satisfy the test.
- Protocol fixtures for every new status/session projection, including version mismatch and an
  inspection request that creates no attachment or resize.

M9-01 builds the harness that coverage is written in. `crates/cloo-client/tests/visual/harness.rs`
holds three pieces: `FrameMatrix`, which paints already-composed spans onto a blank terminal of a
known size and is a pure function of the spans it is given; `ExpectedFrame`, a golden authored as
text — one line of characters and one parallel line of single-character style keys per row, over a
legend of semantic styles; and `check_frame`, which reports the *first* differing cell by row,
column, character, and semantic role, with the row drawn either side of it and a caret under the
cell. `crates/cloo-client/tests/visual/scenes.rs` assembles the client-side state a frame needs —
geometry, tabs, each pane's rectangle, cached grid and header, the attention queue, and the prefix
hint — and runs the production `compose_frame` over it, so a fixture asserts the composed picture
rather than one chrome helper's output. Nothing in the harness opens a pseudoterminal, connects to a
daemon, or writes a byte to a descriptor.

A golden names style-guide *roles*, never literal colors, which is what lets one expectation be
checked against a truecolor theme and its 16-color resolution; `Paint::Reference` marks a role that
chrome still draws from the reference Storm palette rather than resolving through the client theme,
so the remaining gap is visible in the fixture instead of hidden inside an RGB triple. The
`tests/visual.rs` fixtures today cover the one-pane workspace at both capability tiers against one
shared golden, the scene's grids being byte-identical before and after a frame is captured, and the
diff itself: a character difference naming its row, column, and both characters with the caret under
the named cell; a style-only difference naming both semantic roles and both rendition sets; and a
geometry difference refused before any cell is compared. The workspace goldens were confirmed
non-vacuous by recoloring the focused pane header's title and watching both tiers fail.

M9-11 adds the tab row's complete-row goldens on top of that harness. `FrameMatrix::only_row` takes
one row of a live composed frame as a frame of its own, which keeps a row golden a *complete*
expectation while letting the eight documented widths be asserted without re-authoring the pane and
status rows beneath each one. `TAB_ROW_LADDER` is those eight widths — 60, 40, 32, 24, 16, 12, 8, and
5 cells over a three-tab, two-pane, one-client workspace — checked against both a truecolor Storm
theme and its 16-color resolution, and read top to bottom it is the style guide's yield order. The
scene's panes are headerless and real, so the pane count on the row comes from the composed layout
rather than from a number the fixture asserted about itself. The ladder was confirmed non-vacuous by
dropping `UNDERLINE` from the active chip and watching both tiers fail on the attribute. Unit
coverage in `chrome.rs` adds the badge-on-accent and raised-chip treatments, metadata yielding ahead
of any tab, an unpublished session name or client count omitted rather than invented, and the
characters of the row being identical under Storm truecolor, Storm 16-color, and terminal
inheritance — which is what proves the `>` marker and the lower-edge treatment do not depend on
color. `attach.rs` proves the wiring: before any `WorkspaceStatus` the row has no badge and no
client segment, and after one it names the daemon's session and client count.

Golden updates require an intentional review against
[`STYLEGUIDE.md`](STYLEGUIDE.md#acceptance-contract-and-delivery-state); regenerating expected
frames until a failing implementation passes is not an acceptance process. Font rasterization,
rounded corners, and shadows are outside cell tests. Before M9 is declared complete, capture the
reference states manually in a truecolor terminal and confirm that the documented cell adaptation
is recognizably equivalent to the handoff.

---

## What Is Covered

**Every crate in the workspace, including the binary.** The workspace run covers unit,
integration, and doctest surfaces across all six crates. This section grows as tasks land.

Covered today in `cloo-core`, all as unit tests:

- Every layout operation, table-driven: split on both axes, nested and mixed-axis splits, close
  and parent collapse at every depth, ratio-based resize, and the flattened layout pass.
- Rectangles tiling their area exactly, asserted on an odd-sized area so rounding is exercised.
- Every rejection path leaving the layout unchanged, compared structurally against a clone taken
  before the call: minimum-size violations, zero-size areas, extreme ratios, non-finite ratios,
  unknown panes, duplicate panes, and closing the last pane.
- A shrunken area squeezing panes to a one-cell floor rather than dropping them, and a zero-size
  area resolving without a panic.
- Copy mode and search (M5-01): vim-like cursor motion over retained text, a linear selection
  preserving its anchor, selection extraction without a grid mutation, regex matches and wrapped
  navigation, and an invalid regex returned as a clean error while the prior query remains live.
- Directional focus over a quad and over an asymmetric tree, table-driven: every side from every
  pane, an edge answering `None` rather than wrapping, a diagonal pane never being a neighbour, an
  unknown pane having none, traversal never answering with the pane it started from, and the case a
  structural walk gets wrong — a pane whose tree sibling is a subtree, where only geometry says
  which leaf is actually below it.
- Zoom as a view flag: one pane resolving at the full area with the rest hidden but still in the
  layout, zoom and unzoom leaving the tree equal ratio for ratio at every pane in turn, both
  operations idempotent, a toggle undoing a zoom whichever pane asked, an unknown pane refused with
  nothing changed, closing the zoomed pane unzooming while closing another does not, and a split
  unzooming after being measured against the pane's real geometry rather than the area the zoom
  lent it.
- ID allocators being monotonic, non-reusing, resumable, and saturating at `u64::MAX`.
- Profiles as data: the three built-ins in launcher order, each validating, none carrying an
  adapter, and `codex` reconstructible field for field from the public constructor — the assertion
  that fails the moment a vendor earns a special case. Plus every validation rejection: a profile
  ID outside its alphabet, an over-long or unprintable default name, a command with a NUL or a
  control character, and a recommended minimum below the layout floor.
- Pane metadata: names and labels rejecting control characters and bounded by *characters* rather
  than bytes, a working directory refusing a relative path (including an unexpanded `~`) and a NUL
  while validating a path that certainly does not exist — the assertion that pins validation to
  being pure.
- Attention as state plus provenance: an uninstrumented pane defaulting to `unknown` with no
  source, only `needs_input`/`ready`/`failed` entering the queue, acknowledgment cleared by a
  changed state but *kept* when the same state is re-reported, and only an adapter source reporting
  as advisory. Its wire projection (M2-07) is proved too: an uninstrumented pane crossing as
  `unknown`/`None`/unseen, a projection keeping state, provenance, and acknowledgment together, and
  every state mapping to a distinct wire form.
- Profile configuration parsed from `config.toml` text, with the two failure modes kept apart: a
  malformed document or an unknown key is an error and the caller keeps the defaults, while a
  well-formed profile that does not validate is dropped alone with a warning and its neighbours
  still load. Plus the merge rules — a local profile appended after the built-ins, an override of a
  built-in replacing it *in place*, and a duplicate ID keeping the first definition — and the
  command and size surface: an omitted command meaning the login shell, an explicit empty array
  rejected rather than read as one, arguments kept verbatim so a space is never word-split, a
  recommendation below the layout floor dropping the profile, and a configured profile able to
  rebuild `codex` field for field.
- The tab and session lifecycle (M3-01): a tab as a named layout with a focused pane, its name
  validated exactly as a pane name and focus refusing a pane the layout does not hold. Over that,
  the session lifecycle — create appending a tab and activating it, rename touching only its tab,
  select moving activation without reordering the bar, and close with its defined active-tab
  behaviour: closing the active tab activating its right neighbour, falling back to the new
  rightmost when it was last, and leaving activation alone when some other tab closes. Every
  rejection is proved to change nothing — an unknown tab on rename/select/close, and the last tab
  refused with unknown checked first so a bad ID never masquerades as the last-tab rule.
- The emulator-cell to wire-cell conversion in `grid.rs`: every colour form and rendition flag
  crossing intact, an invisible cursor becoming "nothing to draw" rather than a hidden shape, and
  `HollowBlock` degrading to a block. One assertion compares the two crates' attribute bit values
  directly — it is the tripwire for the duplicated `CellAttrs` layouts drifting apart.
- Named theme data (M4-03): the four stable names each carrying all twelve style-guide tokens,
  their configuration spellings round-tripping, and Storm's reference values matching the style
  guide exactly. Theme choice remains model data; terminal-specific colour resolution stays in the
  client. M9-02 adds the `[visual]` document surface over that data — an absent table proving the
  documented Storm/dimmed/minimal/animated defaults, a full table round-tripping into the typed
  accessors, and a third failure mode beside the other two: an unreadable theme or status *name*
  warns and keeps the whole table at its defaults, because a half-applied appearance is one nobody
  chose. That atomicity is proved non-vacuously — applying any surviving field of a refused table
  fails the fixture.
- Keymap resolution (M4-02): chord spellings parsed and rendered as inverses, so the documentation
  and the parser cannot drift; each invalid spelling refused by its own error, including `S-a`,
  which a terminal could never send, and a literal control character reported without printing it.
  The action vocabulary round-trips over `ACTION_NAMES`, and an action needing typed text has no
  spelling in either direction. Over that, the table: the tmux-shaped defaults under `C-b`, an
  override replacing in place and reporting what it displaced, an addition appended, two keys for
  one action treated as an alias rather than a conflict, a rebound prefix leaving every binding
  alone, and unbinding removing exactly one entry. The `[keys]` document surface is proved beside
  the profiles, with the same two failure modes — a chord written twice is a document error because
  TOML refuses it, while an unspellable chord, an unknown action, or an unusable prefix drops one
  line and keeps the default it would have replaced.

Covered today in `cloo-proto`, all as unit tests:

- Round-trip encode/decode for every `ClientMessage` and `ServerMessage` variant, and for the
  value types they carry, asserting the decode consumes exactly the frame it was given.
- Tab wire values are included in that round-trip matrix: `TabSummary`, the tab-bearing `Hello`,
  every tab `Action`, and a standalone `Tabs` update all survive postcard framing unchanged.
- Back-to-back frames decoding out of a single buffer, which is how a socket reader sees them.
- Partial buffers reading as `Incomplete` at *every* split point rather than as an error.
- An oversized length prefix rejected before allocation, and a corrupt payload surfacing as an
  error rather than a panic.
- Handshake version match and mismatch, including that the mismatch error names both versions
  and tells the user to reattach — the acceptance criterion, asserted on the rendered string.
- Every allowlisted outer-terminal effect, including unavailable graphics, round-tripping without
  any raw OSC/DCS payload type, and the `ServerMessage::Effect` envelope carrying one by pane.
- The M9 status vocabulary in that same matrix: `WorkspaceStatus` and `SessionSummary` both as
  standalone values and inside their messages, an empty name, a zero-client session, and saturated
  counts and uptime; `ConfigReloaded` at its first and largest revision; and `InspectSession`
  round-tripping at a stale version as well as the current one, since a version it cannot carry is
  a version nobody can refuse. A stale inspection is then refused through `check_version` exactly
  as a stale attach is. `SessionSummary` is additionally destructured field by field, so a later
  task widening the inspection reply toward an attach has to change a test to do it.

Covered today in `cloo-term`, all as unit tests, all by feeding known byte sequences and
asserting grid or typed-effect state. This is the seam where an `alacritty_terminal` upgrade will
break things, so this coverage is what makes the pinned dependency safe to bump:

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
- A complete retained-scrollback text read leaving the current display offset untouched, so a
  server-side search cannot move another client's viewport.
- Cursor position under output and absolute positioning, DECTCEM visibility, and DECSCUSR shape.
- OSC title and OSC 52 clipboard-store sequences turning into typed queued effects, an empty title
  normalizing to a reset, and a backend device-attribute reply producing no outer-terminal effect.
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

Split and close are the second set that forks, in `tests/session.rs`, driving the session actor
rather than a bare reactor. Every child there reports its own `stty size` **on demand** — once per
line written to it — rather than on a loop, which is what keeps the assertions non-vacuous: a
looping reporter leaves its old answer on the grid and passes whether or not anything still works,
while an on-demand one can only show a report produced after the split or close under test. Each
assertion is on the *last* non-blank line for the same reason.

- A split putting both panes in the layout, moving focus to the new one, and its child starting at
  the rectangle the layout pass produced rather than at the session's whole area.
- A close collapsing the parent split, moving focus to a pane that still exists, and the survivor's
  child being told it grew back.
- A split with no room refused as `TooSmall`, with the layout, the focus, and the refused pane's
  own PTY all exactly as they were.
- Closing the last pane and closing an unknown pane refused, with the child still running and
  still resizable afterwards.
- A resize divided between both panes, each child driven from the same layout pass the client's
  rectangles came from.

M2-02 adds focus and zoom to that file, against the same on-demand reporters:

- Focus moving left and right across an **uneven** split, so the size a child reports names which
  pane received the keystroke; an edge pane staying put rather than wrapping.
- Zoom giving the focused pane the whole area, its child hearing about it, and unzoom restoring the
  split at the ratio it always had.
- Neither direction restarting a child. That child prints `pid=$$` once at startup and reports on
  demand after, so a pane whose PTY had been torn down and respawned would answer with a different
  pid on a freshly cleared grid. Comparing the line before and after the zoom cycle is the whole
  assertion, and it is the only direct evidence available — the layout knows nothing about
  processes and the session exposes no per-pane child id.
- A split while zoomed unzooming, so the pane it created is visible.

M2-07 adds attention through the session actor to the same file, without a PTY assertion: a
`set_attention` reaching the next snapshot with its provenance intact, `acknowledge_attention`
moving only the seen flag, a re-reported state keeping the acknowledgment while a changed one
clears it — the coalescing rule proved through the actor rather than only in the model — and a
report for a pane that has closed dropped without disturbing the survivor.

M2-08 adds the generic sources against real children: a child that rings the bell and then blocks
reaching `needs_input` with `Bell` provenance, a child exiting `0` reaching `ready` and one exiting
non-zero reaching `failed`, both with `Lifecycle` provenance and the two exit codes proving the
reap distinguishes them, and — the "no screen scraping" rule made concrete — a child printing
`error: waiting for input... done` whose attention stays `unknown`. The bell itself is covered
purely in `cloo-term::Emulator`: a `BEL` byte taken exactly once, several bells coalescing to one,
ordinary text never ringing, and a bell never appearing as an outer-terminal effect.

M5-01 adds copy mode through that same session actor: a retained-scrollback regex and visual
selection are projected in the next snapshot, a cloned handle representing a reattached client
moves the same cursor, and a malformed regex is a clean reply that leaves the prior query intact.
The burst-output attach fixture also proves that an inactive copy surface does not traverse
scrollback on every PTY read.

M5-02 adds the explicit copy to the same file: a selection made over a line that has already
scrolled into history is returned as a typed `ClipboardStore` naming the pane it came from, the
projected `viewport_top` is proved to contain the cursor the server just revealed, the snapshot is
identical before and after so a copy is shown to mutate nothing, and a cleared selection yields
`None` rather than an empty clipboard store.

Both geometry halves were confirmed non-vacuous the way `AGENTS.md` prescribes: breaking the
post-split layout pass fails three of these tests, and the survivor's regrowth has no second path
to pass by. The rollback a failed spawn depends on cannot be reached from here — the session's
program is fixed at spawn — so it is covered where it actually lives, as an exactness property of
`cloo-core::layout`: closing a freshly split pane restores the previous tree ratio for ratio, not
merely the same set of panes.

The `cloo-server` unit tests in `src/pty.rs` are pure by rule: config defaults, the `winsize`
conversion, and error conversion. Nothing that spawns. The same rule applies to `src/socket.rs`,
whose unit tests cover only path resolution and name validation — `resolve_socket_path` takes the
environment as arguments precisely so no test has to mutate the process's own, which would race
across the test harness's threads.

Configuration follows the same seam. `src/config.rs` unit-tests pure path precedence for
`CLOO_CONFIG`, `XDG_CONFIG_HOME`, and the `HOME/.config` fallback. Real file reads and replacement
belong in `tests/config.rs`: a valid replacement takes effect through the same `ConfigManager`
without a process restart, a malformed document is rejected with the old value exactly intact, a
missing file resets safely to built-ins, an invalid entry warns while valid neighbours apply, a
replacement document swaps the `[keys]` and `[visual]` tables in the same assignment as the
profiles, a manager holding no file keeps its supplied value instead of resetting to built-ins, and
a real `SIGHUP` drives that same atomic replacement path.

The daemon half of the reload lives in the same file, driving a real daemon, socket, and child
while speaking the wire protocol directly — `cloo-server` may never name `cloo-client`, and the
server's own half is all these need. A valid `SIGHUP` publishes revision 1, the replaced table is
proved live by launching a profile only the new document names, and the child still answers through
the session actor afterwards, which is how "a reload does not block PTY pumping or damage
publication" is asserted rather than assumed. The refused case sends an invalid document, proves
the previous table still launches, then repairs the file and asserts the *first* revision anyone
sees is still 1 — catching a publication on rejection in either interleaving — and asserts the
diagnostic reached the daemon's caller-supplied sink naming the file. `SIGHUP` is process-wide and
a running daemon owns a listener for it, so every test that raises one holds a shared async lock
for as long as its daemon lives; without it, one test's reload lands in another's frames.

Socket lifecycle behaviour needs a real filesystem, so it lives in `tests/socket.rs`. Each test
binds inside its own uniquely named directory under `$TMPDIR`, so nothing depends on
`XDG_RUNTIME_DIR` and no two tests collide:

- A fresh path binding, creating its directory at `0700`, and accepting a connection.
- A second `bind` on a held socket refused with `AlreadyRunning`, leaving the first daemon's
  socket connectable.
- `Drop` unlinking the socket and freeing the name, while leaving the lock file in place.
- A stale socket from a `SIGKILL`ed daemon — a socket file plus a leftover lock file, with
  nothing listening — cleared and replaced.
- A regular file at the socket path refused as `NotASocket` with its contents intact, which is
  the test that would catch a cleanup that deletes whatever it finds.
- A **symlink** at the socket path refused too, with its target left alone. Following the link
  would report the target's type and the unlink could then reach outside the socket directory.
- A departing daemon leaving a successor's socket at the same path alone, which is what the
  `(device, inode)` check in `Drop` exists for.
- A path with no parent directory refused rather than bound relative to the cwd.

Attach and detach are covered from both directions. The framed transport is unit tested in
`cloo-proto`'s `src/stream.rs` over a `tokio::io::duplex` pipe, which is what makes a frame split
across reads and a peer that dies mid-frame testable without a socket at all: reassembly across
reads, queued frames coming back in order, a clean close between frames reading as `Ok(None)`, a
close *inside* a frame reading as `Truncated`, and an implausible length prefix refused before
anything is read for it. The handshake itself is unit tested the same way in
`cloo-server::conn` and `cloo-client::attach` — matching attach and inspection handshakes accepted,
a version mismatch on either refused with a reason that names both versions and says "reattach", a
first frame that is neither handshake refused, a silent peer treated as a close rather than a
refusal, and the attach snapshot batch ordered geometry-first.

The end-to-end coverage lives in `crates/cloo/tests/attach.rs`, in the **binary** crate rather
than in `cloo-server`. That is not a convenience: it needs both halves of the wire, and
`cloo-server` may never name `cloo-client`, dev-dependency or otherwise. Each test binds its own
socket under `$TMPDIR` and synchronizes by reading the wire until the expected frame arrives,
bounded by a timeout — never by sleeping:

- An attach delivering a `Hello` and a session snapshot that contains what the child had already
  written.
- A detach leaving the child alive — asserted with `kill(pid, 0)` — and a second client
  reattaching to find the same grid, then driving the child to exit and proving it is reaped.
- A client connection dropped without a detach costing the session nothing.
- A client announcing a different protocol version refused with an actionable reason, and the
  session still attachable afterwards.
- A read-only inspection receiving exactly one truthful name/count/uptime summary and EOF, both
  before and during an attachment, with the attached geometry unchanged; stale and malformed
  inspection peers are then refused or closed while an existing client remains usable, and a
  vanished daemon yields a connection failure rather than stale session data.
- Attaching where nothing is listening, and where a `SIGKILL`ed daemon left a socket file behind,
  both reporting "no cloo daemon is listening".
- Two clients attached at once both receiving a shared update, proving neither handshake waits for
  the other client to disconnect.
- A large `yes | head` burst reaching an active client in a bounded number of `Damage` frames
  while an unread client falls behind and later converges on the final grid from a fresh snapshot.
- A child OSC 52 request crossing the emulator, session actor, daemon, and wire as one typed
  `Effect` frame; a capable client with explicit clipboard permission renders it exactly once.
- An opt-in adapter connecting to the control socket beside the session's, reporting an advisory
  state that reaches an attached client as `Attention` with `Adapter(<name>)` provenance, while a
  second adapter the profile never named is refused `NotPermitted` and changes nothing.

The public lifecycle command is covered in `crates/cloo/tests/cli.rs`: a foreground `cloo server
work` binds an isolated named socket without entering raw mode, a second server receives the
existing ownership refusal while `cloo attach work` renders and detaches, and a final attachment
exits the generic shell so the server removes only its own socket path.

M8-01 adds the ordinary entry fixture in that same binary crate. With an isolated default socket
and nothing listening, a bare `cloo` produces a workspace — a drawn frame, a raw terminal, and a
socket that did not exist before — and the prefix detach leaves that socket serving, which is what
proves the daemon is a separate process rather than the client itself. Against a live foreground
`cloo server default`, a bare `cloo` joins it: the socket's `(device, inode)` is unchanged and the
foreground server is still the one waiting, so nothing displaced it. The branch those two share is
unit tested in `src/main.rs`, where `daemon_is_listening` answers true for a bound socket and false
for both a missing path and a socket file whose listener has been dropped — the property that keeps
a killed daemon's leftovers from being mistaken for a workspace.

M8-02 puts the startup races and failures of that entry under the same fixture, still in
`crates/cloo/tests/cli.rs`. Two bare `cloo`s started back to back against one empty socket converge
on **one** workspace: text typed into one client's terminal reaches the other's screen, which can
only happen if both are drawing the same session, and the surviving tab still holds exactly one
pane — asked over the wire, because a daemon that lost the bind race is refused before it launches
anything. A socket a `SIGKILL`ed daemon left behind is recovered rather than treated as live, and
the recovery's own proof is that a client attaches at all: `bind` refuses an existing path, so
serving it means the stale file was unlinked under the lock. An inode is *not* proof of that — a
filesystem may hand the freed number straight back — and a fixture that compares one is unsound.
A regular file and a symlink out of the socket directory are each refused with the terminal left
cooked, the occupied path untouched byte for byte, and no socket left anywhere in the directory;
the refusal names `cloo server default`, since a background daemon's own diagnostics go to
`/dev/null`. The other half of "a failed bootstrap leaves the caller cooked" is an attach that
fails *after* raw mode was entered: a `TERM` of `dumb` is refused by the client's capability
negotiation, the outer terminal comes back, and the daemon that attempt created is an ordinary
workspace the fixture then shuts down. Every wait in the file goes through the bounded `wait_until`,
`wait_for_path`, `wait_for_exit`, or `read_until` helpers, and every test ends by exiting its
session's shell and waiting for the socket to disappear.

One fixture caution belongs with that: a stale-socket precondition is *waited for*, not asserted
once. A test binary is multithreaded, so another thread forking a child between this thread's `bind`
and its `drop` leaves that child holding a duplicate of the listener until it execs, and the socket
answers a connect for exactly that long.

M8-03 covers the first-attach shortcut hints at three layers. `cloo-client/src/chrome.rs` asserts
the widest row byte for byte, that the clues yield from the end — `help ?`, then `stack "`, then
`split %` — while `session:7 >2 build 0!` is still at its widest at every rung, that a second pane
withdraws them, that a configured `M-Space` is drawn verbatim and leaves `e` as the four-cell
marker, and that a pending prefix is bracketed as well as accented; a loop over every width from 0
to 60 keeps the row exactly its width and pure ASCII with the clues and the pending brackets both
on. `cloo-client/src/renderer.rs` renders that same row through a terminal with no truecolor and
asserts the visible text — escape sequences stripped, because a styled field is split across SGR
sequences and a byte-window search cannot tell missing text from a mid-field colour change — still
carries `[M-a] split % stack " help ?`. `cloo-client/src/attach.rs` drives the live client state: a
one-pane frame offers the configured chord and never the default, a second pane returns the row to
its ordinary summary, and setting the router's pending flag changes what the composed frame draws.
`crates/cloo/tests/cli.rs` closes the loop over a real pseudoterminal — the first frame a bare
`cloo` draws carries the clue words — and that assertion was confirmed non-vacuous by making
`PrefixHint::is_guided` answer `false` and watching it fail.

M8-04 proves that routing change where it is decided, in `cloo-client/src/attach.rs`: `?` opens the
prefix surface and no longer pane details, and `i` still opens details. M9-17 keeps those two
fixtures against the command palette and replaces the third — an open palette now consumes *typed*
bytes rather than navigation keys, so the fixture types a query and asserts the query. A further
fixture opens it over a client whose prefix is `M-a` and reads the *composed frame* back, so the
title and the split, detach, and add-pane rows are asserted as drawn rather than as modelled.

M8-06 extends the same file with the launcher's client half: `a` opens a launcher listing exactly
the profiles the client was given, confirming a row yields the launch the caller must send, an
empty launcher consumes the confirmation without naming anything, and a dismissal leaves neither an
overlay nor a notice behind. Two more cover the outcome the wire cannot report — a launch whose
deadline passes becomes a refusal that reaches the *composed frame* and then clears itself, and only
a pane the client had not already seen, carrying the profile it named, retires the notice. The
end-to-end half lives in `crates/cloo/tests/cli.rs` over a real pseudoterminal: `echo a-is-just-text`
typed for the shell creates no pane, `<prefix> a` then `G` then Enter creates one whose reported
profile is the `CLOO_CONFIG` profile that was confirmed, no overlay key reaches the shell, and the
terminal is cooked again after the detach. Removing the `ADD_PANE_KEY` arm and removing the
`send_command` were each confirmed to fail it.

M9-07 covers the attached workspace projection over the real socket/PTY boundary in
`crates/cloo/tests/attach.rs`. One named daemon receives a wide client, then a larger peer: each
gets an initial `WorkspaceStatus`, while the first receives the join update with a truthful count
and unchanged minimum. A private resize that remains above the minimum is bounded and asserted to
emit no duplicate status. When the peer becomes narrower, both clients receive the new effective
size, and the sending client's cached size is asserted to remain at the daemon's preceding answer
until that frame arrives. Detaching the peer restores the survivor's count and effective size. The
same helper checks after every frame that `Attached` replaced its cached status rather than deriving
either field from a pane grid.

M9-09 covers local session discovery at both client seams. `cloo-client::attach` uses a duplex
transport to assert the exact first frame is `InspectSession` at the current protocol version with
no size, capabilities, or attachment fields, and rejects both a `Hello` and a `Damage` reply rather
than becoming an attach or grid reader. `cloo-client::session_catalog` unit-tests the
`XDG_RUNTIME_DIR/cloo` and per-uid `/tmp` directory rules. The end-to-end fixtures in
`crates/cloo/tests/attach.rs` place two real named daemons beside a regular file, socket symlink,
stale socket, and listener that accepts the request but never answers. Only the daemon summaries
become rows, ordered by their reported names rather than deliberately opposite filenames, both
report zero clients after inspection, and the silent peer's private 50ms test deadline keeps the
whole discovery inside the fixture deadline. A second fixture gives the catalog an explicit socket
override outside the runtime directory and proves it returns that one verified session while
ignoring a live daemon inside the directory.

Resize is covered there too, as of M1-03, and it is the one case where a single assertion would
be worthless. A resize is two things — the grid reflows and the child is told through
`TIOCSWINSZ` — and a test that checked only one would pass with the other missing. So both halves
are asserted from the same client:

- The **grid** half, by waiting for a `Damage` frame whose rows are the new width. Only a
  reflowed emulator produces those.
- The **PTY** half, by scripting the child to run `stty size` on demand and asserting on what it
  prints. Nothing but a `TIOCSWINSZ` on that pty's master can change that answer.

Both were confirmed non-vacuous by breaking each half of `PtyReactor::resize` in turn and watching
the test fail. A degenerate resize — zero rows, which real terminals report mid-drag — has its own
test asserting the child is still alive and still at its old geometry.

The reconnect/resize race is hardened in M7-01 with two more fixtures in the same file, aimed at
the multi-client minimum-size negotiation rather than the single-client `TIOCSWINSZ` path:

- A narrower client joins, dragging the session down so the already-attached survivor's grid
  reflows to the smaller width; the narrow client then *detaches*, and the survivor must receive a
  full-width redraw rather than a cache left stuck at the narrow width. Both directions are
  asserted by waiting for a `Damage` row exactly the expected width — 40 cells, then 80.
- Two clients attached at different sizes both converge on the negotiated minimum width, which is
  the "two clients stay visually consistent" success criterion made assertable: a 50-cell row must
  reach *both*, not each client drawing its own size.

True-colour detection is `cloo-client::capabilities::truecolor_from_env`, a pure two-argument
function unit-tested directly (M7-01): each standard `COLORTERM` value in any case and a `*-direct`
`TERM` entry establish it, while a `256color`-only terminal and an unrelated `COLORTERM` value do
not — the ambiguous case answers `false`, because a wrongly claimed truecolor corrupts the screen.

Input routing, as of M1-07, is covered at three levels because the property spans all three. The
*decoder* is unit tested in `cloo-client::input`: one fixture per negotiated mode's request and
its matching reset, one per mouse report kind, sequences split across reads held rather than
mis-decoded, a lone Escape released only by a flush, and — the one that would otherwise pass
vacuously — a sequence for a mode that was never requested passing through as ordinary keys. The
*encoders* are unit tested in `cloo-server::session`, in the same shape: one fixture per mouse
event kind at the tracking level that asks for it and silence at the level below, a paste
bracketed only for a child that enabled it, and a paste carrying a paste terminator inside it
coming out with exactly one terminator at the end.

The end-to-end half is in `crates/cloo/tests/attach.rs`, and it is what proves the two agree. The
scripted child enables a mode with its own escape sequence, and cloo's answer arrives back through
`ServerMessage::Modes`; the child then reads a fixed number of bytes and prints them with the
escape byte stripped, so the encoding is assertable as text on the grid. The negative test is the
one worth keeping honest: a child that enabled neither focus reporting nor mouse tracking is sent
both and then four typed bytes, and it must read exactly those four. Those children run under
`stty -echo -icanon` — without `-icanon` a report with no newline in it is never delivered at all,
and the test hangs rather than failing.

Mouse *ownership* (M6-01) is covered at the two ends that can each get it wrong on their own. The
client half is pure and unit tested in `cloo-client::input`: one screen with a tab row, two panes
either side of a gutter, headers, and a status bar is hit-tested at every region at once, and the
two ordering rules are asserted against deliberately wrong layouts — a pane described as reaching
over the status row must not swallow it, and a header must not swallow a cell some pane's grid
occupies. Routing is then asserted as a property rather than case by case: with the most permissive
application state there is (full motion tracking in the focused pane), *no* chrome region produces
a wire event, which is the whole of "a chrome event never reaches the wire." A click in an
unfocused pane and a shift-held click both route to `ChromeTarget::PaneBody` naming their pane,
because that is what click-to-focus will be made of.

The server half is in `cloo-server/tests/session.rs` against real children, and its assertion is
deliberately on the *wrong text* rather than on a hang: two panes each read a fixed byte count and
echo it with the escape byte stripped, so a report delivered to the wrong pane shows up as `[<0`
where `done` was expected. Three fixtures — an event naming an unfocused pane reaching that pane
and not the focused one, a pane that never enabled the mouse being written nothing while its
neighbour tracks, and an event naming a closed pane dropped rather than redirected. All three were
confirmed non-vacuous by reverting `deliver_mouse` to the naive implementation — write to the
focused pane, with no visibility check — and watching each fail. Reverting only the pane lookup is
*not* enough for the closed-pane fixture: the visibility check drops the event first, so that
fixture passes against a half-reverted implementation and proves nothing.

Mouse *actions* (M6-02) are covered at three layers, because "a drag changes ratios only" is a
property each of them can break alone. `cloo-core::layout` asserts it on the tree itself: a resize
is compared against the tree's *shape* with every ratio erased, as well as against the rectangles it
resolves into, so a drag that reshaped anything fails even if the cells came out right. The clamp is
tested from both ends — a drag of 500 cells stops at `MIN_PANE_SIZE` on either side rather than
being refused — and an extent too small to divide is asserted to leave the ratio untouched, since a
ratio invented for a one-column area would survive into the resize that gives the split room again.
`cloo-client::input` covers the client half over a screen with both kinds of divider at once, a
gutter column and a header row between stacked panes: every command a drag produces is a resize and
nothing else, the deltas are relative (`[2, 1, -4]` for a pointer walking `10 → 12 → 13 → 13 → 9`,
with the motion that did not move a cell commanding nothing), a press on a divider commands nothing
at all, and motion after the release belongs to nobody. The keyboard-equivalence criterion is a test
rather than a comment: `keymap.rs` asserts `FocusPane` and `ResizePane` have no spelling in either
direction *and* that `focus-left` and its three siblings are still bindable, and
`ChromeAction::commands` is asserted to return exactly the copy-mode actions the keyboard sends.

`cloo-server/tests/session.rs` closes it against real children. A drag is asserted on both halves —
the rectangles move by the cells asked for and the neighbour gives up exactly that much, and the
same child is still running in each pane afterwards, proved by the `pid=$$` line it printed at
startup — so a drag that restarted anything fails even though the geometry would look right. A
click naming a pane that never existed, and one naming a pane a zoom is hiding, are both asserted to
be *dropped*: focus stays where it was. The end-to-end fixture in `crates/cloo/tests/attach.rs`
drives the wheel over a real socket, from the client's own hit test through `ChromeAction::commands`
to copy state coming back with the cursor exactly `WHEEL_LINES` above where entering copy mode put
it. It sends the commands in two halves only so it has a baseline to measure against — the server
coalesces copy-state frames, so a client that sent all of them at once would see one frame and have
nothing to compare it to.

The layout *commands* those gestures ride on (M6-04) are covered end to end in
`crates/cloo/tests/attach.rs`: `split_zoom_and_close_from_a_client_move_the_reported_layout` sends
`Action::SplitVertical`, `ToggleZoom` twice, and `ClosePane` over a real socket and reads the
`ServerMessage::Layout` frame each produces. A split moves the reported geometry from one pane to
two with the new pane focused; a zoom resolves to that pane alone with `zoomed` set and the tree
still two panes behind it; unzoom brings the second pane back; and a close — naming no pane — drops
the focused one and falls focus back to the survivor. The fixture matches on a layout *predicate*
rather than a fixed frame count, because the damage tracker sends a `Layout` only when the geometry
changed. It is what proves the daemon's catch-all no longer swallows a layout command; reverting the
four new match arms to the catch-all fails it.

The typed profile launch request (M8-05) is covered at three layers, one per thing that could be
wrong. `cloo-proto/src/frame.rs` round-trips `Action::LaunchProfile` — including an empty
identifier — through the same every-message list every other variant is in, so the wire form is
covered by the version bump's own fixture rather than a new one. `cloo-server/src/launch.rs` covers
resolution purely: a configured identifier becomes that profile's launch under the profile's own
default name, with the caller's directory and no invented task, while an unknown ID, an empty one,
and a *command line* are each a `LaunchError::Unknown` — the last of those is the whole property,
since a string that looks like a command is only ever looked up. `cloo-server/tests/session.rs`
proves the same three answers against real children: a profile from a parsed `config.toml` reaches a
pane whose metadata names it and whose child's own `pwd` is the directory it was given, an
identifier the document does not name leaves one pane with the root still answering `stty size`, and
a configured profile whose program is not on `PATH` fails as `PaneError::Spawn` with the layout
already rolled back. Those documents are *parsed*, not assembled, because that is the only way a
profile reaches a running server.

`crates/cloo/tests/attach.rs` closes the loop over a real socket:
`a_typed_launch_request_creates_a_pane_from_the_servers_own_profile_table` sends three refused
identifiers *first* — an unknown one, an empty one, and a shell command — and then the configured
one, so a pane wrongly created by any of them is still counted when the fixture asserts exactly two
panes at the moment the `harness` pane appears. It was confirmed non-vacuous by resolving against
`Config::defaults()` instead of the daemon's own table and watching it time out.

Two of the M6-01 fixtures are also what found the stall M6-03 fixed, and `session.rs` now carries the
regression coverage. `a_snapshot_is_answered_after_every_child_has_exited` runs a child that prints
one line and exits — output *and then* an exit, which is the whole reproduction, since the bytes
queue the coalesced `Output` level and the exit is the event that used to be awaited into a channel
already holding it. It never drains `SpawnedSession::events`, which is not an artificial condition:
it is what a daemon looks like for as long as it is inside its own `publish_current`. A child that
exits silently leaves the channel empty and never stalls, which is why the M2-08 lifecycle fixtures
passed throughout and why this one does not use `exit 0`.
`output_that_nobody_drains_never_stalls_the_actor` covers the general case with sixty-four
undeliverable notifications and then asserts a resize still applies.

Every snapshot in that file goes through a `snapshot_now` helper that wraps
`SessionHandle::snapshot` in the same 20-second deadline the polling waits use. This is the rule
worth carrying to any future actor fixture: **a test must never await an actor reply without a
timeout.** A wedged actor is a real failure mode, and an unbounded await turns it into a
`cargo test --workspace` that never returns instead of a test that fails and names the problem —
which is precisely what happened between M6-01 and M6-03. Both stall fixtures were confirmed
non-vacuous by restoring the awaited `Exited` send and watching them fail on that deadline.

Copy-mode rendering (M5-02) is covered the same way, at both ends. The *client* half is pure and
unit tested in `cloo-client::copy_mode`: spans are built from the grid cache and the cache is
compared against a clone taken before the call, because "a selection does not mutate the grid" is
exactly the property a lazy implementation breaks. The end-to-end fixture in
`crates/cloo/tests/attach.rs` drives copy mode over the wire, applies every damage frame into a
real client `Grid` — starting with the attach snapshot, or the cache is empty and the highlight
assertion passes on blanks — and then asserts the selected text is what the highlight covers and
that the copy's OSC 52 bytes are written under a permitting policy and not written at all under
the default one.

The `SIGWINCH` end of the same path is covered from `crates/cloo/tests/cli.rs`, because the signal
has to be delivered to a *process*: the test resizes the outer pseudoterminal, sends the real
binary a `SIGWINCH`, and asserts the inner child's `stty size` reports the new geometry. That is
the whole chain — signal, `TIOCGWINSZ`, resize command, layout pass, grid, `TIOCSWINSZ` — in one
assertion. `read_until` polls with the time actually remaining rather than reading blindly, so a
terminal that goes quiet (exactly what a broken resize looks like) fails at the timeout instead of
hanging the suite.

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
- Incremental row damage repainting only its named row and never emitting a full-screen clear.
- Row updates rejected out of range and at the wrong width, each compared against a clone taken
  before the call to prove the grid is unchanged.
- Resize keeping the overlapping cells and blanking the rest, a zero-sized grid rendering without
  a panic, and multi-byte characters surviving the render intact.

Pane chrome joined the renderer at M2-03 and is tested the same way, because it is also a pure
function — from a pane description into cells, with no bytes and no descriptor. `src/chrome.rs`
covers:

- Focus and attention as independent signals: a focused quiet pane and an unfocused pane needing
  input differ in both axes, and focus restyles the title without touching the state glyph.
- Every state having a distinct ASCII glyph and a label, and both appearing in a wide header — the
  assertion that colour is never the only signal.
- The width ladder, asserted against exact strings: the task label dropped first, then the state's
  text label, then the title truncated, then the glyph standing alone. A header is exactly the pane
  width at *every* size from 0 to 60, which is what catches an off-by-one in the gap arithmetic.
- The no-dim fallback leaving an unfocused header at full contrast while its text is unchanged, a
  focused header never dimmed, and a dimmed `needs input` still distinguishable from a dimmed
  `quiet` — the property that fails the moment dimming stops preserving hue.
- Dimming a 24-bit cell by blending rather than by stacking `DIM`, and a palette index dimming with
  the attribute rather than a guess at the user's colour.
- The compact tab row retaining tab-bar order and a text `>` active marker, then yielding inactive
  tabs before truncating the active title; its positioned span retains the caller's origin.

M2-10 adds the attention summary, queue, and toast deck to the same file, tested as pure functions
into cells and into deterministic model state:

- The queue's ordering and coalescing, which are what "deterministic" means here: only the three
  actionable states enter, entries list newest-first, a repeat of the same live state coalesces
  without churning the order, a changed state moves its pane to the front, an acknowledged state
  cannot refill the queue while a genuinely different one alerts again, and a pane returning to a
  quiet state resets the slate so its next real event is heard.
- Keyboard navigation and the focus/acknowledge actions: the cursor walking and clamping, the focus
  target following the selection, and acknowledging the selected entry removing exactly it.
- The status-bar summary tallying each present state with its glyph and colour in a fixed urgency
  order, and every actionable state rendering text, glyph, and colour in a queue row that is exactly
  the width at every size — reusing the header's degradation ladder.
- The toast deck being bounded (the oldest evicted at capacity, and a zero request still holding
  one) and coalescing per pane (a repeat becoming one notice with a growing count, moved to newest),
  plus a toast line carrying text, glyph, colour, and a `(xN)` count only when it repeated.

M9-16 extends the deck in the same file with its clock and its placement, all with time as an
argument: a notice expiring exactly on its deadline and not a millisecond before, a refresh
restarting that deadline rather than the first raise owning it, a closed pane position taking its
notice with it, an entrance advancing at most `MOTION_STEPS` frames however often it is sampled and
settling into cells the chrome would draw anyway, reduce-motion asking for no entrance frame at all,
and a motion-preference change keeping the stack rather than dismissing it. Placement is asserted as
a pure function: rows between the tab and status rows, the focused cursor's row skipped, an empty
answer on a frame with no room, right alignment inside the margin, and a narrow frame truncating
rather than overflowing. A 16-colour toast is asserted to draw the same characters as the truecolour
one, which is the property that fails if a fallback ever carries meaning in colour alone.

`src/input.rs` gains the queue's keyboard vocabulary: the conventional bindings mapping to `Next`,
`Prev`, `Focus`, `Acknowledge`, and `Dismiss`, and an unbound key mapping to nothing.

M3-04 adds the keyboard-first overlays in `src/overlay.rs`, tested as one model and one renderer
rather than three of each:

- **Dismissal from every state.** Every overlay — including an empty switcher and an empty
  launcher — answers `Dismissed` to `Dismiss`, driven through `input::overlay_action(b"\x1b")` so
  the binding and the model are asserted together. This is the fixture that fails the moment a
  surface can trap the terminal.
- **Navigation.** The cursor walks and clamps at both ends, `First`/`Last` jump, and an empty
  overlay has nowhere to go and confirms to nothing at all — a launcher with no configured profile
  must not invent one.
- **Explicit profiles only.** A confirmed launcher row yields a `LaunchRequest` whose profile is
  one the caller supplied, and a `Profile` that fails its own `validate` never becomes a row: the
  fixture builds a deliberately invalid profile beside a valid one and asserts only the valid one is
  offered. Reverting the validation filter fails exactly that test.
- **The launch notice, as of M8-06.** A `LaunchNotice` can only be built from a `LaunchRequest`, so
  it names a profile some launcher row named; it says both states in words, settles from waiting to
  refused exactly once at `LAUNCH_DEADLINE`, finishes after `NOTICE_LINGER`, and is answered only by
  a pane the client had not seen carrying the profile it asked for — neither an existing pane of the
  same profile nor a new pane of another one. Its row takes the same exact-width ladder as every
  other, pinned to a literal string at width 40.
- **Pane details are what the server said.** The field list comes from `PaneInfo` plus the reported
  attention, and a task the user never set is absent rather than blank.
- **The shared width ladder.** Every row of every overlay is exactly the width asked for at every
  width from 0 to 60, and the box is exactly as tall as it was asked for from 0 to 10 rows. Exact
  strings pin the yield order — extras from the end, then the title truncates — the dismissal hint
  as the last hint standing, the selected row's `>` marker as text rather than colour alone, and a
  box too short for its list keeping the title and the hints.
- **The backdrop** dims through the same `dim_cell_with_theme` an unfocused pane takes, changing
  rendition and never a character.

`src/input.rs` gains the matching overlay vocabulary: `j`/`k` and the arrows, `g`/`G` and Home/End,
Enter, and Escape or `q`, with an unbound key mapping to nothing.

M9-19 adds active resize coverage at all client seams under names containing `resize`. `keymap.rs`
asserts each default arrow resolves to its side, `h/j/k/l` do not, and an override or unbind wins.
`input.rs` resolves a focused edge through the same divider geometry mouse dragging uses, proves an
outer edge has no answer, checks both divider bands and their visible ratios, and routes a prefixed
arrow as resize while `h` remains `FocusLeft`. `chrome.rs` pins the accent-and-bold divider glyphs,
the `resize · ratio 0.62` label, and the identical text with indexed 16-colour roles. `attach.rs`
drives keyboard and mouse activity through `LiveState`, applies a 0.50-to-0.62 layout answer, proves
only that divider is lit, and checks mouse release and the 750ms keyboard deadline. The binary-crate
fixture uses a real daemon, split PTYs, outer PTY, and `TERM=vt100`: a prefixed arrow draws the 0.49
result without SGR mouse support, then prefix detach restores the terminal.

M8-04 adds the prefix surface to that same file as a fourth kind of the one model — it joins the
dismissal and width-ladder loops rather than getting its own — plus four fixtures for the property
that it is *read from the keymap*: a rebound prefix and a rebound chord are shown verbatim, an
action the user unbound has no row at all, a keymap that binds a client chord to a real action takes
that row away while leaving its neighbours, and each row names the `[keys]` action to write when
rebinding it (`client` where there is no such action). A hand-written table of the defaults passes
none of the first three. The 16-colour contract is a loop over every drawn cell asserting ASCII, with
the chord column bold as well as accented, and the exact-string ladder pins the note as the field
that yields first. M9-17 carries all of it onto the command palette unchanged.

M4-02 adds the keyboard's half of the ownership question to `src/input.rs`, in two layers. First
`decode_key`, with one fixture per encoding a terminal actually sends — control letters, `M-x`,
`C-M-b`, `CSI Z`, `SS3` arrows, the `;modifier` parameter forms, numbered editing and function keys
— each asserted against the spelling `cloo-core` would parse, which is the join between the two
crates. A sequence cloo does not model, and half of one, answer `None` rather than a guess.

Then `KeyRouter`, whose fixtures are all one property: **nothing is consumed outside a pending
prefix.** Every default-bound chord fed without the prefix comes back as `Pane` with exactly its own
bytes, and the fixture asserts the chord *is* bound first so it cannot pass vacuously. Around that:
ordinary typing passed through byte for byte, a prefix and its chord arriving in one read, typing
around a command keeping its order and its bytes, an unbound or undecodable chord after the prefix
consumed rather than typed at a shell, the prefix twice sending itself to the child, a rebound
prefix handing `C-b` back to the pane, and a reset forgetting a pending prefix. Reverting the router
to one that looks a chord up without the prefix fails five of them.

`src/renderer.rs` gained the positioned `Span` that chrome is painted from: a span drawn at its own
origin, each span restating its style absolutely so a second one cannot inherit the first's, an
empty span moving nothing, and spans never clearing the outer terminal.

M4-03 adds pure theme resolution in `cloo-client::theme`: every named palette resolves each
style-guide role deterministically, non-truecolor paths choose explicit ANSI entries below 16, and
terminal-palette inheritance leaves default foreground/background alone while retaining distinct
semantic colours. A chrome-and-renderer fixture proves a focused `>` and `needs input` `!` remain
different ANSI colours and textual signals without truecolor; it asserts no RGB SGR is emitted.

M4-04 adds the transition model in `src/motion.rs`, tested without sleeping: time is passed in, so
a whole 120ms transition is stepped frame by frame from one `Instant`. The fixtures are the two
properties the acceptance criteria name, plus the arithmetic that keeps them honest:

- **Interruption settles, never rewinds.** One fixture per motion kind interrupts mid-ramp and
  asserts the returned phase is the *end* state, that nothing is left in flight, and that a later
  tick asks for no further frame. A second fixture closes the loop through the cells: a mid-frame
  cell is visibly different, and the interrupted frame is the chrome's own cells unchanged.
  Reverting `interrupt` to return the step it had reached fails both.
- **Motion cannot become a per-read repaint.** A thousand samples inside one frame budget produce
  no frame at all, and a thousand samples spread across a whole transition — eighty times the
  budget's rate, which is what a large `cat` looks like — produce at most `MOTION_STEPS + 1`.
  Deleting the already-drawn-step check fails both.
- **The budget itself.** Seven whole 16ms frames fit inside 120ms and an eighth would not, and the
  frame budget is asserted to be the render loop's own.
- **Reduce-motion.** Every kind starts already settled, nothing is left active, a tick answers
  nothing, and the painted cell is the chrome's own — one frame for a layout change, exactly what a
  client with no motion at all would draw.
- **The ramp.** Every step keeps its character, its attributes, and a foreground that is never the
  frame colour, so an interruption at any step leaves readable text; the distance to the chrome's
  own colour closes monotonically and lands exactly on it. A palette index or the terminal's own
  default takes the `DIM` attribute instead of an invented colour, and drops it again when settled.
  A span keeps its origin, because motion ramps contrast and never moves chrome.

`src/renderer.rs` gains the transition frame those phases are painted through: a settled phase
produces bytes *byte-identical* to an ordinary `render_spans` frame, a mid-transition frame keeps
its characters while the accent has not landed yet, and a transition frame never clears the outer
terminal — motion paints chrome, never a pane's contents.

M3-03 adds the always-on minimal status row through the same pure chrome-and-renderer seam:

- A wide row carries its session, active one-based tab and title, per-state actionable tally, and
  `C-b ?` hint; the active marker and tally glyphs are asserted as text as well as colour.
- Narrow rows follow the fixed yield order rather than dropping fields opportunistically: a
  12-cell row keeps `s7 >2 3! C-b`, and the four-cell `s>!b` form retains one ASCII marker for
  every required field. An empty queue explicitly says `0!`.
- A renderer with `truecolor` disabled paints the same status row without any 24-bit SGR while its
  session, active-tab, attention, and prefix strings remain visible, covering the terminal-safe
  colour and ASCII fallback together.

M6-05 adds `compose_frame` to `src/renderer.rs`, tested through the same pure span seam:

- A two-pane frame lays every visible pane's grid into its own `PaneArea`, byte-exact: the tab row
  owns row zero and the status row the last row, each pane's header fills its top frame between
  corners, side and bottom edges remain visible without overlap, and a focused body drops in
  undimmed, row by row, equal to the cached grid at exactly its rect origin — the positions are the
  resolved layout's, never a guessed offset.
- An unfocused body recedes while a focused one is untouched, so `chrome::body_span` is the same
  one-place dimming policy `dim_cells` is; a headerless area composes no header row; and a
  zero-sized frame composes nothing rather than panicking.
- Rendering a composed frame through `render_spans` lands the grid's cells at the pane's rect origin
  in outer-terminal coordinates, tying the composition to byte-exact output.

M9-10 closes the geometry half against live state and a real PTY. `input.rs` hit-tests the body,
top/header, both side edges, bottom edge, adjacent frame-gutter band, fixed chrome rows, and outside
cells from the same `PaneArea` the renderer consumes; every chrome cell is proved unable to produce
a wire mouse event, while a body event keeps pane-local coordinates. The card-01 visual golden now
contains all four frame edges in truecolor and 16-color. `crates/cloo/tests/attach.rs` resizes a
daemon-owned session and asserts both damage rows and the child's own `stty size` are two columns
and two rows smaller than the pane allocation, proving frame cells never enter the PTY. The local
one-pane `SIGWINCH` fixture still reports the complete outer size, proving the no-chrome path was
not accidentally inset.

M6-07 layers the remaining client visual state onto that live-frame seam:

- A `CopyMode` update is retained alongside the pane grid, projected into highlight spans over the
  body's exact origin, and replaces the ordinary bottom status span with the copy-mode status row.
  The cache remains the source of every highlighted character.
- An open overlay first dims the already-composed frame and then draws its raised box above it;
  navigation is consumed by the overlay model rather than routed to a child. The session and
  focused-pane-details entries are exercised through their client-local prefix chords.
- The live loop starts motion only from a changed resolved layout and settles it before input or a
  resize. `Renderer::render_layered_transition` keeps pane-body and copy-highlight spans ordinary
  while ramping chrome, with a settled layered frame byte-identical to ordinary span rendering.

M9-15 adds the live attention queue at both seams, under names carrying `attention_queue` so the
whole surface runs as one filter.

In `src/overlay.rs` the queue joins the shared dismissal and width-ladder loops as a fifth kind, plus
fixtures for what is only true of it: focus and acknowledge answer `FocusPane`/`Acknowledge` naming
the row's `PaneId` — a second fixture gives a different pane the position the first fixture's row
held, which is what fails an implementation that acts on a queue index — an empty queue acts on
nothing at all, a refresh keeps the cursor on its *pane* across a removed row and clamps onto a
shorter list, refreshing leaves another overlay's rows alone, and `!` is offered by the command
palette only while the keymap leaves it free. Legibility is asserted at all four states the milestone names:
every drawn cell ASCII and the state colour resolved through the client theme in both truecolor and
16-colour, the exact-string width ladder at 32/20/8/4 columns, a narrow box still saying `esc close`,
and an empty queue rendering a titled, closable box with no position claim.

In `src/attach.rs`, `LiveState` fixtures build the queue from real `Panes` and `Attention`
projections: a `working` pane never becomes a row, `!` opens the surface with one row per waiting
pane newest-first, the overlay consumes navigation while answering only
`Action::FocusPane`/`Action::AcknowledgeAttention`, focus closes it and acknowledgment does not, and
an acknowledged row survives the keypress and leaves only on the projection that reports it — the
fixture that fails any client-local dismissal. `crates/cloo/tests/attach.rs` closes the wire half
against a real daemon and PTY: a bell reaches `needs_input` unacknowledged, the client sends
`Action::AcknowledgeAttention`, and *both* the sending client and a second attached client are told
the pane is acknowledged with its state unchanged. Removing the daemon's dispatch arm fails it.

M9-16 adds the live toast stack to `src/attach.rs` under names carrying `toast`, driven from a
nested-layout `LiveState` — two stacked panes beside a tall one — so the floating stack is asserted
over the geometry it actually has to survive. The fixtures cover the diff that turns wire *state*
into an *event*: one actionable projection raises exactly one notice, an identical projection resent
leaves the repeat count alone, a pane raising an event again coalesces into one notice with a
growing count, a changed state refreshes it in place, four events leave exactly `TOAST_CAPACITY`
with the oldest evicted, an acknowledgment retires a notice while a pane settling to quiet leaves it
to its deadline, and a closing pane takes its notice with it. Five hundred `Damage` messages raise
nothing and change nothing, and a thousand ticks inside one transition draw at most `MOTION_STEPS`
frames — the pair that fails the moment a notice starts riding a pane's output clock. The composed
frame is asserted to place every notice below the tab row, above the status row, off the focused
cursor's row, and against the right edge, with each one flagged client chrome rather than pane
content. The 16-colour client draws the same characters as the truecolour one with indexed rather
than invented colour, and every overlay key offered to a client showing toasts is `Ignored`, which
is what proves the stack owns no keyboard and leaks nothing to a pane.

M9-17 turns the help surface into the command palette, and its fixtures live under names carrying
`command_palette` so `cargo test -p cloo-client command_palette` is the task's own suite. The
keymap-derived properties M8-04 established are kept verbatim against the new type — the effective
prefix in the title, every promised control reachable, a rebound prefix and chord verbatim, a keymap
that claims a client chord taking that row away — with one addition that a filtered list makes
possible: an unbound action must not come back through the *search* either. The search loop itself is
asserted end to end in `src/overlay.rs`: typing narrows the list and the title reports the position
among results, backspacing widens it back and an empty query is not a dismissal, terms match in any
order and against the `[keys]` name as well as the label, and case is not a filter. Selection
stability is proved by narrowing away the row *above* the cursor and confirming the command the user
was looking at — the fixture an implementation that clamps by position fails.

Both confirmation arms are asserted as typed values rather than text: a keymap row answers
`RunAction` with the exact `Action` its chord would have sent, and a client row answers
`OpenSurface`. Escape closes and `q` types a `q`, which is the documented departure from the shared
overlay vocabulary; `src/input.rs` pins the rest of it — arrows and `C-n`/`C-p` for navigation,
backspace and `C-u` for editing, everything printable including space as `Insert`, and
`palette_actions` decoding a coalesced run byte by byte with a three-byte arrow inside it winning
over the Escape it starts with. A byte the palette cannot use is dropped rather than passed on.
Rendering is checked at the four surfaces card 04 names: ASCII cells and a bold chord column in both
truecolor and 16-colour, an exact query-row string with its own width ladder from 0 to 60, a
narrow box keeping title, query, and dismissal hint in that order of importance, and a query that
matches nothing drawing `no matches` over an empty list rather than a blank box.

`src/attach.rs` closes the client half: `?` opens the palette rather than pane details, a typed run
of ordinary bytes is consumed locally and becomes the query — the fixture that fails the moment a
search term can reach a pane — Enter hands back the typed `Action`, and confirming a client row
swaps the palette for that surface without touching the wire.

M9-18 connects the session switcher under names carrying `session_switcher`. `src/attach.rs` feeds
two `SessionCatalogEntry` values into `LiveState`, asserts the frame draws only their reported names
and counts with the current socket marked attached, and confirms the other row to its socket path.
A refresh removing the selected daemon keeps the surface open and clamps selection onto the one
verified row left. The card-05 visual fixture captures the complete 40x4 switcher in Storm truecolor
and its 16-color resolution, plus explicit empty and 12-column frames; the golden includes the
selection glyph, attachment word, all three count nouns, and dismissal hint.

The end-to-end half is `crates/cloo/tests/attach.rs`. Two named daemons bind ordinary sockets under
one isolated `XDG_RUNTIME_DIR`; a real outer-PTY client opens the switcher on the first, waits for the
bounded catalog refresh, selects the second, and renders its child without leaving raw mode. A fresh
attachment to the first daemon then sends `safe` and its child reports exactly `main-got=safe`, which
is the non-vacuous assertion that neither `j` nor Enter leaked out of the overlay. Prefix detach
restores the terminal, and both daemon sockets remain connectable afterward.

Typed outer-terminal effects are unit tested in `src/effects.rs`: the policy begins deny-all, a
permitted title and a capable, permitted OSC 52 store produce their exact terminal bytes once,
and an unsupported, unsafe, policy-denied, or capability-denied effect leaves the output buffer
unchanged. Clipboard base64 encoding is checked for every padding shape.

Raw-mode behaviour needs a real tty, so it lives in `crates/cloo-client/tests/raw_mode.rs`, which
opens a pseudoterminal pair and drives the slave side. Three of the four restore paths are
asserted there — the signal path cannot be, since a library test asserting it would have to kill
its own process. It is covered instead from `crates/cloo/tests/cli.rs`, which signals the real
binary as a *child*: `a_terminating_signal_still_hands_the_terminal_back` spawns `cloo` on a
pseudoterminal, waits for the first frame, sends `SIGTERM`, and asserts both that the wait status
carries the signal (the handler re-raises rather than calling `exit`) and that the terminal came
back cooked. **All four restore paths are now asserted automatically.** The library tests cover:

- Entering raw mode actually clearing `ECHO`, `ICANON`, and `ISIG`, and drop restoring the exact
  original flag words — not merely "some cooked state".
- An explicit `restore` reporting success and releasing the global slot, and the following `Drop`
  being a no-op.
- An error unwinding past a live guard, and a panic inside one, both leaving the terminal cooked.
- A second guard refused with `AlreadyActive` while leaving its own terminal untouched, so a
  collision cannot overwrite the first guard's saved state.
- A pipe refused as `NotATerminal`.

Outer-terminal geometry is unit tested in `src/outer.rs`: a degenerate `winsize` falls back to
80x24 rather than rendering into a zero-sized grid.

Capability detection is a pure function of `TERM` and `COLORTERM`, unit tested in
`src/capabilities.rs`: truecolor established only by an explicit signal, capabilities that need a
query-and-reply staying false, an unresolvable `TERM` refusing an *attach* with a message naming
both the fix and the local-pane alternative while the same `TERM` leaves the *local pane* claiming
nothing, and every baseline capability's documented fallback. Two of those tests exist because
they fail loudly rather than vacuously: `every_capability_reads_its_own_field` sets one field at a
time and asserts exactly one capability reads back, which is what catches a `present_in` arm
wired to a neighbouring field, and `a_present_capability_takes_no_fallback` pins the exact
degradation list rather than asserting it is merely short.

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
- `SIGTERM` mid-session restoring the terminal and re-raising, so the wait status still carries
  the signal — the one restore path the `cloo-client` tests cannot reach on their own.
- The child's exit code becoming cloo's exit code.

These read until an expected string appears rather than sleeping, with a deadline so a wiring
regression fails instead of hanging. Command-line parsing and the `$SHELL` fallback are unit
tested in `src/local.rs`.

The intended shape for the rest, in the order it becomes testable:

- **`cloo-core`** — profile and pane-metadata models joined layout at M2-04, profile
  configuration parsing at M2-05, the tab and session lifecycle at M3-01, and keymap resolution at
  M4-02; the rest of the configuration surface is still to come. Like layout, all of them are pure
  and testable without a terminal.
- **`cloo-server`** — the socket lifecycle joined the PTY tests at M1-01, handshake and attach
  coverage at M1-02, the session task at M1-03, and split and close at M2-01. Slower; keep the
  count deliberate.
- **`cloo-client`** — full-grid rendering and raw-mode restoration landed at M0-06, and the
  signal restore path joined them from the binary's own tests once M0-07 gave it a child process
  to signal. `SIGWINCH` went the same way at M1-03, for the same reason: a library test that
  signals itself signals the test runner. Incremental row diffing and its byte-exact renderer
  coverage landed with damage coalescing at M1-04, and pane chrome — headers, focus, attention, and
  dimming — at M2-03, extended at M2-10 with the attention summary, queue, and toast deck and their
  keyboard actions, all pure and testable without a terminal. Motion joined them at M4-04 on the
  same terms: time is an argument, so a transition is stepped rather than slept through.

### Agent-harness compatibility

Compatibility is tested in two layers. Deterministic fixture programs emit alternate-screen,
bracketed-paste, extended-key, focus, SGR mouse, OSC 52/8, notification/title, and resize
sequences; they cover cloo's semantics without requiring a vendor login or a moving CLI release.
Manual smoke runs of installed Codex and Claude Code cover one pane, splits, zoom, resize,
detach/reattach, large paste, mouse, and attention notification. Record the harness and terminal
versions in the test result when a manual behavior changes.

#### Recorded manual smoke — 2026-07-24

The recorded baseline used cloo `0.0.1` at commit `0650bd0` in util-linux `script` 2.39.3's
pseudoterminal with `TERM=xterm-256color`; `COLORTERM` and `TERM_PROGRAM` were unset. This is an
xterm-compatible fallback run, not a claim for a separately identified terminal emulator. The
harnesses were Codex CLI `0.145.0` and Claude Code `2.1.218`.

For each harness, an isolated foreground server accepted an attachment, the harness reached its
first interactive frame, and a bracketed `matrix-paste` payload reached its input without a
submission. The same session split with `C-b %`, zoomed and unzoomed with `C-b z`, detached with
`C-b d`, and reattached successfully. Closing the harness pane, exiting the survivor, and observing
the isolated socket disappear completed both runs. No prompt was submitted, and no credential,
account, or model output was collected.

This evidence confirms the vendor TUI startup, paste, layout, and lifecycle path. The
alternate-screen, extended-key, focus, SGR mouse, typed-effect, and resize rows remain asserted by
`crates/cloo-server/tests/compat.rs`; they must not be made contingent on a live vendor account.
Inline graphics are optional: the xterm-compatible baseline offered none, and cloo suppresses the
typed unavailable-graphics effect without forwarding a graphics payload. Thus a missing Codex pet
or comparable optional graphic is a successful degradation, not a smoke failure.

The consolidated deterministic suite is `crates/cloo-server/tests/compat.rs`, added at M7-02 — the
automated gate the [compatibility contract](AGENT_WORKFLOWS.md#compatibility-matrix) refers to. It
drives one scripted `sh -c` child per category the contract names, through the session actor, and
asserts cloo's server-side semantics: **screens** (the alternate screen round-tripping while the
primary grid survives), **paste** (bracketed exactly when the child set `?2004h`, plain typing
otherwise), **keys** (a `\x1b[1;5A` Ctrl-Up reaching the child byte for byte), **focus** (`?1004h`
negotiated and `\x1b[I` delivered, silence to a child that asked for neither it nor the mouse),
**mouse** (`?1000h`/`?1006h` negotiated and an SGR press `\x1b[<0;1;1M` delivered, silence
otherwise), **effects** (a title and an OSC 52 store crossing as typed effects while a sixel DCS and
an OSC 9 notification are dropped — an arbitrary OSC or DCS payload cannot become an effect), and
**resize** (both the grid rectangle and the child's own `stty size`). This layer proves the same
input semantics the wire-level `crates/cloo/tests/attach.rs` fixtures do, one level down at the
actor, because `cloo-server` may never name `cloo-client`. No fixture needs a vendor CLI, a login,
or a moving release.

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
| `crates/cloo-proto/src/frame.rs` | Wire protocol | Round-trip for every message and value type, including typed outer-terminal effects, unavailable graphics, and per-pane attention with its provenance, back-to-back framing, partial and oversized frames, corrupt payloads, handshake version match/mismatch, and the M9 status vocabulary — `WorkspaceStatus`, `SessionSummary` with its fields asserted one by one, `ConfigReloaded`, and an `InspectSession` refused at a stale version the way an attach is |
| `crates/cloo-proto/src/adapter.rs` | Adapter control protocol | The four permitted states mapping one-to-one onto attention states with `quiet` and `unknown` unreachable from any of them, and every adapter message, reply, and rejection round-tripping with a rejection that explains itself |
| `crates/cloo-proto/src/ids.rs` | Wire protocol | Newtype ID accessors, `Display` prefixes, transparent serialization |
| `crates/cloo-proto/src/stream.rs` | Framed transport | Reassembly across reads, ordered queued frames, a clean close as `Ok(None)`, a mid-frame close as `Truncated`, and an oversized prefix refused |
| `crates/cloo-core/src/layout.rs` | Layout tree | Split, close, collapse, resize, the layout pass, exact tiling, every rejection leaving the tree unchanged, closing a freshly split pane restoring the previous tree exactly — the rollback a failed pane spawn depends on — geometric directional focus in every direction from every pane, zoom as a view flag that preserves every ratio, and the dragging form of resize: one divider moved with the tree's shape and every other ratio asserted unchanged, either side of a divider moving it the same way, a drag past the end clamped to `MIN_PANE_SIZE` rather than refused, an extent too small to divide leaving the ratio alone, and a missing pane or a missing axis refused |
| `crates/cloo-core/src/copy_mode.rs` | Copy mode | Retained-scrollback positions, vim-like cursor motion, linear selection and extraction, regex match collection/navigation, and invalid-regex preservation of the prior search |
| `crates/cloo-core/src/id.rs` | Session model | Monotonic non-reusing ID allocation, resume, and saturation |
| `crates/cloo-core/src/tab.rs` | Tab model | A tab as a named layout with a focused pane, its name validated like a pane name, and focus refusing a pane the layout does not hold |
| `crates/cloo-core/src/session.rs` | Session model | The tab lifecycle: create appending and activating, rename and select touching only their target, close with its defined active-tab behaviour (right neighbour, rightmost fallback, non-active left alone), and every rejection changing nothing — unknown tab and the last tab refused with unknown checked first |
| `crates/cloo-core/src/profile.rs` | Profiles | The three built-ins as data — launcher order, each validating, none carrying an adapter, `codex` reconstructible field for field — plus every shape rejection: ID alphabet and bound, default name, command NUL or control character, and a recommendation below the layout floor |
| `crates/cloo-core/src/pane.rs` | Pane metadata | Validated names, labels, and an absolute-only working directory (a path that does not exist still validating, which is what pins validation to being pure), attention as state plus provenance: `unknown` by default, only three states queueing, acknowledgment cleared on change but kept on a repeat, only an adapter advisory, and the wire projection carrying what the user supplied with an absent task staying absent, plus attention's own wire projection: `unknown`/`None`/unseen by default, state, provenance, and acknowledgment kept together, and every state mapping to a distinct wire form, plus the adapter opt-in: a pane permitting only the adapter its profile named, every built-in permitting none, and an adapter state that can never become `quiet` or `unknown` |
| `crates/cloo-core/src/config.rs` | Configuration | Profile definitions parsed from `config.toml` text: a document error keeping the defaults and an unknown key refusing rather than being ignored, one invalid profile dropped with a warning while its neighbours load, built-in override in place, duplicate IDs keeping the first, and the command and `min_size` surface — omitted command as login shell, empty array refused, arguments verbatim, a recommendation below the layout floor rejected; plus the `[keys]` table — no table meaning the `C-b` defaults, a rebound prefix keeping every binding, an override leaving the other defaults alone, `none` unbinding, a chord written twice as a document error, an unspellable prefix keeping `C-b`, an invalid chord or unknown action dropped alone in chord order, and an action needing typed text refused; plus the `[visual]` table — no table meaning Storm, dimming, the minimal status row and ordinary motion, every named theme and `terminal` inheritance and both status modes spellable, a full table round-tripping into the typed accessors, motion off and reduce-motion answering `animates` identically while staying distinct fields, an unreadable theme or status name warning and keeping the *whole* table's defaults without costing the profiles, and a boolean given a string or an unknown key refusing the document |
| `crates/cloo-core/src/keymap.rs` | Keymap | Chord spellings — modifiers then a key, case-sensitive characters and case-insensitive names, a trailing `-` as the key, every canonical spelling round-tripping, and each invalid spelling refused by its own error including a literal control character reported without printing it; the action vocabulary round-tripping with no spelling for an action that needs typed text or that names a pane, and the four directional focus actions asserted still bindable because they are what a click's keyboard equivalent is; and the table itself: the tmux-shaped defaults under `C-b`, default arrows identified as client-resolved resize sides while `h/j/k/l` remain focus and overrides/unbinds win, an override replacing in place and reporting what it displaced, an addition appended, two keys for one action as an alias, a rebound prefix leaving the bindings alone, and unbinding removing exactly one entry |
| `crates/cloo-core/src/error.rs` | Session model | `LayoutError` messages naming the pane, sizes, and axis they refused, `MetadataError` naming its field and escaping a rejected control character rather than printing it, and `SessionError` naming the tab it refused and explaining the last-tab rule |
| `crates/cloo-core/src/grid.rs` | Wire conversion | Emulator cells, colours, attributes, cursor, and negotiated pane modes crossing into wire types, and the two crates' attribute bit layouts still agreeing |
| `crates/cloo-core/src/theme.rs` | Theme model | The four named palette spellings, complete style-guide token tables, Storm's exact reference values, and a palette choice round-tripping as either a named theme or `terminal` inheritance in one namespace |
| `crates/cloo-term/src/emulator.rs` | Emulation | Feed across read boundaries, every SGR flag and colour form, alternate screen, cursor position/visibility/shape, resize and reflow, scrollback growth and clamping including a complete history read that leaves the viewport put, typed title/clipboard effects with backend replies suppressed, one fixture per negotiated input mode — set, read back, and cleared — and the bell taken once, coalesced across several rings, never rung by text, and never surfaced as an effect |
| `crates/cloo-server/src/pty.rs` | PTY reactor | Pure only: config defaults and builder, `winsize` conversion, `TermError` to `PtyError` conversion |
| `crates/cloo-server/src/launch.rs` | Launching | Pure only: a profile's default name kept and the user's overriding it, an invalid profile refused before anything is spawned, argv kept verbatim through `configure`, the session's environment surviving a profile's command, and login-shell resolution with its `/bin/sh` fallback |
| `crates/cloo-server/tests/pty.rs` | PTY reactor | Scripted-shell output reaching the grid, split reads, `winsize` and controlling terminal, input forwarding, resize seen by the child, EOF and exit status, spawn failure, and drop reaping the child |
| `crates/cloo-server/tests/session.rs` | Session task | Split, close, focus, and zoom against real PTYs: both panes in the layout with the new one focused and its child started at its own geometry, a close collapsing the split and regrowing the survivor's child, a split with no room refused with nothing changed, the last pane and an unknown pane refused with the child still running, a resize divided between every pane, focus moving across an uneven split with input following it, and a zoom cycle that fills the area, restores the ratio, and leaves both children's pids unchanged; tab switching additionally proves both tab children retain their original pids; plus launching from an explicit profile: metadata reaching every snapshot with the split pane untouched, the child's own `pwd` proving the working directory (not only the metadata), a named profile reaching the pane it launched, a plain split repeating the session's launch, and a missing program failing with a message that names it and `PATH` while the layout rolls back; plus attention through the actor (no PTY): a report reaching the next snapshot with its provenance, acknowledgment moving only the seen flag, a re-report keeping it while a changed state clears it, and a report for a closed pane dropped without touching the survivor; plus the generic sources against real children: a bell reaching `needs_input`/`Bell`, a clean and an error exit reaching `ready`/`failed` with `Lifecycle` provenance, and bait text leaving attention `unknown`; plus copy mode: a retained regex and visual selection projected on the session snapshot, a reattached handle moving the same cursor, and a malformed regex retaining the earlier query; plus mouse delivery against real children: an event naming an unfocused pane reaching that pane while the focused one still reads the typed bytes, a pane that never enabled the mouse written nothing while its neighbour tracks, and an event naming a closed pane dropped rather than redirected; plus the chrome mouse actions: a click focusing the pane it names with a stale name and a zoom-hidden pane both dropped, and a gutter drag moving one divider by the cells asked for while the neighbour gives up exactly that much, both children keep the pids they printed at startup, focus stays put, a drag past the end stops at the minimum, and a drag naming a missing pane or an axis with no divider changes nothing; plus the adapter control path: an opted-in adapter's report arriving attributed and obeying the same coalescing rule, an adapter the profile did not name refused with an observed `failed` state left intact, a pane that opted into nothing reachable by none, and a report naming a closed pane refused rather than silently dropped |
| `crates/cloo-server/tests/compat.rs` | Terminal compatibility | The deterministic compatibility fixture suite (M7-02), one scripted child per category through the session actor: the alternate screen round-tripping while the primary grid survives, bracketed paste negotiated and encoded with its plain-typing fallback, a `\x1b[1;5A` extended key reaching the child verbatim, focus reporting negotiated and delivered with silence to an unasking child, an SGR mouse press negotiated and delivered with silence otherwise, typed title and OSC 52 effects crossing while a sixel DCS and an OSC 9 notification are dropped, and a resize reaching both the grid rectangle and the child's `stty size` — no vendor CLI or account |
| `crates/cloo-server/src/config.rs` | Configuration | Pure `CLOO_CONFIG`/`XDG_CONFIG_HOME`/`HOME` path precedence, file reading at the server boundary, atomic `ConfigManager` replacement, and an awaitable `SIGHUP` source |
| `crates/cloo-server/tests/config.rs` | Configuration | Real-file valid reload without a restart, malformed reload preserving the last valid configuration, missing-file reset to built-ins, per-profile warning with valid neighbours applied, `[keys]` and `[visual]` replaced in the same assignment as the profiles, a fileless manager keeping its supplied value, a rejection rendering one diagnostic naming its file, and — over a real daemon, socket, and child — a `SIGHUP` publishing one increasing revision with the new table live and the child still answering, against a refused reload that publishes none and reports why |
| `crates/cloo-server/src/socket.rs` | Socket lifecycle | Pure only: `CLOO_SOCKET`/`XDG_RUNTIME_DIR` precedence, the per-uid `/tmp` fallback, session-name validation, the lock file path, and the adapter control socket derived from the session socket with a lock of its own |
| `crates/cloo-server/tests/socket.rs` | Socket lifecycle | Bind creating a `0700` directory, a second daemon refused, unlink on drop, stale-socket replacement, refusal to remove a non-socket or follow a symlink, a successor's socket left alone, and a parentless path refused |
| `crates/cloo-server/src/conn.rs` | Handshake | Matching attach and inspection first frames accepted through one version gate, with attach `TermCaps` intact field for field; stale forms and a non-handshake first frame refused with a reason on the wire, a silent peer read as a close, the snapshot batch carrying workspace status first, then tabs before geometry with pane identity and attention before contents, and the session's layout pass carried through rather than recomputed; plus the control handshake: an adapter accepted under its own name, and a report before a hello, a mismatched version, and a name outside the `AdapterId` alphabet each refused |
| `crates/cloo-server/src/session.rs` | Session task | Pure only: the degenerate-area guard, one layout pass giving a single pane the whole area, attached geometry insetting that allocation to the framed PTY interior, a handle whose task is gone reporting it rather than hanging, and the input encoders — bracketed and plain paste, a paste that cannot close its own bracket, focus reported only on request, and one fixture per mouse event kind in both the SGR and legacy encodings |
| `crates/cloo-server/src/damage.rs` | Damage tracking | First-picture resync, changed-row-only frames, no-op snapshots, exit-frame detection, configuration revisions and workspace status each travelling in their own non-grid frames, and pane identity, attention, and tab selection each resent only when they change rather than on every damaged row |
| `crates/cloo-server/src/daemon.rs` | Daemon | Frame-rate cap, fixed IDs, minimum-size arithmetic with degenerate clients counted but excluded from sizing, changed-only workspace-status publication, a one-reply session inspection projected without attachment or damage subscription, and a lagged broadcast receiver replacement |
| `crates/cloo-client/src/renderer.rs` | Renderer | Byte-exact full and incremental frames, positioned chrome spans, absolute SGR, colour downsampling (including a status row with truecolor disabled), cursor placement, grid apply/resize rejections, transition frames (including a layered phase that leaves pane-body spans ordinary and a settled byte-identical frame), and `compose_frame` laying every pane's grid into its `PaneArea` with complete non-overlapping top/side/bottom frames, the fixed tab/status rows, and nested gutters positioned from one layout pass while render-time theme mapping and dimming leave the cache unchanged |
| `crates/cloo-client/src/motion.rs` | Motion | The 120ms transition stepped frame by frame from an injected `Instant`: an interruption settling at the end state rather than rewinding for every motion kind, a bounded frame count however often the transition is sampled, seven whole frames fitting the style guide's budget, reduce-motion drawing exactly one settled frame, both visual stillness switches resolving through `VisualConfig::animates`, a late tick settling rather than overshooting and a backwards clock reading as the start, a second transition replacing the one in flight, and the contrast ramp keeping every character, attribute, and readable colour with the `DIM` fallback for a non-blendable palette entry |
| `crates/cloo-client/src/theme.rs` | Theme resolution | Named theme RGB tokens, deliberate ANSI semantic fallback below truecolor, outer-terminal palette inheritance, and child default foreground/background mapping that preserves explicit colours |
| `crates/cloo-client/src/chrome.rs` | Pane chrome | Focus and attention as independent signals, glyph-and-label state without colour, the fixed width-degradation ladder at every width, the zoom marker, dimming by blend with a no-dim fallback, and the session-aware tab row — badge on the accent, raised and underlined active chip, right-aligned metadata yielding ahead of any tab, inactive tabs yielding around the active one, a badge reducing to `s` before disappearing, an unpublished name or count omitted, and identical characters under every theme; plus the attention queue's deterministic order and coalescing, an acknowledged state not refilling it, keyboard navigation and focus/acknowledge, the per-state summary tally, every state rendered text-glyph-and-colour in a row, and the bounded, per-pane-coalescing toast deck with its injected-clock lifetime, frame-budgeted entrance, reduce-motion answer, and upper-right placement between the chrome rows and off the focused cursor's row; plus the always-on status row's session, active tab, attention, and prefix forms yielding to ASCII markers; and `body_span` mapping child defaults on a copy before applying the shared dimming policy |
| `crates/cloo-client/src/copy_mode.rs` | Copy-mode rendering | Selection, match, and cursor spans painted from the grid cache with the cache asserted unchanged, role precedence with each role distinct by attribute as well as colour, positions outside the viewport dropped rather than clamped, the status row exactly its width at every width in one fixed degradation order, and the explicit copy: a denied policy or an incapable terminal writing nothing and not even sending the request, a permitted one writing exact OSC 52, and a non-clipboard effect refused on the copy path |
| `crates/cloo-client/src/overlay.rs` | Overlays | Every overlay dismissible from every state including an empty one, the Escape binding driven through `input::overlay_action`, navigation clamping at both ends with an empty overlay confirming to nothing, a confirmed session row naming that session, a confirmed launcher row yielding a profile the caller supplied, a profile that fails its own `validate` never becoming a row, pane details listing only what the server reported with an unset task absent, every row exactly its width from 0 to 60 and the box exactly its height from 0 to 10, the yield order and the dismissal hint standing last as exact strings, the selected row's `>` marker as text, and the backdrop dimming rendition without changing a character; plus the command palette read from a live keymap — the effective prefix in its title, every promised control reachable, a rebound prefix and chord verbatim, an unbound action with no row and none reachable by search either, a keymap that claims a client chord taking its row away, each row naming its `[keys]` action, ASCII-only cells with a bold chord column in truecolor and 16-colour, and its own exact width ladder — plus the palette's search loop: typing narrowing the list with the title reporting a position among results, backspace widening it and an empty query not dismissing, terms matching in any order and against the `[keys]` name case-insensitively, the cursor staying on its command when the row above it stops matching, both confirmation arms typed as `RunAction`/`OpenSurface`, Escape closing while `q` types a `q`, `C-u` clearing, a whole coalesced run decoded byte by byte, an exact query-row string with its own 0-to-60 ladder, a narrow box keeping title/query/dismissal in that order, and `no matches` over an emptied list; plus the launch notice — buildable only from a `LaunchRequest`, both states in words, settling once at `LAUNCH_DEADLINE` and finishing at `NOTICE_LINGER`, answered only by an unseen pane of the profile it named, and its own exact width ladder pinned to a literal row; plus the attention queue as a fifth kind of the one model — focus and acknowledge naming the row's `PaneId` rather than its queue position (proved by a row whose pane differs from the position a neighbour held), an empty queue acting on nothing, a refresh keeping the cursor on its pane and clamping onto a shorter list while leaving another overlay alone, `!` offered by help only while the keymap leaves it free, and legibility in truecolor, 16-colour, narrow, and empty states with the state colour resolved through the client theme |
| `crates/cloo-client/src/effects.rs` | Outer-terminal effects | Default-deny client policy, exact title and OSC 52 rendering, capability checks, safe suppression, and base64 padding |
| `crates/cloo-client/src/outer.rs` | Outer terminal | The degenerate-`winsize` fallback |
| `crates/cloo-client/src/capabilities.rs` | Capabilities | Detection from `TERM`/`COLORTERM`, an unresolvable `TERM` refusing an attach but not a local pane, each capability reading its own field, and the documented fallback for every baseline capability |
| `crates/cloo-client/src/resize.rs` | Resize watch | The recorded starting size, and nothing reported without a `SIGWINCH` — the signal itself is driven from the binary's tests |
| `crates/cloo-client/src/attach.rs` | Attach | A hello completing the attach, `TermCaps` round-tripping over the handshake, `Tabs` and `WorkspaceStatus` updates replacing their caches while a sent resize leaves effective size unchanged until the daemon answers, and a resolved command reaching the server; read-only inspection sending only the versioned `InspectSession` request and accepting only a summary while rejecting attach and grid replies; an unresolvable `TERM` surfacing as a capability failure, a refusal surfacing the server's own reason, a future server caught client-side, a non-hello reply and a silent server refused, detach waiting for its acknowledgement, and the live cache placing server grid rows below its tab/header chrome while the matching `ScreenLayout` sends hits to the same pane; the composed tab row carrying no badge and no client segment before a `WorkspaceStatus` and the daemon's own session name and client count after one, over the client's own visible pane count; plus client-local visual state — the fresh theme/dimming/status/motion value kept whole, a valid newer revision replacing it atomically, a rejected or duplicate revision preserving it, and two clients sharing one session while resolving different themes; plus copy highlights/status and a dimmed, key-owning overlay layered over that composed frame, with live layout motion restricted to chrome; plus `?` opening the command palette while `i` keeps pane details, an open palette consuming typed query bytes locally and never leaking one to a pane, Enter handing back the typed `Action` its row named, a client row swapping the palette for that surface without touching the wire, and a composed frame drawing the configured prefix's title and its split, detach, and add-pane rows; plus the live launcher — `a` listing exactly the profiles the client was given, a confirmed row handed back as the launch to send, an empty launcher and a dismissal both naming nothing, a launch whose deadline passes becoming a refusal drawn into the composed frame and then clearing, and only an unseen pane of the confirmed profile retiring that notice; plus the live attention queue — `!` opening one row per waiting pane from the real projections with a `working` pane excluded, navigation consumed locally, focus and acknowledge answering only `Action::FocusPane`/`Action::AcknowledgeAttention` with focus closing the surface and acknowledgment leaving it open, an acknowledged row surviving the keypress and leaving on the projection that reports it, an empty queue naming no pane, and the box drawn as dimmed client chrome that Escape always closes; plus the live toast stack over a nested layout — one notice per new actionable event with a resent projection raising none, repeats coalescing and changed states refreshing, capacity evicting the oldest, an acknowledgment or a closed pane retiring one while a settled pane waits for its deadline, pane damage raising and animating nothing, and the placed stack drawn as client chrome inside the safe area in both truecolor and 16-color while owning no keys |
| `crates/cloo-client/src/session_catalog.rs` | Session discovery | Environment-to-directory resolution for `$XDG_RUNTIME_DIR/cloo` and the per-uid `/tmp` fallback, actual-socket filtering without following symlinks, a `CLOO_SOCKET` override becoming one candidate, concurrent per-candidate inspection deadlines, and deterministic ordering by verified summary name then socket path |
| `crates/cloo-client/src/input.rs` | Input routing | One fixture per negotiated mode's request and matching reset, decoding of paste, focus, and every mouse report kind, sequences split across reads, a lone Escape released by a flush, a mode never requested left alone, the three mouse-ownership rules, hit testing every region of a drawn screen with a mis-described pane unable to swallow a chrome row or a header a pane's own cell, no chrome region producing a wire event even under full motion tracking, an unfocused-pane and a shift-held click routing to `PaneBody`, a tracking level below the event left for the server to drop, the attention-queue and overlay key bindings mapping to their actions with an unbound key mapping to none, and the command palette's separate vocabulary — arrows and `C-n`/`C-p` navigating, backspace and `C-u` editing, everything printable including space inserting, Escape dismissing while `q` inserts, a coalesced run decoded byte by byte with a three-byte arrow inside it beating the Escape it starts with, and an unusable byte dropped rather than passed on; plus chrome gestures — a divider found from the pane rectangles for both a gutter column and a header row between stacked panes, a cell with no pane on both sides of it refused, a gutter drag emitting resizes and nothing else with relative deltas and no command on the press or after the release, a header drag naming the pane above it, a click focusing a pane body or a header and chrome that names no pane focusing nothing, the wheel mapping to the copy-mode commands with a pane already in copy mode not asked again, a wheel over the tab row or status bar doing nothing, and an application-owned report never reaching the gesture machine at all; plus keys — one fixture per encoding a terminal sends decoded to its `cloo-core` spelling, a multi-byte character as one chord, an unmodelled or half-arrived sequence refused rather than guessed at, and the prefix state machine: every default-bound chord still the pane's without the prefix, typing passed through byte for byte, a prefix and its chord in one read, typing around a command keeping its order and bytes, an unbound or undecodable chord after the prefix consumed rather than typed, the prefix twice sending itself to the child, a rebound prefix giving the old one back to the pane, and a reset forgetting a pending prefix |
| `crates/cloo-client/src/raw_mode.rs` | Raw mode | Pure `termios` transformation and the restore slot's arm/disarm state machine |
| `crates/cloo-client/tests/raw_mode.rs` | Raw mode | Entry, drop, explicit restore, error unwind, panic, second-guard refusal, a pipe refused, and a registered mode reset written on the normal and panic paths, once, and refused rather than truncated |
| `crates/cloo/src/cli.rs` | Binary | The command line as a pure function: every launch option read, options stopping at the program so `sh -c` keeps its own flags, `--` for a program that looks like a flag, an unknown or repeated flag refused, `--profile` and a program refused together, and resolution — a named or configured profile with its defaults, the user's name/task/directory winning, an unknown profile naming the ones that exist, a program running as a generic pane named for itself, a relative directory resolved and a tilde refused, and a control character in a name or task refused; plus `attach` and `server` each accepting their default session or one name and refusing flags or extra arguments, and an empty argument vector — and only an empty one — parsing as the default workspace on the session an omitted `attach` would have named |
| `crates/cloo/src/main.rs` | Binary | The workspace startup branch: a bound socket is a live workspace while a missing path and a socket file whose listener was dropped are not, and the readiness probe's interval, hand-off grace, and deadline are bounded and ordered |
| `crates/cloo/src/local.rs` | Binary | The frame-rate cap |
| `crates/cloo/tests/cli.rs` | Binary | The command line, refusal without a terminal, the one-pane smoke path driven over a pseudoterminal, signal-path terminal restore, a `SIGWINCH` resizing the pane all the way down to the child's own pty, and the launch surface end to end: the help naming the options and the built-in profiles, an unknown profile and a control-character task label refused as usage errors, a `CLOO_CONFIG` profile resolved before terminal setup, and a profile whose program is missing failing with a message that names it; plus the public foreground server lifecycle — a named socket, exclusive ownership, an attach and prefix detach, a cooked server terminal, and cleanup limited to the server's own socket; plus the managed default workspace — a bare `cloo` creating a daemon that outlives its detaching client, and a bare `cloo` joining a live one with the socket's identity and the foreground owner both unchanged; plus that entry's races and failures — two concurrent bare `cloo`s converging on one session with one pane, a `SIGKILL`ed daemon's socket recovered, a regular file and a symlink each refused with the path intact and no socket left behind, and a `TERM` the client refuses handing back a cooked terminal while the workspace it created stays usable; plus the profile launcher end to end — shell text creating no pane, `<prefix> a` then `G` then Enter creating one whose reported profile is the confirmed `CLOO_CONFIG` profile, no overlay key reaching the shell, and a cooked terminal after the detach |
| `crates/cloo/tests/attach.rs` | Attach end to end | A real daemon and clients over real sockets: hello and snapshot, truthful initial/join/resize/detach `WorkspaceStatus` with unchanged projections suppressed and the client cache following only daemon answers, and a one-frame `InspectSession` summary with truthful name/counts/uptime, no geometry change or transcript, stale/malformed isolation, and clean failure after the daemon disappears; catalog discovery verifying and daemon-name-ordering two live sessions while rejecting a regular file, symlink, stale socket, and deadline-bound silent peer, plus a socket override yielding exactly one verified candidate without attaching; detach leaving the child alive and its state intact, then reattaching and reaping it after exit; a vanished client, a refused stale client, no daemon listening, a resize reaching both the grid and the child, a degenerate resize changing nothing, bounded burst damage with lagged-client recovery, concurrent-client fan-out, a typed OSC 52 effect reaching a capable, permitted client once, and a resync telling a client who every pane is — profile, name, task label, and working directory; plus input routing end to end: a paste bracketed exactly when the child asked, a focus report and an SGR mouse report reaching a child that enabled them, and neither reaching one that did not; plus copy mode end to end: copy-mode actions reaching the session over the wire, the projected copy state turning into highlight spans over the client's own damage-applied grid cache without changing it, and the explicit copy returning one typed clipboard store that the default policy refuses byte for byte and a permitting one writes; plus the wheel end to end: a chrome-routed report becoming copy-mode commands whose answer puts the copy cursor exactly `WHEEL_LINES` above where entering copy mode left it; plus the layout commands end to end: split, zoom, unzoom, and a focus-resolved close each moving the `Layout` frame the session reports; plus the `cloo attach` command over a real outer pty: composed chrome and child output render, the prefix detach restores terminal mode, and a second attachment reaches the same child; plus attention acknowledgment end to end: a bell reaching `needs_input` unacknowledged, a client's `Action::AcknowledgeAttention` reaching the session actor, and both that client and a second attached client told the pane is acknowledged with its state unchanged |

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
