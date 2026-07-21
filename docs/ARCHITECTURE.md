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
| `cloo-proto` | Wire types, framing (serde + postcard), the framed async transport, handshake version | Know anything about PTYs or rendering |
| `cloo-term` | Thin wrapper over `alacritty_terminal` — feed bytes, read cells, resize, scrollback | Leak `alacritty_terminal` types across its public API |
| `cloo-core` | Session/tab/pane model, layout tree, keymap, profiles, pane metadata, config | Perform I/O |
| `cloo-server` | Daemon: socket, PTY reactor, damage tracking | Decide what anything looks like |
| `cloo-client` | Attach, raw mode, renderer, theming, input encoding | Hold authoritative session state |
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

The `alacritty_terminal` rule is untouched by any of this: `cloo-server` names only `cloo-term`'s
own types, and the backend stays behind that wrapper.

### Emulation

`cloo-term::Emulator` is one terminal emulator per pane, owned by the session task. It is
synchronous and does no I/O: the PTY reactor reads bytes and calls `feed`, which is safe across
read boundaries because parser state persists between calls — a sequence or a multi-byte
character split across two reads still parses.

The surface is exactly what the crate table promises. `feed` takes bytes; `row`, `rows`, and
`row_text` read the visible grid; `resize` reflows it; `scrollback_len`, `scroll_offset`,
`scroll`, and `scroll_to_bottom` cover history. `cursor` and `is_alt_screen` report the state a
renderer needs but cannot derive from cells alone.

`Emulator::resize` moves emulation state only. The child still has to be told through
`TIOCSWINSZ` from the PTY layer, and the two together are the resize race described in
`AGENTS.md`.

The value types (`Cell`, `Color`, `CellAttrs`, `CursorState`) are `cloo-term`'s own. They mirror
the `cloo-proto` shapes without depending on them, because `cloo-term` sits at the bottom of the
dependency graph next to `cloo-proto` and depends on nothing in the workspace. `cloo-core` owns
the conversion; the `CellAttrs` bit layouts match so it stays a field copy.

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
crate that sees both vocabularies — and a full capture per frame is the M0 shape, bounded by the
caller's frame timer rather than by output volume. M1-04 replaces the capture with coalesced
per-row damage; the type a client applies does not change.

#### Socket lifecycle

`cloo-server::socket` decides where a session's socket lives, guarantees exactly one daemon owns
it, and clears the one a dead daemon left behind. Path resolution is a pure function of its
inputs — `resolve_socket_path(session, CLOO_SOCKET, XDG_RUNTIME_DIR, uid)` — with
`session_socket_path` as the thin wrapper that reads the process environment, matching
`cloo-client::outer`. Precedence is `CLOO_SOCKET` verbatim, then
`$XDG_RUNTIME_DIR/cloo/<session>.sock`, then `/tmp/cloo-<uid>/<session>.sock`. `CLOO_SOCKET`
names a socket rather than a directory and ignores the session name entirely, because its purpose
is standing a development daemon beside a live one. The `/tmp` form is per-uid so two users never
collide, and it is a fallback rather than the default because `/tmp` outlives a login session.

A session name reaches the filesystem, so `/`, `\`, control characters, `.`, `..`, and the empty
string are refused rather than sanitized — silently renaming a session produces a socket the user
cannot find.

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
types an incremental update uses — `Layout`, then `Damage`, then `CursorMoved` — so a resync and a
damage frame stay one code path on the client. Geometry comes first so rows never arrive with
nowhere to land.

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

Two things in the daemon are deliberate placeholders. It serves one client at a time and sends a
full grid capture per frame tick — fan-out and coalesced row damage are M1-04. What is already
true is the property that task must not break: the update rate is capped by a frame timer rather
than driven by PTY readiness.

#### The session task

`cloo-server::session` is the one thing that mutates a session. Everything that changes it — a
keystroke, a resize, a future split — arrives as a `Command` on a single `mpsc` and is applied in
arrival order by one task, as of M1-03. There is no `Mutex` on session state and no second path
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

Output flows back as a `SessionEvent`. `Output` is a *level*, not an edge — the channel holds one,
so a session producing bytes faster than anyone reads them coalesces into a single pending
notification rather than one per PTY read, and the reader asks for a snapshot when it is ready to
draw. `Exited` is sent once the PTY reaches end of file; the task stays alive and still answers
snapshot commands after it, which is what lets a child's last words be drawn before its death is
reported. The task pumps its PTY for its whole life, attached or not, so nothing written between
connections is lost.

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

#### Raw mode

`cloo-client::raw_mode::RawMode` is an RAII guard over one terminal descriptor. Restoration is by
ownership, matching the PTY layer, and covers four paths with the same restore:

| Path | Mechanism |
|---|---|
| Normal | `RawMode::restore`, or `Drop` if the caller never calls it |
| Error | `Drop` while an error unwinds out of the client |
| Panic | a panic hook installed on first entry, chained to the previous hook |
| Signal | `SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT` handlers that restore, then re-raise |

The panic hook and the signal handlers cannot borrow the guard, so the saved `termios` also lives
in a process-global restore slot that the guard arms on entry and disarms on restore. The slot is
a three-state atomic (`IDLE`/`ARMING`/`ARMED`) plus the payload, so a handler firing mid-arm sees
`ARMING` and reads nothing. A handler's only libc call is `tcsetattr`, which POSIX lists as
async-signal-safe: no allocation, no locking, no `Mutex`. Only one guard may be armed per process;
a second `enter` is refused with `AlreadyActive` rather than overwriting the saved state. Signal
handlers restore the default disposition and re-raise rather than calling `exit`, so the wait
status a parent shell sees is the one it expects from a signalled child.

#### Outer terminal

`cloo-client::outer` is what the client knows about the terminal it draws into: its geometry from
`TIOCGWINSZ` and its capabilities from `TERM` and `COLORTERM`. Detection is a pure function of
those two values so it is testable without touching the process environment, and it claims only
what can be established without writing a query sequence and waiting for a reply — everything
else stays false and takes its documented fallback. Both belong to the client, never to session
state, which is what keeps a capability difference between two attached clients from becoming
something the server has to model. A terminal that reports a zero-width or zero-height `winsize`
gets a conventional 80x24 rather than an error.

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

---

## Concurrency

Tokio, actor-shaped rather than shared mutable state:

- One task per PTY, reading into that pane's `cloo-term`.
- One **session task** owning all session state — the only thing that mutates grids and layout.
- One task per attached client, holding a `broadcast` receiver for damage.

Everything reaches the session task through a single `mpsc<Command>`. There is no `Mutex` on
session state. Expect bugs in PTY/resize *ordering*, not in lock discipline.

The session task is real as of M1-03 — see [The session task](#the-session-task). Today it owns
its pane's PTY directly rather than talking to a separate per-PTY task; splitting the two is a
detail of M2-01, and it changes nothing about the rule, since a second PTY task would still reach
session state only through the same channel.

---

## Wire Protocol

Length-framed postcard over the Unix socket. Implemented in `cloo-proto`.

```
Client → Server:  Attach { protocol_version, size, term_caps, session }
                  Detach  Input(Vec<u8>)  Mouse(MouseEvent)
                  Resize(Size)  Command(Action)

Server → Client:  Hello { protocol_version, session, tabs, size }
                  Refused { reason }
                  Damage { pane, rows: Vec<RowUpdate> }
                  CursorMoved { pane, pos, shape, visible }
                  Layout(LayoutSnapshot)  Bell(pane)  Tabs(Vec<TabSummary>)
                  Detached  Exit(code)
```

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
server relays it in `Refused { reason }` and closes the connection.

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
pretend support.

Failing to *resolve* `TERM` at all is the one case that is refused rather than degraded. A client
attaching with an unset or `dumb` `TERM` is turned away with an actionable error, because there is
no baseline to negotiate from and a silently degraded remote session is the harder failure to
diagnose. The in-process local pane has no such negotiation and keeps running with every
capability false. See [DECISIONS.md](DECISIONS.md) RESOLVED-12; the two rules compose as *refuse
when there is nothing to negotiate from, degrade when there is*.

Some child programs emit sequences intended for the *outer* terminal: notifications, titles,
clipboard writes, hyperlinks, or graphics. These are not raw bytes to relay around the grid.
cloo parses them into narrowly typed, versioned effects and each client applies only effects its
capabilities and local policy permit. Effects must be safe to suppress and must never alter
authoritative session state. Arbitrary OSC/DCS passthrough is forbidden because clients can differ
and because it can bypass cloo chrome, damage accounting, and terminal-state restoration.

Inline graphics are an optional enhancement, never a compatibility requirement. If a terminal or
intermediate multiplexer cannot support graphics, the pane remains usable and cloo exposes no
broken placeholder. This is specifically relevant to Codex terminal pets, which are unavailable
inside tmux and Zellij according to the upstream documentation.

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
