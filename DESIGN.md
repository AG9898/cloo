# cloo — design & initial scope

A terminal multiplexer in Rust. Client-server, tmux-shaped, intended to replace tmux for daily
use.

**The differentiator is the UI.** Functionally cloo aims to be a peer of tmux and zellij, not to
beat them on features. What it does differently is look and feel: borders, status bar, focus
treatment, theming, motion. Every scoping decision below follows from that — anything that
doesn't show up on screen gets bought off the shelf rather than built.

> This doc is the working plan. It's written to be picked up cold in a future session — see
> **Status** at the bottom for where things actually stand.

## Decisions

| Question | Answer |
|---|---|
| Goal | Daily driver replacement, differentiated on UI |
| Architecture | Client-server daemon over a Unix socket |
| Emulation | **Off the shelf** — parser *and* grid (see below) |
| Keys | tmux-style prefix (`C-b` default, rebindable) |
| Layout | Binary split tree |
| First demo | Detach and reattach |
| v1 scope | Tabs, copy mode + search, config file, mouse |
| Distribution | npm wrapper w/ prebuilt binaries, + crates.io |

Names are secured: `cloo` is free on both npm and crates.io. Binary is `cloo`; `cloo-terminal` is
reserved on npm as a descriptive alias.

## Emulation: buy, don't build

An earlier draft had us hand-rolling the ANSI/CSI parser. **That was reversed deliberately.** It's
the single largest chunk of work in a multiplexer, it's where the brutal long-tail bugs live
(wide chars, combining marks, ZWJ emoji, alt-screen edge cases, DCS passthrough), and *none of it
is visible to users*. It contributes nothing to the thing that makes cloo different.

So: take the whole emulation layer as a dependency.

**Primary choice: `alacritty_terminal`.** Battle-tested (it powers Alacritty), well documented by
the standards of this space, comparatively lean dep tree. Its `Term` handles grid, scrollback,
alt screen, SGR, and selection.

The one catch is that `alacritty_terminal` explicitly does not promise a stable public API. Two
mitigations, both cheap and both mandatory:

1. **Pin the exact version.** Upgrade deliberately, never transitively.
2. **Keep it behind `cloo-term`** — a thin wrapper exposing only what cloo needs (feed bytes,
   read cells, resize, scrollback access). Nothing outside that crate imports
   `alacritty_terminal` directly.

With that boundary, swapping the backend is a contained job. **Fallback: `wezterm-term`**, which
has a more deliberately public API but a heavier dep tree. Re-evaluate at M2 if the pin hurts;
don't agonize before then.

## Architecture

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

**The server owns everything.** All PTYs, grids, scrollback, layout state. Clients hold a copy of
the visible cell grid, receive damage updates, diff, and emit escape sequences to the real
terminal. Input flows the other way as key/mouse events.

This is the most important structural call. Multi-client attach becomes nearly free (the server
fans out damage), all interesting logic sits in one testable place, and a client crash can never
lose state. Costs: more socket traffic than a naive "forward raw PTY bytes" design, and terminal
capabilities must be negotiated at attach rather than assumed.

Note where the boundary puts the UI work — **chrome is rendered client-side**. The server sends
pane contents and layout geometry; the client decides what borders, status bar, and focus
treatment look like. That keeps theming from touching session state at all.

### Concurrency

Tokio, actor-shaped rather than shared mutable state:

- one task per PTY, reading into that pane's `cloo-term`
- one **session task** owning all session state — the only thing that mutates grids and layout
- one task per attached client, holding a `broadcast` receiver for damage

Everything reaches the session task through a single `mpsc<Command>`. No `Mutex` on session
state. Expect races in PTY/resize *ordering*, not in lock discipline.

### Crates

```
cloo-proto     wire types + framing (serde + postcard)
cloo-term      thin wrapper over alacritty_terminal — THE ONLY crate that imports it
cloo-core      session/tab/pane model, layout tree, keymap, config
cloo-server    daemon: socket, PTY reactor, damage tracking
cloo-client    attach, raw mode, renderer, theming, input encoding
cloo           the binary; decides client-vs-server, CLI surface
```

## Wire protocol

Length-framed postcard over `$XDG_RUNTIME_DIR/cloo/<session>.sock`.

```
Client → Server:  Attach { size, term_caps }  Detach  Input(Vec<u8>)
                  Mouse(MouseEvent)  Resize(Size)  Command(Action)

Server → Client:  Hello { session, tabs }  Damage { pane, rows: Vec<RowUpdate> }
                  CursorMoved { pane, pos, shape, visible }
                  Layout(LayoutSnapshot)  Bell(pane)  Detached  Exit(code)
```

**Version the handshake from day one.** You *will* have a stale client attached to a newer server
the first time you rebuild mid-session, and a clean "version mismatch, reattach" message beats a
protocol desync that presents as a rendering bug.

### Multi-client sizing

Two clients of different sizes → render at the **minimum** of both; the larger letterboxes. This
is tmux's default and the least surprising. Per-client independent views are a real feature but
they push size out of the session and into the client — post-v1.

## Layout

Binary tree of containers and leaves:

```rust
enum Node {
    Leaf(PaneId),
    Split { dir: Direction, ratio: f32, left: Box<Node>, right: Box<Node> },
}
```

Splitting replaces a leaf with a `Split` holding the old leaf plus a new one. Closing a pane
collapses its parent. Resize walks the tree adjusting `ratio`, then one layout pass assigns each
leaf a concrete `Rect` and issues `TIOCSWINSZ` to that pane's PTY.

Store **ratios, not cell counts** — that's what makes layout survive a terminal resize sanely.
Enforce a minimum pane size and reject splits that would violate it, or you'll create zero-width
PTYs and some deeply confusing shell behavior.

## Visual direction

The reason the project exists, so it gets budget rather than being a finishing pass.

Open questions to settle around M2, once there are actual borders on screen:

- **Borders** — weight, color, and whether focus is signaled by border, dimming unfocused panes,
  or both. Dimming reads well but fights with apps that set their own backgrounds.
- **Status bar** — how much chrome, and whether it's always-on or contextual.
- **Theming** — ship a small set of good built-in themes; support base16/terminal-palette
  inheritance so cloo doesn't clash with the user's existing setup.
- **Motion** — split/close/focus transitions. High impact, and the thing no existing multiplexer
  does. Also the easiest place to make something feel sluggish, so it must be frame-budgeted.

Constraint worth writing down now: **every visual choice has to survive a plain 16-color TTY.**
Detect capability, degrade deliberately, and never let the fallback look accidental.

## Milestones

Each one is runnable.

**M0 — one pane, no UI.** Spawn a PTY, run a shell, feed output through `cloo-term`, dump the grid
to stdout on a timer. No daemon, no TUI. Validates the emulation dependency and the PTY plumbing.
*With emulation bought rather than built, this is days, not months.*

**M1 — detach and reattach.** ← *first demo.* Daemonize, Unix socket, one pane, full screen.
Client sets raw mode, renders damage, forwards input, restores the terminal on exit. `SIGWINCH` →
`Resize`. Run a shell, kill the client, reattach, find it alive.

Proving this before anything pretty is the point: if the ownership model is wrong, M1 is when you
want to find out, not after building splits on top of it.

**M2 — splits.** The tree, focus movement, resize, close-and-collapse. Prefix keymap hardcoded.
**First real visual work** — borders and focus treatment now exist and need designing.

**M3 — tabs.** Multiple named tabs per session, with a status bar.

**M4 — config + theming.** TOML at `~/.config/cloo/config.toml`; keybinds parsed into the `Action`
enum; theme definitions; live reload on `SIGHUP`. The dedicated visual-identity pass.

**M5 — copy mode + search.** Server-side, since scrollback lives there: vim-ish motions,
selection, regex search with match highlighting, clipboard out via OSC 52 through the client.

**M6 — mouse.** SGR mode 1006. Click-to-focus, border drag to resize, wheel to scrollback — plus
pass-through to apps that requested mouse themselves.

**M7 — hardening + packaging.** True color, bracketed paste, focus events, alt-screen edges,
reconnect races, `$TERM`/terminfo. Then the npm wrapper: prebuilt binaries per platform
(`darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`) as optional deps, following the
esbuild/swc pattern.

Living in it from M4 onward is what will actually keep the project honest.

## Known risks

**Throughput.** `cat`ing a large file is the classic multiplexer killer. Coalesce damage and cap
render rate (~60fps) instead of emitting an update per PTY read. **Design this in at M1** — it's
architectural, not a later optimization.

**`alacritty_terminal` API churn.** Mitigated by the pin + `cloo-term` boundary above. The
mitigation only works if the boundary stays honest; no direct imports elsewhere.

**Resize ordering.** Grid resize, PTY `TIOCSWINSZ`, and the app's own `SIGWINCH` handling form a
three-way race. Serializing through the session task helps, but this remains the likeliest source
of "why is vim drawing garbage."

**Motion vs. latency.** Animations in a terminal are a differentiator and a trap. Budget frames,
make everything interruptible, and offer a "reduce motion" setting.

**Scope.** "Daily driver" is a long road. M1–M4 is when it becomes livable.

## Explicitly not in v1

Session persistence across a *server* crash (tmux doesn't do this either), plugins/WASM, session
sharing over SSH, per-client independent sizing, layout presets, Windows support.

## Status

**Planning complete. No code written yet.**

Next step is M0: scaffold the cargo workspace and get a PTY rendering through `cloo-term`.

Decisions already made and *not* worth relitigating without a specific reason: server-owns-state,
off-the-shelf emulation, binary split tree, prefix keybindings. The open questions are the visual
ones in **Visual direction**, and they're deliberately deferred to M2 when there's something on
screen to judge.
