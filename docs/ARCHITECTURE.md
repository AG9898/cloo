# Architecture

> Canonical source for system topology, runtime boundaries, and component responsibilities.
> Other docs should link here rather than restating architecture details.

---

## Overview

cloo is a client-server terminal multiplexer written in Rust. A background daemon owns every
PTY, terminal grid, and piece of layout state; thin clients attach over a Unix socket, receive
damage updates, and render. Detaching kills the client, not the session.

The functional target is parity with tmux and zellij. The differentiator is the rendered
interface — borders, status bar, focus treatment, theming, motion — which is why the boundary
below deliberately puts all visual work client-side.

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
the socket lives at `$XDG_RUNTIME_DIR/cloo/<session>.sock`.

---

## Component Responsibilities

The workspace is six crates. The split is load-bearing — in particular the `cloo-term`
boundary, which is what makes the emulation backend replaceable.

| Crate | Owns | Explicitly does not |
|---|---|---|
| `cloo-proto` | Wire types, framing (serde + postcard), handshake version | Know anything about PTYs or rendering |
| `cloo-term` | Thin wrapper over `alacritty_terminal` — feed bytes, read cells, resize, scrollback | Leak `alacritty_terminal` types across its public API |
| `cloo-core` | Session/tab/pane model, layout tree, keymap, config | Perform I/O |
| `cloo-server` | Daemon: socket, PTY reactor, damage tracking | Decide what anything looks like |
| `cloo-client` | Attach, raw mode, renderer, theming, input encoding | Hold authoritative session state |
| `cloo` | The binary; client-vs-server dispatch, CLI surface | Contain logic that belongs in a library crate |

**Only `crates/cloo` exists today.** The other five are planned and land across M0–M2.

### Server

Owns all PTYs, grids, scrollback, and layout state. Fans damage out to every attached client.
A client crash can never lose state, because the client holds only a cache of the visible grid.

### Client

Holds a copy of the visible cell grid, diffs against incoming damage, and emits escape
sequences to the real terminal. **All chrome is rendered here.** The server sends pane contents
and layout geometry; the client decides how borders, status bar, and focus treatment look.

That boundary is why theming never touches session state.

---

## Concurrency

Tokio, actor-shaped rather than shared mutable state:

- One task per PTY, reading into that pane's `cloo-term`.
- One **session task** owning all session state — the only thing that mutates grids and layout.
- One task per attached client, holding a `broadcast` receiver for damage.

Everything reaches the session task through a single `mpsc<Command>`. There is no `Mutex` on
session state. Expect bugs in PTY/resize *ordering*, not in lock discipline.

---

## Wire Protocol

Length-framed postcard over the Unix socket.

```
Client → Server:  Attach { size, term_caps }  Detach  Input(Vec<u8>)
                  Mouse(MouseEvent)  Resize(Size)  Command(Action)

Server → Client:  Hello { session, tabs }  Damage { pane, rows: Vec<RowUpdate> }
                  CursorMoved { pane, pos, shape, visible }
                  Layout(LayoutSnapshot)  Bell(pane)  Detached  Exit(code)
```

**The handshake is versioned from day one.** A stale client will attach to a newer server the
first time anyone rebuilds mid-session. A clean "version mismatch, reattach" beats a protocol
desync that presents as a rendering bug.

### Multi-client sizing

Two clients of different sizes render at the **minimum** of both; the larger letterboxes. This
matches tmux and is the least surprising. Per-client independent views push size out of the
session and into the client — post-v1.

---

## Layout

Binary tree of containers and leaves:

```rust
enum Node {
    Leaf(PaneId),
    Split { dir: Direction, ratio: f32, left: Box<Node>, right: Box<Node> },
}
```

Splitting replaces a leaf with a `Split` holding the old leaf plus a new one. Closing a pane
collapses its parent. Resize walks the tree adjusting `ratio`, then a single layout pass assigns
each leaf a concrete `Rect` and issues `TIOCSWINSZ` to that pane's PTY.

Two rules that are easy to get wrong:

- **Store ratios, not cell counts.** This is what makes layout survive a terminal resize sanely.
- **Enforce a minimum pane size** and reject splits that would violate it, or you will create
  zero-width PTYs and correspondingly confusing shell behavior.

---

## External Dependencies

| Dependency | Purpose | Required / Optional |
|---|---|---|
| `alacritty_terminal` | Terminal emulation — parser, grid, scrollback, alt screen, SGR | Required, **exact-version pinned** |
| `tokio` | Async runtime for the PTY reactor and socket | Required |
| `serde` + `postcard` | Wire serialization and framing | Required |

None are wired up yet — the workspace currently has no dependencies.

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
