# PRD — cloo

> **Status** (2026-07-24)
>
> | Track | State |
> |---|---|
> | Shipped | Nothing published. `cloo` 0.0.1 on crates.io and `clooterminal` 0.0.1 on npm are name-reservation placeholders. |
> | Implemented in the tree | M0–M5, M6-01 through M6-05, and M7-01 through M7-02: the daemon/session model, attach transport, multipane workspace primitives, chrome composition, and deterministic compatibility fixtures are built and tested. |
> | Current CLI | `cloo` still exposes the M0 local one-pane path. It launches `$SHELL`, an explicit program, or a profile, but does not yet run the attached multipane render loop. |
> | Next | M6-06 wires the attached client’s composed frame, input, resize, and layout commands into the CLI. M6-07 then layers overlays, copy highlights, and motion onto that live loop. |
> | Remaining release work | M7-03 records manual Codex/Claude compatibility, M7-04 packages supported targets, and M7-05 applies the approved external brand system. See `docs/workboard.json`. |

---

## Objective

cloo is a terminal multiplexer for developers who already live in tmux or zellij and want the
same capabilities without the 2007 aesthetic. It is a client-server multiplexer: a daemon owns
your shells, thin clients attach and detach, sessions survive a closed terminal.

Its primary daily workflow is an agent workspace: several long-running coding harnesses, usually
Codex or Claude Code, each working in a separate pane. cloo must make it fast to launch, identify,
focus, resize, and return to the one harness that needs attention while preserving ordinary shell
and TUI compatibility.

The product bet is narrow and worth stating plainly. cloo does not aim to beat tmux on features
— it aims to be a functional peer that is markedly better to look at and to move around in.
Every scoping decision follows from that: anything invisible to the user is bought off the
shelf, so the effort concentrates on the part you stare at all day.

---

## Users

- **Primary: the author coordinating coding agents.** cloo is a daily-driver replacement for
  tmux while several Codex and Claude Code harnesses run in parallel. Living in it from M4 onward
  is the mechanism that keeps the project honest, and dogfooding is a requirement rather than a
  nice-to-have.
- **Secondary: tmux and zellij users** who are fluent with a multiplexer, are not looking to
  learn a new mental model, and would switch for a better-looking one. This is why keybindings
  are tmux-shaped by default.

There is no admin role, no accounts, and no multi-tenancy. cloo is a single-user local tool.

---

## Scope

### Phase 1 — M0–M1: prove the ownership model

- Spawn a PTY, run a shell, feed output through `cloo-term`, dump the grid. **Done.**
- Wire the three crates together in-process: `cloo` runs `$SHELL` (or a named program) in one
  full-screen pane, renders it at a capped frame rate, and forwards keystrokes. **Done at M0-07.**
  No socket, no daemon, no detach — the child dies with the client, and that is the boundary M1
  moves.
- Daemonize; Unix socket; one full-screen pane. **Implemented and integration-tested across
  M1-01–M1-05.** The attach transport is not yet exposed by the `cloo` command.
- Client raw mode, damage rendering, input forwarding, terminal restore on exit. **Implemented
  across M0–M1**, with the live attached-client loop still pending in M6-06.
- `SIGWINCH` → `Resize`. **Done at M1-03.** The signal becomes a command on the session task's
  single `mpsc<Command>`, which runs one layout pass and issues `TIOCSWINSZ` — one serialized
  owner for the grid-and-child race, and the same channel the local in-process path uses.
- Baseline harness compatibility: alternate screen, bracketed paste, extended keys, focus events,
  mouse routing, and a capability contract for terminal-dependent enhancements. **Implemented
  across M1-06–M1-09.**
- **Delivery boundary:** the daemon/attach transport can run a shell, survive a disconnected
  client, and reattach in the end-to-end test path. **Done at M1-05; CLI exposure remains M6-06.**

Proving this before anything visual is the point. If the ownership model is wrong, M1 is when
that should surface — not after splits are built on top of it.

### Phase 2 — M2–M4: make it livable and make it cloo

- **M2 splits + agent panes.** **Implemented.** Binary layout tree, focus movement, resize, close-and-collapse.
  Profiles launch generic shells, Codex, or Claude Code with explicit pane names, task labels,
  working directories, and attention state. Prefix keymap hardcoded.
- **M3 tabs + attention navigation.** **Implemented.** Multiple named tabs per session, an always-on status bar,
  and a compact queue for panes that need input, completed with unread output, or failed.
- **M4 config + theming.** **Implemented.** TOML at `~/.config/cloo/config.toml`, keybinds parsed into the
  `Action` enum, theme definitions, live reload on `SIGHUP`. The dedicated visual-identity pass.

### Phase 3 — M5–M7: v1 completion

- **M5 copy mode + search.** **Implemented.** Server-side, since scrollback lives there: vim-ish motions,
  selection, regex search with match highlighting, clipboard out via OSC 52 through the client.
- **M6 mouse and live client integration.** Mouse ownership, click-to-focus, divider drag, wheel
  actions, wire command routing, and composed multipane frames are implemented. **M6-06** is the
  remaining live attached-client render loop; **M6-07** layers overlays, copy highlights, and
  motion onto it.
- **M7 hardening + packaging.** True-color detection, reconnect/resize-race handling, and the
  deterministic compatibility fixture suite are implemented. The manual compatibility matrix,
  release packaging, and external brand application remain.

The runtime integration boundary is deliberate: many workspace capabilities are implemented below
the binary, but the current `cloo` command still starts the local one-pane path. It will not show
tabs, headers, status chrome, splits, or themes until M6-06 connects the attached client loop.

### Out of Scope

Explicitly not in v1:

- Session persistence across a *server* crash — tmux does not do this either.
- Plugins or WASM extensions.
- Session sharing over SSH.
- Per-client independent sizing. Two clients render at the minimum of both.
- Layout presets.
- Windows support.

---

## Success Criteria

- A shell survives client death: start work, kill the terminal, reattach, find the session
  running with scrollback intact.
- Two clients attach to one session simultaneously and stay visually consistent.
- `cat` of a large file does not stall or visibly tear the renderer — damage coalescing holds
  the frame budget.
- The author can run many Codex and Claude Code panes, locate a named task and every
  attention-needing pane without reading each transcript, and use zoom when a harness needs more
  room.
- Codex and Claude Code remain usable through split, focus, resize, detach, and reattach; optional
  outer-terminal graphics may degrade without breaking the harness.
- The author uses cloo as their only multiplexer for a full week without reaching for tmux.
- Every visual treatment degrades legibly on a plain 16-color TTY.
- Installing via `npm i -g clooterminal` or `cargo install cloo` yields a working `cloo`
  command on all four supported platform targets.

---

## Constraints

- **macOS and Linux only.** Windows is out of scope for v1 and no code should carry Windows
  compatibility shims.
- **Terminal emulation is a dependency, not a rewrite.** See [`DECISIONS.md`](DECISIONS.md)
  RESOLVED-02. Hand-rolling the ANSI/CSI parser is off the table.
- **Motion must be frame-budgeted and interruptible**, with a reduce-motion setting. Animation
  in a terminal is both the differentiator and the easiest way to feel sluggish.
- **Visual choices must survive a 16-color TTY.** Capability is detected and degradation is
  deliberate.
- Distribution is npm (prebuilt binaries) plus crates.io (from source).

---

## Non-Goals

- Not a tmux feature-superset. Parity is the target; exceeding tmux on features is not.
- Not a plugin platform. There is no extension API in v1.
- Not a remote/collaborative tool. No SSH session sharing, no multi-user access control.
- Not a terminal emulator. cloo runs inside your existing terminal and depends on one for
  emulation.
- Not a cloud integration for agent vendors. Harness profiles and adapters are local, opt-in, and
  work without vendor credentials beyond those the child CLI already uses.
