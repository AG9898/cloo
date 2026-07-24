# Architecture

> Canonical source for system topology, runtime boundaries, and component responsibilities.
> Other docs should link here rather than restating architecture details.

---

## Overview

cloo is a client-server terminal multiplexer written in Rust. A background daemon owns every
PTY, terminal grid, and piece of layout state; thin clients attach over a Unix socket, receive
damage updates, and render. Detaching kills the client, not the session.

The functional target is parity with tmux and zellij. The differentiator is the rendered
interface — borders, status bar, focus treatment, theming, motion — and an agent-workspace
workflow for many concurrent coding harnesses. The boundary below deliberately puts all visual
work client-side.

---

## System Topology

```
      ┌──────────────── cloo server (daemon) ─────────────┐
      │  session ──┬── tab ──┬── layout tree              │
      │            │         └── pane ── PTY ── shell     │
      │            └── tab ...                            │
      │  each pane: PTY bytes → cloo-term → grid          │
      └───────────────────┬───────────────────────────────┘
                          │ unix socket, length-framed
              ┌───────────┴───────────┐
         client A                 client B    (both attached, same session)
      raw mode + render        raw mode + render
      ← all visual identity lives here →
```

There is no network tier, no database, and no external service. Both processes are local, and
the socket lives at `$XDG_RUNTIME_DIR/cloo/<session>.sock` — see [Socket lifecycle](#socket-lifecycle)
for the full resolution order and the ownership rules around that path.

---

## Component Responsibilities

The workspace is six crates. The split is load-bearing — in particular the `cloo-term`
boundary, which is what makes the emulation backend replaceable.

| Crate | Owns | Explicitly does not |
|---|---|---|
| `cloo-proto` | Wire types, framing (serde + postcard), the framed async transport, the adapter control vocabulary, handshake version | Know anything about PTYs or rendering |
| `cloo-term` | Thin wrapper over `alacritty_terminal` — feed bytes, read cells, resize, scrollback | Leak `alacritty_terminal` types across its public API |
| `cloo-core` | Session/tab/pane model, layout tree, keymap, profiles, pane metadata, config | Perform I/O |
| `cloo-server` | Daemon: socket, PTY reactor, damage tracking | Decide what anything looks like |
| `cloo-client` | Attach, raw mode, renderer, theming, input decoding and routing | Hold authoritative session state, or encode input for a pane |
| `cloo` | The binary; client-vs-server dispatch, CLI surface | Contain logic that belongs in a library crate |

All six crates exist in the workspace and are wired together end to end as of M0-07: `crates/cloo`
runs one local pane by composing `cloo-server`'s PTY reactor with `cloo-client`'s renderer. The
remaining contents land across M1–M2.

Dependencies flow one way and are declared through `[workspace.dependencies]` in the root
manifest, so every member inherits the same path and version. The crates sit in four layers:

```
       cloo            composition root — depends on anything below
      ↙    ↘
cloo-server  cloo-client    the two halves — never on each other
      ↘    ↙
     cloo-core             model and conversion — no I/O
      ↙    ↘
cloo-proto  cloo-term      leaves — no intra-workspace dependencies at all
```

**The rule is the layering, not a single chain.** A crate may depend on any crate in a lower
layer, and the leaves may be named directly by anything above them. What is forbidden is a
back-edge (a lower layer naming a higher one), a cycle, and an edge between `cloo-server` and
`cloo-client`, which must stay independent halves. The forbidden edges hold for
dev-dependencies too: a test that needs both halves belongs in `crates/cloo`, which already
depends on both.

An earlier draft of this section drew a strict chain — `cloo → {server, client} → core →
{proto, term}` — which the implementation contradicted three times in M0 alone. The reasons were
the same each time and are worth stating once rather than per-milestone: `cloo-proto` is the wire
vocabulary, so every crate that speaks the wire names it, and routing it through `cloo-core`
would reduce that crate to a re-export shim. The current edges are:

| Crate | Depends on | Why the direct edge |
|---|---|---|
| `cloo` | `cloo-server`, `cloo-client`, `cloo-proto` | Composition root; names the geometry it passes between the halves |
| `cloo-server` | `cloo-core`, `cloo-proto`, `cloo-term` | Hands clients wire contents; the PTY reactor owns a pane's `Emulator` |
| `cloo-client` | `cloo-core`, `cloo-proto` | The grid cache stores wire `Cell`s and applies wire `RowUpdate`s; attach speaks the wire directly |
| `cloo-core` | `cloo-proto`, `cloo-term` | Owns the conversion between the two cell vocabularies |

`cloo-core` also names `toml` and `serde`, for configuration *parsing* only — the serializer and
the format-preserving document model are left out, since cloo never writes a config file back.
Parsing text is not I/O, so the no-I/O rule is intact: the file is read by the server.

The `alacritty_terminal` rule is untouched by any of this: `cloo-server` names only `cloo-term`'s
own types, and the backend stays behind that wrapper.

### Emulation

`cloo-term::Emulator` is one terminal emulator per pane, owned by the session task. It is
synchronous and does no I/O: the PTY reactor reads bytes and calls `feed`, which is safe across
read boundaries because parser state persists between calls — a sequence or a multi-byte
character split across two reads still parses.

The surface is exactly what the crate table promises. `feed` takes bytes; `row`, `rows`, and
`row_text` read the visible grid; `resize` reflows it; `scrollback_len`, `scroll_offset`,
`scrollback_text`, `scroll`, and `scroll_to_bottom` cover history. `scrollback_text` reads
retained history without moving the display offset, so server-side copy mode can search it without
disturbing the current viewport. `cursor` and `is_alt_screen` report the state a
renderer needs but cannot derive from cells alone, and `modes` reports the input modes the child
*application* has negotiated — bracketed paste, focus reporting, mouse tracking and its encoding,
and the Kitty keyboard protocol. Those come from private mode sets the child wrote, so the
emulator is the only place that can answer; reading them from it rather than parsing the same
sequences a second time is what keeps the two answers from disagreeing. The Kitty protocol is off
by default in the backend and cloo turns it on, or a pushed flag set would be silently discarded
and `modes` would report legacy keys forever.

`Emulator::resize` moves emulation state only. The child still has to be told through
`TIOCSWINSZ` from the PTY layer, and the two together are the resize race described in
`AGENTS.md`.

The value types (`Cell`, `Color`, `CellAttrs`, `CursorState`, `PaneModes`, `MouseTracking`) are
`cloo-term`'s own. They mirror
the `cloo-proto` shapes without depending on them, because `cloo-term` sits at the bottom of the
dependency graph next to `cloo-proto` and depends on nothing in the workspace. `cloo-core` owns
the conversion; the `CellAttrs` bit layouts match so it stays a field copy.

`Emulator::drain_effects` is the analogous boundary for pane requests aimed at the outer terminal.
M1-08 replaced the backend's `VoidListener` with a bounded, non-blocking typed queue: it
recognizes title changes and OSC 52 clipboard stores, but drops backend replies, a full queue, and
every event cloo has no allowlisted type for. `OuterTerminalEffect` deliberately offers intent
such as title, clipboard, hyperlink, notification, and progress changes, plus
`Graphics(Unavailable)`; it contains no raw OSC, DCS, or graphics payload. `cloo-proto` mirrors
that vocabulary in `ServerMessage::Effect { pane, effect }`, which took the handshake to v3;
M2-06's `ServerMessage::Panes` took it to v4, M2-07's `ServerMessage::Attention` to v5,
M5-01's `ServerMessage::CopyMode` to v6, and M5-02's copy-mode `Action`s plus
`CopyModeState::viewport_top` to v7.
M1-09 drains those values through the session actor and fans each one out as its own non-damage
frame. The server neither chooses nor applies an effect: each client combines its terminal
capabilities with a default-deny local policy. Title changes are permitted only by the title
policy, OSC 52 stores need both clipboard policy and `clipboard_osc52`, and every unsupported
effect is a no-op. Drained effects never change session state.

### Server

Owns all PTYs, grids, scrollback, and layout state. Fans damage out to every attached client.
A client crash can never lose state, because the client holds only a cache of the visible grid.

#### PTY reactor

`cloo-server::pty` is two layers. `Pty` is the raw resource: an `openpty` pair, a child spawned
onto the slave side as a session leader with `TIOCSCTTY`, and the `libc` calls that read, write,
and `TIOCSWINSZ` it. The master descriptor is an `OwnedFd`, set non-blocking and close-on-exec,
and `Pty`'s `Drop` kills and reaps the child — restoration is by ownership, not by a shutdown
call a caller has to remember.

`PtyReactor` is the actor body above it: one Tokio task owns one reactor, which owns that pane's
`Emulator` and loops on `pump`. Readiness comes from `AsyncFd`; `pump` reads once and feeds the
result to the emulator, and it never renders or emits an update — damage coalescing belongs to
the session task above.

Two behaviors are worth knowing. A read on a Linux PTY master whose slave has closed fails with
`EIO` rather than returning zero, so that is translated into an ordinary EOF at the boundary and
callers only see genuine errors. And `PtyReactor::resize` resizes the grid *before* issuing
`TIOCSWINSZ`, so output arriving immediately after the child's `SIGWINCH` lands on a grid that is
already the right shape; if the `ioctl` then fails, the grid is ahead of the child, which is the
recoverable direction to be inconsistent in.

`PtyReactor::snapshot` is the other half of the boundary: it captures the whole visible grid as a
`PaneSnapshot` of wire geometry, `RowUpdate`s, and an optional cursor. Nothing in it describes an
appearance. The conversion from emulator cells to wire cells lives in `cloo-core::grid` — the one
crate that sees both vocabularies. At M1-04 the daemon's `DamageTracker` compares those captures
only at its frame boundary and publishes the changed rows; the type a client applies does not
change.

#### Socket lifecycle

`cloo-server::socket` decides where a session's socket lives, guarantees exactly one daemon owns
it, and clears the one a dead daemon left behind. Path resolution is a pure function of its
inputs — `resolve_socket_path(session, CLOO_SOCKET, XDG_RUNTIME_DIR, uid)` — with
`session_socket_path` as the thin wrapper that reads the process environment, matching
`cloo-client::capabilities`. Precedence is `CLOO_SOCKET` verbatim, then
`$XDG_RUNTIME_DIR/cloo/<session>.sock`, then `/tmp/cloo-<uid>/<session>.sock`. `CLOO_SOCKET`
names a socket rather than a directory and ignores the session name entirely, because its purpose
is standing a development daemon beside a live one. The `/tmp` form is per-uid so two users never
collide, and it is a fallback rather than the default because `/tmp` outlives a login session.

A session name reaches the filesystem, so `/`, `\`, control characters, `.`, `..`, and the empty
string are refused rather than sanitized — silently renaming a session produces a socket the user
cannot find.

A session has a second endpoint beside it: `control_path_for` appends `.control` to the session
socket, and that is where opt-in local adapters connect (M2-09). It is derived rather than resolved
separately so `CLOO_SOCKET` moves both halves of a development daemon together, and it is a
`Listener` like any other — its own `.control.lock`, the same stale cleanup, the same unlink on
drop.

**Ownership is an advisory `flock` on a companion `<socket>.lock`, not the presence of the socket
file.** A socket file proves nothing: a daemon killed with `SIGKILL` leaves one behind, and a live
daemon has one too. The kernel releases a `flock` however the holder dies, so the lock answers
"is a daemon running" exactly, and a second daemon gets `SocketError::AlreadyRunning` instead of a
race. Holding the lock is also what makes cleanup safe — the unlink is reachable only after it has
been established that no other daemon exists. The lock file itself is never removed; unlinking it
would race a daemon that has already opened it and is about to lock it.

Cleanup is deliberately narrow. It touches only the one path it holds the lock for, and only when
`symlink_metadata` says that path is a socket. A regular file, a directory, or a **symlink** there
is a `SocketError::NotASocket` refusal — following the link would report the target's type and the
unlink could then remove something outside the socket directory. `CLOO_SOCKET` is user-supplied,
and a typo must not cost anyone a file.

`Listener` restores by ownership, like `Pty` and `RawMode`: its `Drop` unlinks the socket, so a
daemon that exits normally leaves nothing behind. The unlink is guarded by the `(device, inode)`
pair recorded at bind, so a departing daemon cannot remove a successor that already claimed the
same path. The directory is created and narrowed to `0700` on every bind — `create_dir_all`
applies the umask, and a session socket is a channel into the user's shell.

The listener is bound non-blocking as `std::os::unix::net::UnixListener`, which keeps `bind` free
of a runtime requirement; `Daemon::new` hands `try_clone_std` to
`tokio::net::UnixListener::from_std` so the guard and its unlink stay alive alongside the async
half.

#### Attach, hello, and detach

`cloo-server::conn` is the handshake and nothing else. `accept_attach` reads the first frame on a
connection and refuses anything that is not an `Attach` at a matching `PROTOCOL_VERSION`. Every
refusal is *sent* as `Refused { reason }` before the connection is dropped: a client told why it
was turned away can print something the user can act on, and a client that only sees a closed
socket cannot. A peer that closes before saying anything is not a refusal — there is nobody left
to report to.

`conn::session_snapshot` is what an attach delivers. A client caches the visible grid and nothing
else, so it needs the whole picture the moment it connects, and it arrives as the same message
types an incremental update uses — `Layout`, then `Panes`, then `Attention`, then `Damage`, then
`Modes`, then `CursorMoved` — so a resync and a
damage frame stay one code path on the client. Geometry comes first so rows never arrive with
nowhere to land, and identity and attention come before contents so a pane header has something to
say before there is anything to draw it around.

`cloo-server::daemon` is the serving loop that owns the pane and outlives every client attached to
it. The property it exists to guarantee is that the child belongs to the daemon, not to whoever is
watching: a client that detaches, disconnects, or dies takes nothing with it, and the PTY keeps
being pumped *between* connections so a reattaching client finds the session where it left it
rather than where it last drew it. `Detach` is acknowledged with `Detached` and the connection
closed; the child never learns it happened.

The daemon owns *no session state*. It holds a `SessionHandle` — a sender — and every keystroke
and resize it receives becomes a command on that channel; snapshots come back the same way. That
is what makes it a transport rather than a second owner: there is no other path to the grid or
the PTY for a bug to take.

As of M1-04, the daemon accepts connections continuously and gives each attached client its own
socket task and bounded `broadcast` receiver. The coordinator is the only task that captures the
session: on a dirty frame tick it compares the new snapshot to the last published one and sends a
single ordered batch — layout, changed rows, modes, cursor — without awaiting any socket. A client
whose receiver reports lag discards the partial backlog and asks the coordinator for a full
snapshot; a slow terminal can therefore delay only its own resync, never the session task.

The session geometry is the component-wise minimum of usable attached-client sizes. When a client
attaches, disconnects, or resizes, the coordinator relays that minimum through the session task;
with no clients it keeps the last usable geometry so a detached child is not surprised by a resize.
This is what keeps two clients visually consistent, and it is also the reconnect/resize race the
M7-01 fixtures pin down: a narrower client that joins shrinks the survivor's grid, and when it
*leaves* the survivor must be redrawn back at the full width — because a pane whose size changed
resends every row, a client cache is never left applying a full-width row against a stale narrow
grid. The grid corruption those fixtures rule out is exactly that geometry disagreement.

#### The session task

`cloo-server::session` is the one thing that mutates a session. Everything that changes it — a
keystroke, a resize, a split — arrives as a `Command` on a single `mpsc` and is applied in
arrival order by one task, as of M1-03. `Input` carries already-encoded keys; `Paste`, `Focus`,
and `Mouse` carry events the task encodes itself from the pane's negotiated modes, as of M1-07,
and `SessionSnapshot` carries those modes back out so a client can route the next event. `Mouse`
is delivered to the pane the event *names* and encoded from **that pane's** modes, as of M6-01 —
never the focused pane's, since an application that never asked for the mouse must not be handed a
report because its neighbour did — and only if the named pane is one the user can see: in the
active tab, and not hidden behind a zoom. There is
no `Mutex` on session state and no second path
to it: a `SessionHandle` is a sender and nothing more, so a caller cannot reach past it. Both the
daemon and the binary's local loop hold one, which is why the in-process path and the socket path
cannot drift.

`Session::resize` is why the serialization matters. Resize is a three-way race between the grid,
the child's `TIOCSWINSZ`, and the application's own `SIGWINCH` handling, and the only way to
reason about it is for one actor to do the halves in a fixed order. It runs **one layout pass** —
`Layout::resolve` — and drives every pane's geometry from its output, so the rect a client is told
about and the `winsize` its child is given cannot come from two different computations. Within a
pane, `PtyReactor::resize` keeps the grid-then-ioctl order. A degenerate area is ignored rather
than refused: a client that briefly reports zero rows mid-drag has no bearing on a child that is
running fine, and refusing would turn a cosmetic glitch into a dead session.

Output flows back as a `SessionEvent`. `Output` is a *level*, not an edge — at most one is ever
queued, so a session producing bytes faster than anyone reads them coalesces into a single pending
notification rather than one per PTY read, and the reader asks for a snapshot when it is ready to
draw. `Exited` is sent once the PTY reaches end of file; the task stays alive and still answers
snapshot commands after it, which is what lets a child's last words be drawn before its death is
reported. The task pumps every pane's PTY for its whole life, attached or not, so nothing written
between connections is lost. A pane whose child exits stops being pumped and keeps its grid;
`Exited` is sent once *every* pane's child is gone, because a session with one dead pane and three
live ones is not over.

Every event leaves through an **outbox** the session task owns, and the loop never awaits a send on
the event channel. That is the fix M6-03 made, and the reason is a deadlock rather than a
throughput concern: an actor parked on `send` has stopped applying commands, so it no longer
answers `Command::Snapshot` — and the reader that would drain the channel is normally the one
blocked awaiting exactly that snapshot. The loop instead selects over a channel permit alongside
the PTY pump and the command receiver, keeps applying commands while it waits for room, and puts
the event back at the front of the outbox whenever another branch wins. `Output` coalesces inside
the outbox instead of relying on the channel's capacity; an `Effect` is ordered with respect to
other effects and is the one thing overflow may drop, on the same reasoning as `cloo-term`'s
bounded queue — an effect changes no grid cell and no session state. `Exited` is never dropped and
never coalesced, because no later snapshot can recover it.

##### Split and close

As of M2-01 a session owns one PTY per pane, and the layout tree is the only record of which panes
exist. `Command::Split` and `Command::Close` are what keep the two in step, and the ordering is the
whole of the atomicity:

1. **The layout is asked first**, because it is the half that can refuse. A split below
   `MIN_PANE_SIZE`, a close of an unknown pane, and a close of the session's last pane all fail
   here — before a process is spawned or a child is killed.
2. **Then the PTY.** A split spawns its child at the rectangle that same layout pass produced, so
   the child's first `TIOCGWINSZ` is already right; a close drops the pane's `PtyReactor`, and
   dropping it is what kills and reaps the child. There is no separate teardown to forget.
3. **A spawn that fails rolls the layout back** by closing the pane it just added, which restores
   the tree exactly — ratios included, since collapsing a fresh split promotes the pane that was
   split back into its own place.

No `await` sits between those steps, so no other command can observe a pane that exists in the
layout and not in the PTY set, or the reverse. Both commands then run the same geometry pass a
resize does, so a split shrinks its neighbour's child and a close regrows the survivor's.

Focus follows a split. Closing the focused pane moves focus to the first surviving pane in
traversal order — directional movement needs a pane to start from, and the closed one is gone.

##### Directional focus and zoom

As of M2-02, `Command::MoveFocus` and `Command::ToggleZoom` sit on top of that without disturbing
it. Neither can fail in a way a user needs to hear about, so neither carries a reply channel.

`Layout::neighbor` is **geometric, not structural**: it reads one layout pass and picks the pane a
user actually sees in a direction, rather than whichever sibling the tree holds. A candidate must
lie wholly on the named side and share some extent on the perpendicular axis — a pane diagonally
across the tab is not what anyone means by *left*. Among those the nearest wins, ties going to the
one nearest the origin's own leading edge and then to traversal order. Moving past the edge of the
layout is not an error and does nothing: wrapping around would move attention somewhere nobody was
looking. `Side` lives in `cloo-core` and is not a wire type — the client sends `Action::FocusLeft`
and the server turns it into one, so adding a direction is never a protocol change. As of M6-02
those four actions are routed by the daemon, alongside `Action::FocusPane`, which names a pane
directly because that is what a click does; both land on the same actor, which is what makes the
mouse's and the keyboard's focus one code path rather than two that must agree.

**Zoom is a view flag, not a shape.** `Layout::zoom` records a pane id and changes nothing else;
`Layout::resolve` then answers with that pane alone, filling the area. Three properties follow, and
they are the reason it is modelled this way:

- **Ratios are preserved by construction.** Unzoom restores the previous picture exactly, because
  no split and no ratio was ever touched.
- **No PTY is restarted.** Zoom's only effect on a child is a `TIOCSWINSZ` from the same geometry
  pass a resize runs, and a *hidden* pane's child is not even told that — it keeps the `winsize` it
  had until it is visible again.
- **Zoom follows focus.** A zoom always names the focused pane: moving focus while zoomed re-aims
  it, since the alternative leaves a user typing into something they cannot see. A split unzooms,
  or the pane it just created would be invisible; a spawn failure puts the zoom back along with the
  tree, since a rollback restores everything or nothing. Closing the zoomed pane unzooms, and
  closing any other leaves it alone.

`SessionSnapshot::zoomed` carries the state out, and it reaches clients as `LayoutSnapshot::zoomed`
on both the attach resync and any frame in which it changed. Chrome for it is M2-03.

##### Launching from a profile

As of M2-06 a pane is only ever created from a `cloo-server::launch::Launch`: a validated profile
plus the name, task label, and working directory the user supplied. `Launch::new` validates the
profile and builds the pane's `PaneMeta` **before any process exists**, which is the same ordering
split and close use — ask the half that can refuse first, and a refusal never costs a child. What
is left to fail is `execvp`, and a program that is not on `PATH` surfaces as `PaneError::Spawn`
naming it, with the layout already rolled back.

`Launch::configure` is where the pure model meets the server's I/O. It applies a launch over the
*session's* half of a `PtyConfig` — the environment every pane inherits, and the geometry the
layout pass is about to correct — and overwrites the profile's half: the argv and the working
directory. Splitting it that way is why a split can launch a different profile without losing the
session's `TERM`. Resolving `$SHELL` for a `ProfileCommand::LoginShell` happens here too, because
`cloo-core` performs no I/O; `/bin/sh` is the fallback POSIX guarantees.

`SessionHandle::launch` is the explicit form of `split`: it names what to run, while a plain
`split` repeats the session's own launch, which is what a keybinding means by "split this again".
Every pane's metadata rides out in `SessionSnapshot::metas`, projected from the same layout pass
that resolves geometry — so a client can never be told about a pane it has no identity for, or
handed an identity for a pane that is not on screen.

**Nothing in a `Launch` is inferred.** There is no constructor that takes a grid, a process name,
or transcript text, which is what makes "cloo does not guess a task" a property of the type rather
than a rule someone has to remember.

Reading N PTYs is a hand-rolled `select_all` over the unended panes rather than a dependency:
`PtyReactor::pump` is cancel-safe, so the futures that lose are dropped and cost a wakeup, never a
byte. The polling order rotates, so a pane producing output continuously cannot starve a quiet one.

`cloo-client::attach` is the other end. It connects, sends `Attach`, and interprets nothing until
the reply is a `Hello` whose version matches — both directions check, because `Attach` catches a
stale client and `Hello` catches a rebuilt server, and the second is the case that actually
happens the first time someone rebuilds mid-session. `Attached::detach` sends `Detach`, waits for
the acknowledgement, discards any damage still in flight, and drops the connection. A socket with
nothing behind it reports "no cloo daemon is listening", not a bare `ENOENT`.

### Client

Holds a copy of the visible cell grid, diffs against incoming damage, and emits escape
sequences to the real terminal. **All chrome is rendered here.** The server sends pane contents
and layout geometry; the client decides how borders, status bar, and focus treatment look.

That boundary is why theming never touches session state.

`cloo-client` also depends on `cloo-proto` directly, as of M0-06: the client's grid cache stores
wire `Cell`s and applies wire `RowUpdate`s, and routing those through `cloo-core` would mean
re-exporting the whole message surface from a crate that has no rendering concern. Like
`cloo-server` → `cloo-term`, this is a shortcut down the graph, not a back-edge. As of M1-02 it
also speaks the wire outright: `cloo-client::attach` owns the client half of the handshake.

#### Renderer

`cloo-client::renderer` is two types. `Grid` is the client's cache of one pane's visible cells:
row-major, always exactly `rows * cols` cells, and mutated only by replacing a whole row —
matching the damage unit on the wire, so applying an update is a `copy_from_slice`. A row update
whose row or width disagrees with the cache is rejected as a `RenderError` rather than partly
applied, because that disagreement means a resize crossed a damage message in flight and the
client should resync instead of drawing a guess. A zero-width or zero-height grid is legal: the
layout pass can produce one mid-resize, and a renderer that panicked on it would be the worse
failure.

`Renderer` turns a grid into bytes. It is a pure function of grid, cursor, and `TermCaps` into an
owned buffer — it never writes to a descriptor, which is what lets a fake grid be rendered in a
unit test against an exact expected byte string. The caller writes the buffer wherever it likes.
`render_full` remains the resync and geometry-change path; `render_rows` repaints only validated,
coalesced row indices and never clears the outer terminal. The local composition path uses the
same split, so a complete snapshot that contains only unchanged rows still costs no repaint.

Three rendering invariants:

- **Frame order is hide, clear, paint, reset, place, show.** Nothing is ever seen half-drawn.
- **Every SGR sequence leads with a `0` reset**, so it describes the target rendition absolutely
  rather than as a delta. A dropped or reordered frame cannot leave a cell wearing a stale
  attribute. Runs of identical style still emit one sequence, not one per cell.
- **A capability the client does not have is never emitted.** A `Color::Rgb` on a terminal
  without `truecolor` is downsampled to the nearest 256-palette entry — the greyscale ramp for
  near-greys, the 6x6x6 cube otherwise — rather than sent and hoped for.

Escape sequences are emitted only from this module. A pane's bytes reach the outer terminal
re-rendered from parsed cells, never forwarded, so no pane can drive the user's terminal through
the renderer.

`render_spans` is the third path, added at M2-03 for chrome. A `Span` is a run of cells with its
own origin, because a header or a status row belongs to the client alone and can sit anywhere,
while pane *contents* always come from a validated `Grid` at column zero. Keeping chrome on its own
path is what stops client-composed cells from ever being mistaken for server-owned ones.

#### Chrome

`cloo-client::chrome` turns a pane description — index, title, task label, attention state, focus,
zoom — into the cells of its header row, and applies the dimming policy to an unfocused pane's
body. It is a pure function into cells: it emits no bytes, so the renderer remains the only place
escape sequences are produced, and a narrow header is testable against an exact string.

Two rules from the style guide are structural rather than cosmetic. Width is spent in a fixed
order, so every pane on a screen degrades identically — the task label goes first, then the state's
text label, then the title truncates, and the state glyph is the last thing standing. And colour is
never the only signal: every attention state carries an ASCII glyph and, wherever it fits, its text
label. Focus is not an attention state; it changes the accent and the marker and never the glyph.
See [`STYLEGUIDE.md`](STYLEGUIDE.md) and `DECISIONS.md` RESOLVED-14.

The palette here is the reference `storm` theme, as constants. Named themes and terminal-palette
inheritance land in M4-03; nothing in this module reaches session state, which is what makes that
a client-local change.

#### Overlays

`cloo-client::overlay`, as of M3-04, is the session switcher, the profile launcher, and the
pane-details view as *one* model and one renderer: an `Overlay` is a list, a keyboard cursor, and
a title, and the three differ only in what a row says and what confirming one means. Like `chrome`
it is a pure function into cells, so a row is testable against an exact string.

Two properties are types rather than rules. A `LaunchRequest` carries a `ProfileId` and has no
constructor but confirming a launcher row, and a launcher row has no constructor but a validated
`cloo_core::Profile` — so a launch cannot name anything the configuration did not define, and
there is no free-text command to type into. And `OverlayAction::Dismiss` answers `Dismissed` from
every state, including an empty list, so no overlay can hold the terminal. The keyboard vocabulary
lives beside the attention queue's in `cloo-client::input`, because an open overlay owns the
keyboard exactly as chrome owns a mouse click over a border; none of it reaches a child. The
visual contract — the shared width ladder, the text selection marker, the dimmed backdrop — is in
[`STYLEGUIDE.md`](STYLEGUIDE.md#overlays-and-notifications).

#### Motion

`cloo-client::motion`, as of M4-04, is the style guide's transition vocabulary: focus, split,
close, and overlay, and nothing that arrives on a data clock. A transition is described in *frames*
rather than milliseconds — seven whole render ticks, which fits the 120ms target without asking for
a repaint the frame cap would refuse — and `Motion::tick` answers nothing on a step it already
drew, so sampling faster than the budget costs nothing at all.

Interruption is settling, not rewinding: `Motion::interrupt`, called for input, a resize, or a
state change, ends the transition at its end state, and a settled `Phase` leaves every cell
unchanged. A frame drawn after an interruption — or under reduce-motion, which starts every
transition settled — is therefore byte-identical to one from a client that animates nothing.
Motion changes contrast and never position or character: chrome that slid would be hit-tested where
it was not drawn, and a ramp that faded to nothing would fail the readability rule dimming follows.
Time is a parameter rather than a clock the module reads, which is what makes a transition testable
frame by frame. `Renderer::render_transition` is the only place a phase becomes bytes, and it
paints chrome spans alone — motion can never repaint a pane's contents.

#### Raw mode

`cloo-client::raw_mode::RawMode` is an RAII guard over one terminal descriptor. Restoration is by
ownership, matching the PTY layer, and covers four paths with the same restore:

| Path | Mechanism |
|---|---|
| Normal | `RawMode::restore`, or `Drop` if the caller never calls it |
| Error | `Drop` while an error unwinds out of the client |
| Panic | a panic hook installed on first entry, chained to the previous hook |
| Signal | `SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT` handlers that restore, then re-raise |

The same four paths also write back any reporting modes the client turned on in the outer
terminal. `RawMode::on_restore` registers the reset sequence — the bytes `OuterModes::disable`
produces — and every restore writes it before putting the `termios` back. A terminal left
reporting mouse motion into a shell that knows nothing about it is the same class of bug as one
left raw. A sequence longer than `MAX_RESET_LEN` is refused rather than truncated, and the stored
sequence is *taken* on the first restore, so a panic that unwinds through the hook and then drops
the guard writes it once.

The panic hook and the signal handlers cannot borrow the guard, so the saved `termios` also lives
in a process-global restore slot that the guard arms on entry and disarms on restore. The slot is
a three-state atomic (`IDLE`/`ARMING`/`ARMED`) plus the payload, so a handler firing mid-arm sees
`ARMING` and reads nothing. A handler's only libc call is `tcsetattr`, which POSIX lists as
async-signal-safe: no allocation, no locking, no `Mutex`. Only one guard may be armed per process;
a second `enter` is refused with `AlreadyActive` rather than overwriting the saved state. Signal
handlers restore the default disposition and re-raise rather than calling `exit`, so the wait
status a parent shell sees is the one it expects from a signalled child.

#### Outer terminal

`cloo-client::outer` is the geometry of the terminal the client draws into, read with
`TIOCGWINSZ`. A terminal that reports a zero-width or zero-height `winsize` gets a conventional
80x24 rather than an error.

`cloo-client::capabilities` is the next piece: what that terminal can *do*, from `TERM` and
`COLORTERM`. Detection is a pure function of those two values so it is testable without touching
the process environment, and it claims only what can be established without writing a query
sequence and waiting for a reply — everything else stays false and takes its documented fallback.
True-colour detection is factored out as the named `truecolor_from_env` (M7-01): `COLORTERM` is
`truecolor` or `24bit` (case-insensitive), or `TERM` names a direct-colour entry (`*-direct`). A
`256color` entry is not truecolor, so it establishes nothing — a wrongly claimed truecolor corrupts
the screen while the downsample fallback never does, so the ambiguous case answers `false`.
Both belong to the client, never to session state, which is what keeps a capability difference
between two attached clients from becoming something the server has to model.

The module has two entry points, and the difference between them is the whole of RESOLVED-12:

| Entry point | `TERM` resolves | `TERM` unset or `dumb` |
|---|---|---|
| `attach_caps` / `detect_attach_caps` | negotiates, degrading per the table below | `CapsError`, converted into `AttachError::Capabilities` before the socket is touched |
| `caps_from_env` / `detect_caps` (local pane) | the same negotiation | every capability false, and the pane runs |

`Capability` enumerates the fields of `TermCaps` in a form that can be named and paired with a
`Fallback`; `degradations(caps)` is the list of baseline capabilities a given attach lacks, each
with the behaviour taken in its place. The fallback for a capability is fixed, not per client —
two clients of one session must not behave differently for the same missing capability.

| Missing capability | Fallback |
|---|---|
| `truecolor` | `Color::Rgb` downsampled to the nearest 256-palette entry |
| `bracketed_paste` | pasted text forwarded as ordinary typed input |
| `sgr_mouse` | chrome driven from the keyboard only; no mouse mode sent to the pane |
| `focus_events` | the client is treated as always focused |
| `extended_keys` | legacy key encoding |
| `clipboard_osc52`, `hyperlinks`, `graphics` | the typed effect is suppressed |

#### Noticing a resize

`cloo-client::resize` turns `SIGWINCH` into an awaitable report of the outer terminal's new
geometry, as of M1-03. The signal itself carries no size, so it is always paired with a
`TIOCGWINSZ` afterwards. Two properties let a `ResizeWatch` sit in a `select!`: it is
**cancel-safe** — the only suspension point is the underlying signal receive, and the size is read
with no `await` in between, so a watcher dropped mid-`select!` has consumed nothing — and it
**reports changes, not signals**, swallowing a `SIGWINCH` whose geometry turns out to be
unchanged. That filter is not cosmetic: a resize costs a layout pass, a grid reflow, and a
`SIGWINCH` delivered to the child, and a child redrawing for a size it already had is exactly the
flicker worth avoiding.

The new size becomes a `Command::Resize` on the session channel like anything else. The client
never resizes a grid or a PTY itself.

#### Input routing

`cloo-client::input` is the other half of what the client does with the terminal it sits in, as of
M1-07. Four pieces, composing in one direction:

- **`OuterModes`** — which reporting modes cloo asks the outer terminal to turn on, derived from
  the negotiated `TermCaps` and from nothing else. A capability that could not be established is
  simply not asked for, which is the same silence its fallback already describes. `enable` and
  `disable` are exact inverses, and the reset is registered on the raw-mode guard so it also runs
  on a panic or a signal.
- **`InputDecoder`** — the terminal's byte stream, split back into `InputEvent::{Keys, Paste,
  Focus, Mouse}`. A sequence is recognised only for a mode cloo actually requested: `ESC [ I` is a
  legitimate thing for a program to send when focus reporting was never enabled, and stealing it
  would corrupt input belonging to the pane. A sequence split across two reads is held rather than
  mis-decoded, and because a lone `ESC` is a prefix of every sequence here, the run loop calls
  `flush` on the frame tick — that is what makes the Escape key reach a pane at all.
- **`ScreenLayout` / `route_mouse`** — where a report landed and who owns it. `mouse_owner` is the
  ownership rule on its own, for a caller that has already hit-tested.
- **`decode_key` / `KeyRouter`** — the keyboard's ownership question, added at M4-02. `decode_key`
  turns a run of key bytes into a `cloo_core::keymap::Key`; the router decides whether that chord
  is cloo's at all.

The encoding sits on the far side. `ClientMessage::Paste`, `Focus`, and `Mouse` carry *what
happened*; `cloo-server::session` turns each into bytes using the modes the pane's own application
negotiated, which the client cannot see and is told about in `ServerMessage::Modes`. See
[DECISIONS.md](DECISIONS.md) RESOLVED-13. Concretely:

| Event | Encoded as | When the application asked for nothing |
|---|---|---|
| Paste | `ESC [ 200~` … `ESC [ 201~` | the text alone, as ordinary typed input |
| Focus | `ESC [ I` / `ESC [ O` | nothing is written |
| Mouse | SGR `ESC [ < code;col;row M\|m`, else legacy X10 | nothing is written |

Two details are load-bearing rather than incidental. Pasted text has any paste delimiter *inside*
it stripped and its line endings normalised to carriage returns, because otherwise pasted content
could close the bracket early and have the rest of itself run as typed input — the injection
bracketed paste exists to prevent. And a mouse event is filtered by tracking level before it is
encoded: a bare pointer move is silence under click-only tracking, and a cell the legacy encoding
cannot address is dropped rather than sent with a wrong coordinate.

##### Mouse ownership

Routing one report is two questions, and as of M6-01 `route_mouse` answers both in one pass:
*where did it land*, then *whose is that place*.

**Where.** `ScreenLayout` is the client's description of what it drew — the terminal size, which
rows the tab bar and status bar took, which pane is focused, and each visible pane's grid rectangle
in the outer terminal's own cells. It is built from what was rendered rather than re-derived from
the wire, because a hit test has to agree with the picture the user is pointing at. `hit` answers
in a fixed order, and the order is the safety property: off-screen first, then the chrome rows,
then the pane grids, then the header rows, and gutter otherwise. A layout that wrongly described a
pane as overlapping the status bar still cannot deliver a status-bar click into a child, and a
header still cannot swallow a cell some pane's grid actually occupies. Per
[STYLEGUIDE.md](STYLEGUIDE.md) the header row *is* the pane's top border, so it is chrome and never
contents.

**Whose.** Three rules decide it, in order, and any one of them alone is enough to hand an event to
chrome:

1. The pointer is not over the pane whose modes cloo holds — a border, the status bar, and any pane
   other than the focused one, since `ServerMessage::Modes` reports the focused pane and guessing at
   another application's tracking level is exactly the claim that would steal or invent an event.
   Clicking an unfocused pane is therefore chrome's, which is also what a user means by it.
2. Shift is held. This is the conventional multiplexer override, and the only way to reach chrome
   inside a pane run by a full-screen application.
3. The application is not tracking the mouse, so it cannot own a mouse event. This is what makes
   click-to-focus work in an ordinary shell.

The tracking *level* is deliberately not a fourth rule: a bare pointer move under click-only
tracking is the application's event to be dropped by the encoder, not the client's to reroute, and
rerouting it would turn a drag over a pane into a chrome action the user did not ask for.

**A chrome event never reaches the wire.** Forwarding one would put escape bytes into the child's
input, where they appear as garbage. That is why `MouseRoute` has two differently shaped arms:
`Application` carries the `MouseEvent` the wire takes, and `Chrome` carries a `ChromeTarget` — tab
row, status bar, a pane header, a pane body, gutter, or off-screen — which has no wire form at all,
so there is nothing a caller could send by mistake. Each chrome target names what it needs to be
acted on without a second hit test.

The server does not take the client's word for any of it. `Session::deliver_mouse` re-checks that
the named pane is visible and encodes from that pane's own modes, so a client cannot write into an
arbitrary child and an application that never asked for the mouse hears nothing.

##### Chrome mouse actions

What cloo *does* with the reports it kept is `ChromeMouse`'s answer, as of M6-02. One rule shapes
all of it: **every gesture maps onto commands that already exist.** A gesture reachable only with a
mouse would be unreachable on a terminal that reports none, which the capability fallback makes an
ordinary case rather than an exotic one — so `ChromeAction::commands` is the whole vocabulary, and
it returns `Action`s the keyboard sends too.

| Gesture | `ChromeAction` | Commands | Keyboard equivalent |
|---|---|---|---|
| Click a pane body or its header | `Focus(pane)` | `FocusPane` | `focus-left` and its three siblings |
| Drag a divider | `Resize { pane, dir, delta }` | `ResizePane` | — (a drag is a pointer distance) |
| Wheel over a pane | `Scroll { pane, up, lines }` | `FocusPane`, `EnterCopyMode`, `CopyMotion` ×3 | `enter-copy-mode`, `copy-up`/`copy-down` |

`FocusPane` and `ResizePane` name a *pane*, which a keypress cannot supply, so neither has a keymap
spelling in either direction — the same shape `RenameTab` and `CopySearch` take for text. A wheel
focuses the pane it is over before it scrolls, because copy mode is the focused pane's; it enters
copy mode only when the server has not already reported that pane in it, since copy mode is session
state a second client may have entered.

**A drag changes ratios only.** `ScreenLayout::divider` finds the divider from the pane rectangles
alone — a pane's trailing edge one cell before the cell pointed at, another pane's leading edge one
cell after it, and shared extent on the perpendicular axis — which covers both the one-cell gutter
between side-by-side panes and the header row between stacked ones, since that row *is* the lower
pane's top border. The press records the divider and commands nothing, so a drag can never also
focus; each motion emits a delta measured from the last one, so a drag never applies its distance
twice. The delta crosses the wire in **cells**, never as a ratio: ratios never cross the wire, and a
client computing one would be doing arithmetic over a split extent only the layout tree knows.
`Layout::resize` turns it into exactly one new ratio on the pane's nearest ancestor split along that
axis, clamped so both halves keep `MIN_PANE_SIZE` when the extent can hold two of them — a drag past
the end stops at the end rather than being refused. The tree's shape is untouched, so no pane is
created, closed, reordered, or restarted; each affected child costs one `TIOCSWINSZ` from the
ordinary geometry pass.

`Command::FocusPane` and `Command::ResizePane` carry no reply for the same reason `MoveFocus` does
not: a click or a drag crossed the wire against a screen at most one frame old, so a pane that has
closed — or that a zoom is hiding — is dropped exactly as a stale mouse event is. A refusal would be
a message about a pane the user can no longer see.

The local smoke path draws no chrome and has one full-screen pane, so click-to-focus and a drag have
nothing to move there; the wheel does, and it goes through the same `ChromeAction::commands` list
rather than a path of its own.

##### Keys and the prefix

The keymap itself is `cloo-core`'s — see [Keymap](#keymap) — and resolving it against a real
terminal is `cloo-client::input`'s, as of M4-02.

`decode_key` names the first chord in a run of key bytes and says how many bytes it took, using the
conventional encodings: `0x01`–`0x1a` are control letters, `ESC` before anything but a sequence
introducer is that chord with alt, `CSI`/`SS3` carry the arrows, editing keys, and function keys —
including the `;modifier` parameter forms — and a bare `ESC` is Escape. Two pairs are deliberately
kept apart: `0x0d` is Enter while `0x0a` is `C-j`, and `0x7f` is Backspace while `0x08` is `C-h`.
A sequence cloo does not model answers `None`, which a caller must treat as the pane's rather than
as an unrecognised command — the same rule `InputDecoder` follows for a mode that was never
negotiated.

`KeyRouter` is the prefix state machine, and it has one safety property: **outside a pending
prefix, every byte is the pane's.** A chord in the table means nothing until the prefix is pressed,
so an application using `c`, `x`, `q`, or an arrow key never notices cloo is there. Pass-through
bytes are a copy of the slice that arrived, never a re-encoding of a decoded chord, since a client
that re-encoded would corrupt input the first time it guessed at a convention differently. Only the
single chord after the prefix is looked up; anything after it in the same read is passed through
again.

| `KeyRoute` | Meaning |
|---|---|
| `Pane(bytes)` | the user's bytes, verbatim, for the focused pane's child |
| `Pending` | the prefix was pressed; nothing reached the child, and the status bar shows it |
| `Command(Action)` | a bound chord, sent as `ClientMessage::Command` |
| `Unbound` | a chord after the prefix that no binding names — consumed, never typed |

An unbound chord is consumed rather than delivered because the user was talking to cloo; passing it
on is how a mistyped command ends up in a shell. Pressing the prefix twice sends the prefix itself
to the child, which is tmux's `send-prefix` and the only way to type a `C-b` into a program that
wants one.

### The binary

`crates/cloo` is the composition root and nothing else: it parses the command line and wires the
two halves together. It holds no session state and emits no escape sequences of its own.

As of M0-07 it runs the M0 smoke path in `local.rs` — one PTY, one grid, one renderer, all
in-process, with no socket and no detach. The loop is already shaped like the real one: the server
half owns the PTY and the authoritative grid, the client half owns raw mode and every escape
sequence, and the binary only moves snapshots one way and commands the other. As of M1-03 it holds
a `SessionHandle` rather than a reactor, so the local path mutates session state through exactly
the same `mpsc<Command>` the daemon does — one serialized owner, no second path, no `Mutex`.

Three ordering rules matter. Raw mode is entered *before* the child is spawned, so a failure that
is going to happen happens while the terminal is still untouched and there is nothing to clean up.
The render is driven by a ~60fps frame timer rather than by PTY readiness, so a fast producer
coalesces into at most one frame per tick — the render-rate cap is architectural from the first
line of the loop, not a later optimization. And a `SIGWINCH` becomes a resize *command*, so the
grid reflow and the child's `TIOCSWINSZ` happen in one place in one order.

Stdin is read on a dedicated thread rather than through an async descriptor: making descriptor 0
non-blocking would change a file description the user's shell shares, and a shell left
non-blocking after cloo exits is a worse bug than a parked thread.

M1-01 added the socket lifecycle beneath this loop, M1-02 added a daemon and an attach client over
it, and M1-03 put the session task under both, but none of them changed the CLI surface: `cloo`
with no arguments still runs one pane in-process. `cloo attach` and `cloo new` land with the
detach and reattach flow in M1-05. `crates/cloo/tests/attach.rs` drives the daemon and the client
against each other in the meantime — that end-to-end coverage has to live in the binary crate,
since `cloo-server` may never name `cloo-client`, not even as a dev-dependency.

### Agent pane metadata and attention

The server owns a pane's explicit metadata: user-visible name, optional task label, working
directory, and profile (`generic`, `codex`, `claude`, or a configured local profile). It also
owns the pane's attention state and its source. The client renders these values in pane chrome,
the status bar, and attention navigation; it never determines them by reading terminal cells.

Attention is deliberately provenance-aware. A bell, process exit, manual mark, or opt-in local
adapter may set a state such as `needs_input`, `ready`, or `failed`. A live PTY alone is not proof
that a harness is working, so the default for an uninstrumented child is `unknown`. Screen
scraping a Codex or Claude transcript is prohibited: it is brittle, locale/theme dependent, and
would make the rendered grid a second source of truth.

M2-04 lands the model for both halves in `cloo-core`, as pure data with no I/O and no vendor
dependency:

- `cloo-core::profile` — `Profile { id, command, default_name, min_size, adapter }`. The built-in
  `generic`, `codex`, and `claude` are three *values* of that struct, not three code paths, and a
  configured local profile is built by the same constructor and checked by the same
  `Profile::validate`. `ProfileCommand` is `LoginShell` or an explicit `Program { program, args }`
  — an argv, never a shell string, so no metadata can be word-split on the way to `execvp`.
  Resolving `$SHELL` and finding the executable on `PATH` are launch-time answers the server owns.
- `cloo-core::pane` — `PaneMeta { profile, name, task, cwd, min_size, adapter, attention }`, with
  `PaneName`, `TaskLabel`, and `WorkingDir` as validated newtypes. A working directory must be
  absolute (a relative one means the *daemon's* cwd, not the user's) and control characters are
  rejected in every user-supplied field. `adapter` is copied from the profile the pane was launched
  under and is the pane's whole consent for the M2-09 control interface — a pane carries the opt-in
  it was launched with, not whatever a later configuration reload says.
- `Attention { state, source, acknowledged }` keeps `AttentionState` — the six states of
  [STYLEGUIDE.md](STYLEGUIDE.md#agent-workspace-states) — beside an `AttentionSource` of `None`,
  `Bell`, `Lifecycle`, `User`, or `Adapter(AdapterId)`. Only the adapter variant is advisory.
  `Attention::set` clears acknowledgment when the state *changes* and keeps it when the same state
  is re-reported, which is the queue's coalescing rule stated once rather than in every source.

Validation is entirely pure: it checks shape, never the filesystem or `PATH`. Rejections are
`MetadataError`, the sibling of `LayoutError` — nothing is partially applied.

M2-05 adds `cloo-core::config`, which parses the *text* of `config.toml` into a validated `Config`
and merges local profiles over the built-ins. It takes a string and never a path, because reading
the file is I/O and therefore the server's; a local profile is built through the same public
constructors a built-in uses, so configuration can express exactly what a built-in can and no more.
Two failure modes are kept apart on purpose: a document error — malformed TOML or an unknown key —
returns `ConfigError`, while a well-formed profile that fails `Profile::validate` is dropped alone
with a `ConfigWarning` and the rest of the document still loads. An unknown key is never ignored,
since a silently dropped key is a setting the user believes is applied. Overriding a built-in
replaces it in place rather than appending, so the launcher order a user learned survives their
override. M4-01 puts file I/O in `cloo-server::config`: `CLOO_CONFIG` wins over
`XDG_CONFIG_HOME/cloo/config.toml` and the `$HOME/.config` fallback, a missing file means defaults,
and a startup read failure warns before using defaults. Its `ConfigManager` loads and validates a
whole replacement before one assignment changes the live value; a failed `SIGHUP` reload therefore
keeps the prior valid configuration. M2-06 launches from profiles. M4-02 adds the `[keys]` table to
the same document and the same warning rules — see [Keymap](#keymap) — so a `Config` now carries
the prefix and its bindings alongside the profiles.

M2-07 makes the session actor the one serialized path for that attention state. `Command::SetAttention`
and `Command::AcknowledgeAttention` arrive on the same `mpsc` as every other mutation, so the
coalescing rule in `Attention::set` — re-reporting a state keeps its acknowledgment — cannot be
raced by a chatty source; a report naming a pane that has since closed is dropped exactly as a
stale mouse event is. `SessionSnapshot` projects each pane's attention from the same
`Layout::resolve` pass as its identity, so a client is never told a pane's state without also being
told who the pane is, and the daemon fans it out as `ServerMessage::Attention` on the attention
clock.

M2-08 wires the generic sources the session observes for itself into that same `SetAttention`
path. A terminal bell — surfaced by `cloo-term::Emulator::take_bell`, a coalesced flag set from the
backend's bell event rather than an outer-terminal effect to forward — becomes `needs_input` with
`Bell` provenance. A child's exit, seen as the PTY reaching end of file, becomes `ready` on a clean
exit and `failed` on any other status, both with `Lifecycle` provenance; the exit code is read with
a non-blocking reap (`PtyReactor::try_exit_status`, cached so the shutdown wait agrees) rather than
guessed. An explicit user mark reaches the same command with `User` provenance. None of the three
reads the pane's grid — no transcript or process-name matcher exists.

M2-09 adds the one advisory source: the local adapter control interface. It is a **second socket**,
`<session socket>.control`, derived from the session socket by `socket::control_path_for` and bound
by `Daemon::new` under the same lock, stale-cleanup, and unlink-on-drop rules. An adapter is not a
client and does not speak `ClientMessage`: `cloo-proto::adapter` is a separate vocabulary of
`AdapterMessage::{Hello, Report}` and `AdapterReply::{Ready, Refused, Applied, Rejected}`, so "an
adapter may only report attention" is a property of the enum it can encode rather than a refusal
some branch has to remember. `AdapterState` likewise carries only `working`, `needs_input`,
`ready`, and `failed` — an advisory source cannot assert `quiet` or withdraw an observed state to
`unknown`.

Two gates and no more. `conn::accept_adapter` validates the version and the announced name through
the same `AdapterId` alphabet a profile uses, because that name is rendered as provenance; then
each report goes to the session actor as `Command::AdapterReport`, which applies it only if the
pane's own `PaneMeta::adapter` — copied from the profile it was launched under — names that
adapter. That is what "opt-in" means: a profile's `adapter` field is the user's consent, no
built-in sets one, and a pane that named none is reachable by no adapter. The provenance is stamped
by the session from the announced name, never taken from the report, so an adapter can say what it
thinks and never that a bell, an exit, or the user said it. Every report is answered — `Applied`,
or `Rejected` with `UnknownPane`, `NotPermitted`, or `SessionEnded` — because an adapter is a
separate program and a silent drop is indistinguishable from success to a script.

M5-01 puts copy mode and regex search in that same actor. A pane owns its `CopyMode` state — a
retained-scrollback cursor, an optional linear selection, and the current regex results — while
the emulator retains the text and viewport. Entry, visual selection, vim-like motion, search, and
exit all cross the one `mpsc`; a malformed regex returns a normal reply and keeps the last valid
search rather than failing the actor. New output re-reads retained history only when that pane has
an active copy search, so an inactive copy surface never makes the PTY hot path traverse
scrollback. `CopyModeState` travels independently of damage, so an attach or resync receives the
selection and highlights another client left behind without making client chrome an authority over
scrollback.

M5-02 adds the client half and the copy itself. Every copy-mode command is an `Action` on the
wire — entry, exit, motion, selection, search, and match navigation — routed by the daemon
coordinator into the session handle that already existed, so a rebound key is still not a protocol
change. `CopyModeState::viewport_top` carries the retained line drawn on the pane's first visible
row: server positions are absolute in history while a client holds only the visible grid, and that
one number is what joins them rather than a client-side guess. `cloo-client::copy_mode` projects
the state into positioned highlight spans and one status row, reading the client's `Grid` and never
writing to it — a selection is a rendition, and a client that wrote one into its cache would
disagree with the next damage frame about what the pane says. Match, selection, and cursor each
differ in *attributes* as well as colour, so the three stay apart on a terminal without a palette,
and a position outside the viewport is dropped rather than clamped onto the nearest visible row.

The copy is explicit and answered privately. `Action::CopySelection(target)` is handled by the
socket task, not the coordinator: the session extracts the selected text from retained scrollback
and the effect is written back to the one client that asked, because broadcasting it would put one
user's selection in every attached terminal's clipboard. It arrives as an ordinary
`OuterTerminalEffect::ClipboardStore`, so the M1-09 gate is unchanged — a client whose policy or
terminal cannot store a clipboard writes nothing, and it does not even send the request, so a
user's scrollback never crosses the wire to be discarded. Copying mutates nothing: not a grid cell,
not the cursor, not the selection.

---

## Concurrency

Tokio, actor-shaped rather than shared mutable state:

- One task per PTY, reading into that pane's `cloo-term`.
- One **session task** owning all session state — the only thing that mutates grids and layout.
- One task per attached client, holding a `broadcast` receiver for damage.

Everything reaches the session task through a single `mpsc<Command>`. There is no `Mutex` on
session state. The daemon coordinator alone captures and compares snapshots; its bounded
`broadcast` channel is deliberately allowed to lag, because a receiver resyncs from the session
actor rather than backpressuring it. Expect bugs in PTY/resize *ordering*, not in lock discipline.

The session task is real as of M1-03 — see [The session task](#the-session-task). It owns every
pane's PTY directly rather than talking to a separate per-PTY task, and as of M2-01 it owns
several of them, pumping the set with a rotating `select_all`. Giving each PTY its own task is
still open; it would change nothing about the rule, since a PTY task would reach session state
only through the same channel.

---

## Wire Protocol

Length-framed postcard over the Unix socket. Implemented in `cloo-proto`.

```
Client → Server:  Attach { protocol_version, size, term_caps, session }
                  Detach  Input(Vec<u8>)  Paste(Vec<u8>)
                  Focus { focused }  Mouse(MouseEvent)
                  Resize(Size)  Command(Action)

Server → Client:  Hello { protocol_version, session, tabs, size }
                  Refused { reason }
                  Damage { pane, rows: Vec<RowUpdate> }
                  CursorMoved { pane, pos, shape, visible }
                  Modes { pane, modes: PaneModes }  Effect { pane, effect }
                  Layout(LayoutSnapshot)  Panes(Vec<PaneInfo>)
                  Attention(Vec<PaneAttention>)
                  CopyMode(Option<CopyModeState>)
                  Bell(pane)  Tabs(Vec<TabSummary>)
                  Detached  Exit(code)
```

`Panes` is who the panes are — profile, name, task label, working directory — as opposed to
`Layout`, which is where they sit. The two are separate messages because they change on
completely different clocks: geometry moves on every resize, identity only when a pane is
launched, closed, or renamed, and a full-screen drag must not resend every pane's name. It is
sent whole rather than per pane, so a client replaces its map and can never hold an entry for a
pane that no longer exists. Every field of a `PaneInfo` was supplied by the user or by the
profile's defaults; none of it is ever derived from a pane's grid.

`Attention` is a third clock again: `PaneAttention { pane, state, source, acknowledged }` for
every pane, resent only when some pane's attention actually changes, so a rename does not drag
state and a state change does not drag names. State and source travel together — a state without
its provenance is exactly the claim the chrome must not make — and an uninstrumented pane is
carried as `Unknown`/`None` rather than omitted, because the client renders that state too and
never guesses one from the grid.

`Tabs(Vec<TabSummary>)` is a fourth, compact clock. `Hello` and every resync carry the whole
ordered bar, and a later `Tabs` update replaces the client's cache when creation, close, rename,
or selection changes it. The session actor projects it from `cloo-core::session::Session` in the
same turn as the active tab's snapshot; inactive tabs keep their PTYs and continue pumping, but
only the active tab's layout, grid, metadata, attention, cursor, and modes are sent. A switch
therefore changes view state and active geometry, never a child's identity or process lifetime.

### Framing

Each frame is a big-endian `u32` payload length followed by that many bytes of postcard.
Postcard is not self-delimiting over a stream, so the prefix is what tells a reader it holds a
whole message. `cloo_proto::encode` produces a complete frame; `decode` takes one off the front
of a buffer and reports how many bytes it consumed, so a reader drains and calls again.

Two guards matter on the socket path. A partial buffer returns `ProtoError::Incomplete` — read
more and retry, never an error to report. A length prefix above `MAX_FRAME_LEN` (16 MiB) is
rejected *before* anything is allocated for it; a frame that large is a desync or a hostile
peer, not a real message.

`cloo_proto::stream::FrameStream` pairs that arithmetic with a transport, so the drain-and-retry
loop exists once rather than once per side of the connection. It lives in `cloo-proto` because
both halves need it and neither may depend on the other, and it is generic over the transport —
`UnixStream` in production, a duplex pipe in a test. It draws one distinction the callers rely
on: a clean end of stream *between* frames is `Ok(None)`, because a peer that closed its side is
ordinary, while bytes that stop *inside* a frame are `StreamError::Truncated`, which is a real
error. This is the one place `cloo-proto` names an external runtime (`tokio`); it still knows
nothing about PTYs or rendering.

### Handshake

**The handshake is versioned from day one.** A stale client will attach to a newer server the
first time anyone rebuilds mid-session. A clean "version mismatch, reattach" beats a protocol
desync that presents as a rendering bug.

`PROTOCOL_VERSION` lives in `cloo-proto` and **must be bumped on every change to a wire type.**
`Attach` carries the client's version and `Hello` echoes the server's, so either side can catch
a mismatch before interpreting a single message. `check_version` returns
`ProtoError::VersionMismatch`, whose `Display` output is the user-facing reattach message; the
server relays it in `Refused { reason }` and closes the connection. The adapter control socket
shares the same number and the same check — `AdapterMessage::Hello` carries it and
`AdapterReply::Ready` echoes it — because both protocols are built from this one crate and two
version constants could only ever disagree by accident.

### Types on the wire

IDs are newtypes (`SessionId`, `TabId`, `PaneId`, `ClientId`), serialized transparently as
`u64`. Damage is carried a whole row at a time (`RowUpdate`) rather than per cell — a row is the
smallest unit worth the framing overhead and keeps the client's apply step a copy. `CellAttrs`
is a packed bitfield rather than a struct of `bool`s, because postcard spends a byte per `bool`
and this rides the render path.

`LayoutSnapshot` is the *flattened* result of a layout pass: each pane's resolved `PaneRect` in
cells. The authoritative tree of ratios stays in `cloo-core`. Ratios never cross the wire —
a client has nothing to do with them but draw the answer.

### Multi-client sizing

Two clients of different sizes render at the **minimum** of both; the larger letterboxes. This
matches tmux and is the least surprising. Per-client independent views push size out of the
session and into the client — post-v1.

### Terminal capability and outer-terminal effects

`Attach { term_caps }` reports the client terminal's baseline capabilities. The compatibility
baseline for an interactive pane includes UTF-8 and color rendering, alternate-screen handling,
cursor updates, bracketed paste, extended keyboard input, focus events, SGR mouse routing, and
resize. A client that lacks a required capability must choose a documented fallback rather than
pretend support; the fallback table lives with the client module that owns the decision, under
[Outer terminal](#outer-terminal).

Failing to *resolve* `TERM` at all is the one case that is refused rather than degraded. A client
attaching with an unset or `dumb` `TERM` is turned away with an actionable error, because there is
no baseline to negotiate from and a silently degraded remote session is the harder failure to
diagnose. The refusal is client-side and happens before the socket is touched: `TERM` is the
client's to read, and the server is told capabilities rather than asked to infer them from an
all-false `TermCaps`, which a capable terminal could also legitimately report. The in-process local
pane has no such negotiation and keeps running with every capability false. See
[DECISIONS.md](DECISIONS.md) RESOLVED-12; the two rules compose as *refuse when there is nothing to
negotiate from, degrade when there is*. Shipped as of M1-06; the modes those capabilities turn
into, and the routing built on them, are [Input routing](#input-routing) as of M1-07.

Some child programs emit sequences intended for the *outer* terminal: notifications, titles,
clipboard writes, hyperlinks, or graphics. These are not raw bytes to relay around the grid.
M1-08 parses backend title and OSC 52 store events into narrowly typed, versioned effects and
models the remaining allowlisted requests as title reset, hyperlink, notification, progress, and
explicitly unavailable graphics. Its type model has no raw OSC/DCS or graphics-payload variant.
As of M1-09, the session actor drains each value and the daemon broadcasts it as an ordered,
non-damage `Effect` frame. The client then applies title changes only when its local title policy
permits them, and OSC 52 stores only when both its clipboard policy and `clipboard_osc52` permit
them. Hyperlinks need a renderer-owned span, while notifications, progress, and graphics have no
safe standalone renderer yet, so all remain suppressed even under the broad supported-effects
policy. Effects must be safe to suppress and must never alter authoritative session state;
arbitrary passthrough is forbidden because clients can differ and because it can bypass cloo
chrome, damage accounting, and terminal-state restoration.

Inline graphics are an optional enhancement, never a compatibility requirement. If a terminal or
intermediate multiplexer cannot support graphics, the pane remains usable and cloo exposes no
broken placeholder. This is specifically relevant to Codex terminal pets, which are unavailable
inside tmux and Zellij according to the upstream documentation.

---

## Session and tabs

The top of the pure model, in `cloo-core::session` and `cloo-core::tab` as of M3-01. A `Session`
owns an ordered set of `Tab`s and tracks exactly one active tab; each `Tab` owns one `Layout` — its
own tree of panes — and a focused pane within it. Splitting, zooming, or focusing is tab-local, so
one tab never disturbs another. All of it is pure: the server drives PTYs and the client renders,
but "what tabs exist and which is showing" lives in this one place.

Two invariants mirror the layout tree one level up. A session is **never empty** — it is born with
one tab and always keeps at least one, so closing the last tab is refused (`SessionError::LastTab`)
rather than being a way to reach a session with nothing to render. And the **active tab always
exists** — every removal leaves the active pointer on a tab still present, so `Session::active_tab`
returns a reference rather than an `Option`.

The lifecycle is four operations with defined activation behavior:

- **create** appends a tab holding one full-area pane and makes it active — the tmux `new-window`
  reflex.
- **rename** and **select** touch only their target tab; select never reorders the bar.
- **close** removes a tab. When the closed tab was active, activation moves to the tab that slid
  into its place — the former right neighbour, or the new rightmost tab when the closed one was
  last. Closing any other tab leaves the active one untouched. Every rejection (an unknown tab, or
  the last tab) leaves the session unchanged, with the unknown check first so a bad ID never
  masquerades as the last-tab rule.

Tab IDs come from the same monotonic, never-reused allocators in `cloo-core::id` as pane IDs.

M3-02 makes that model authoritative in the session actor as well. The actor owns one global set
of pane reactors and the pure `Session` model whose tab-local layouts partition those panes; a
new tab starts its first child before it enters the model, and closing a tab removes its layout
before dropping exactly the reactors it named. `NewTab`, `CloseTab`, `NextTab`, `PrevTab`, and
`RenameTab` cross as `Action` values, so the client sends intent while the actor remains the one
writer. Switching applies the selected tab's resolved geometry but leaves all other reactors
alive and pumping, preserving their grids and child processes for a later return.

---

## Keymap

`cloo-core::keymap`, as of M4-02. Three things, kept separate on purpose:

- **`Key`** — one chord: a `KeyCode` (a printable character, a named key, or `F1`–`F12`) and its
  `KeyMods`. It owns a *spelling* and nothing else. `C-` is control, `M-`/`A-` are alt, `S-` is
  shift, and a key is either a character written as itself, case sensitively, or a name matched
  case-insensitively (`enter`, `escape`, `pageup`, `f5`, …). `Display` writes the canonical
  spelling and `Key::parse` reads it back, which is what keeps the parser and the documentation
  from drifting. Shift on a printable character is refused rather than accepted quietly: a
  terminal reports a shifted `a` as `A`, so such a binding could never fire. What *bytes* a
  terminal sends for a chord is the client's, not this crate's — see
  [Keys and the prefix](#keys-and-the-prefix).
- **The action vocabulary** — one kebab-case name per bindable `Action`, with `parse_action` and
  `action_name` as inverses over `ACTION_NAMES`. An action that needs text a chord cannot carry
  (`RenameTab`, `CopySearch`) has **no spelling at all**, so a binding cannot name a command the
  keypress could not supply an argument for; those are reached from a surface that can ask.
- **`Keymap`** — the prefix chord plus the table reached after it. The defaults are tmux's, with
  `C-b` as the prefix ([DECISIONS.md](DECISIONS.md) RESOLVED-04): `%` and `"` split, `x` closes,
  `hjkl` and the arrows move focus, `z` zooms, `c`/`&`/`n`/`p` are tabs, `[` enters copy mode, and
  `d` detaches.

Conflict resolution is one rule: `bind` replaces a key's action **in place** and returns what it
displaced. In place, because the order of the table is the order a user reads it in; returning the
displaced action, because overriding a default and colliding with an earlier binding are the same
operation here and different messages to whoever wrote the file. Two keys bound to one action are
not a conflict — that is how the arrows and `hjkl` both move focus.

Configuration is a `[keys]` table parsed by `cloo-core::config`:

```toml
[keys]
prefix = "C-a"                 # omit to keep C-b

[keys.bindings]
"|" = "split-vertical"         # an addition, or an override of a default
"x" = "none"                   # `none` removes a binding entirely
```

The same two failure modes as profiles, and for the same reason. TOML itself refuses a chord
written twice, so a duplicate is a document error rather than a silent last-wins. A chord that
cannot be spelled, or an action name cloo does not know, drops that one line with a
`ConfigWarning` and leaves the default it would have replaced in place. An unusable `prefix` in
particular keeps `C-b`: a prefix nobody can press is a session with no way out.

---

## Layout

Binary tree of containers and leaves, implemented in `cloo-core::layout` as of M0-03:

```rust
enum Node {
    Leaf(PaneId),
    Split { dir: Direction, ratio: f32, first: Box<Node>, second: Box<Node> },
}
```

`ratio` is the fraction of the parent's extent given to `first` — the left child for
`Horizontal`, the top child for `Vertical` — and is always inside the open interval `(0.0, 1.0)`.

`Layout::split` replaces a leaf with a `Split` holding the old leaf as `first` and the new pane
as `second`. `Layout::close` collapses the parent split, promoting the sibling *subtree* into
the parent's slot. `Layout::set_ratio` is the whole of resize: it walks to the pane's nearest
ancestor split on the requested axis and rewrites one `f32`. Nothing stores cell counts, so
nothing else needs updating.

`Layout::resolve` is the single layout pass. It flattens the tree into one `PaneRect` per leaf,
tiling the area exactly — no gaps, no overlap, no borders, since chrome is drawn client-side.
The server issues `TIOCSWINSZ` from those rects and puts them on the wire as a `LayoutSnapshot`.

Two rules that are easy to get wrong:

- **Store ratios, not cell counts.** This is what makes layout survive a terminal resize sanely.
- **Enforce a minimum pane size** and reject splits that would violate it, or you will create
  zero-width PTYs and correspondingly confusing shell behavior. `MIN_PANE_SIZE` is 20x3 cells.
  Every rejection — unknown pane, duplicate pane, out-of-range ratio, too small, last pane —
  returns a `LayoutError` and leaves the tree byte-for-byte unchanged.

The minimum is enforced at *split* time only. A layout pass over an area that shrank below the
minimum squeezes panes toward a floor of one cell per axis rather than dropping them: a resize
must always produce a drawable answer, and the ratios are still there when the terminal grows
back.

IDs are handed out by the monotonic allocators in `cloo-core::id` and are **never reused within
a session**. A recycled `PaneId` would let a stale client message land on a pane the sender
never meant, and the wire carries no generation counter to catch it.

---

## External Dependencies

| Dependency | Purpose | Required / Optional |
|---|---|---|
| `alacritty_terminal` | Terminal emulation — parser, grid, scrollback, alt screen, SGR | Required, **exact-version pinned** |
| `tokio` | Async runtime for the PTY reactor and socket | Required |
| `libc` | `openpty`, `TIOCSCTTY`, `TIOCSWINSZ`, `fcntl`, and termios | Required |
| `serde` + `postcard` | Wire serialization and framing | Required |

`serde` and `postcard` are wired up in `cloo-proto` as of M0-02. `alacritty_terminal` is pinned
at `=0.26.0` in `[workspace.dependencies]` and reaches only `cloo-term`, as of M0-04. `tokio`
(features `macros`, `net`, `rt`) and `libc` land in `cloo-server` with the PTY reactor as of
M0-05; the `net` feature is what provides `AsyncFd`, not sockets. M0-07 adds the `sync` and
`time` features for the binary's run loop: `sync` carries stdin bytes from the reader thread, and
`time` is the frame timer that caps the render rate. M1-03 adds `signal`, which is what makes
`SIGWINCH` an awaitable event rather than a global flag a handler has to poke; `sync` also carries
the session task's `mpsc<Command>` from that milestone on.

`wezterm-term` is the designated fallback emulation backend: more deliberately public API,
heavier dep tree. Re-evaluate at M2 if the pin hurts. See [`DECISIONS.md`](DECISIONS.md) —
RESOLVED-02.

---

## Deployment Targets

cloo is a locally installed binary. There is no hosted environment.

| Channel | Artifact | Notes |
|---|---|---|
| crates.io | `cloo` | `cargo install cloo` — builds from source |
| npm | `clooterminal` | Prebuilt per-platform binaries as optional deps, esbuild/swc pattern |
| Local dev | `cargo run -p cloo` | — |

Supported platforms: `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`. Windows is out
of scope for v1.

The npm package name is `clooterminal`, not `cloo` — npm's similarity filter rejects `cloo` as
too close to existing packages. The installed command is `cloo` either way, via the `bin` field.
See [`DECISIONS.md`](DECISIONS.md) — RESOLVED-05.

See [`ENV_VARS.md`](ENV_VARS.md) for the runtime variable matrix.

---

## Constraints

Hard architectural rules. These are the invariants that protect the design.

- **Only `cloo-term` may import `alacritty_terminal`.** No exceptions anywhere else in the
  workspace. The pin plus this boundary is the entire mitigation for upstream API churn.
- **Pin `alacritty_terminal` to an exact version.** Upgrade deliberately, never transitively.
- **The server owns all state.** Clients cache the visible grid and nothing more.
- **All session mutation goes through the session task** via `mpsc<Command>`. Never add a
  `Mutex` to session state.
- **Chrome is client-side.** The server never decides what anything looks like.
- **Coalesce damage and cap render rate (~60fps).** Never emit one update per PTY read — this
  is architectural, designed in at M1, not a later optimization.
- **Version the wire handshake** on every protocol change.
- **Every visual choice must survive a plain 16-color TTY.** Detect capability, degrade
  deliberately, never let the fallback look accidental.
- **Harness status is explicit.** Store a state and its source; never infer it by screen-scraping
  terminal output.
- **Outer-terminal effects are capability-gated and allowlisted.** Never relay arbitrary OSC/DCS
  bytes around the renderer or directly to a client terminal.
