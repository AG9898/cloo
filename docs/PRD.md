# PRD — cloo

> **Status** (2026-07-24)
>
> | Track | State |
> |---|---|
> | Shipped | `clooterminal` 0.0.1 is a name-reservation placeholder. The Linux x64 0.0.4 package is prepared for maintainer publication; `cloo` 0.0.1 on crates.io is also a placeholder. |
> | Implemented in the tree | M0–M7: the daemon/session model, public daemon lifecycle, attach transport, multipane workspace primitives, chrome composition, attached-client CLI loop, live visual states, deterministic compatibility fixtures, supported-target packaging, and external brand application are built and tested. |
> | Current CLI | `cloo` launches the M0 local one-pane path; `cloo server [session]` owns a foreground daemon session; and `cloo attach [session]` joins it with the composed multipane frame, decoded input, resize, and layout controls. |
> | Next | M8 is planned: plain `cloo` will attach to, or safely create, one persistent default workspace; the attached UI will expose the essential workspace actions and their shortcuts. Manual release validation and registry publishing remain maintainer-owned actions. |
> | Packaging | M7-04 supplies the package structure. The working npm distribution is published directly from a maintainer terminal and currently carries a Linux x64 native dependency only; other prebuilt targets remain deferred. M7-05 adds the approved product mark to the distribution README. |
> | Remaining release work | M8 is active pre-release product work. Publishing to npm or crates.io still requires explicit maintainer authorization. |

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
  M1-01–M1-05; exposed by `cloo server [session]` and `cloo attach [session]` at M6-08.**
- Client raw mode, damage rendering, input forwarding, terminal restore on exit. **Implemented
  across M0–M1 and driven by the attached-client loop at M6-06.**
- `SIGWINCH` → `Resize`. **Done at M1-03.** The signal becomes a command on the session task's
  single `mpsc<Command>`, which runs one layout pass and issues `TIOCSWINSZ` — one serialized
  owner for the grid-and-child race, and the same channel the local in-process path uses.
- Baseline harness compatibility: alternate screen, bracketed paste, extended keys, focus events,
  mouse routing, and a capability contract for terminal-dependent enhancements. **Implemented
  across M1-06–M1-09.**
- **Delivery boundary:** `cloo server [session]` can run a shell while independent clients detach
  and reattach through `cloo attach [session]`. **Done at M6-08.**

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
  actions, wire command routing, composed multipane frames, the `cloo attach` live loop, and the
  `cloo server` session lifecycle are implemented. **M6-07** adds overlays, copy highlights, and
  motion to that live loop.
- **M7 hardening + packaging.** True-color detection, reconnect/resize-race handling, the
  deterministic compatibility fixture suite, the manual compatibility matrix, supported-target
  release packaging, and external brand application are implemented. Publishing remains a
  maintainer action.

The runtime boundary is now explicit: plain `cloo` remains the local one-pane launcher,
`cloo server [session]` owns a daemon session without altering its own terminal, and `cloo attach
[session]` renders that daemon-owned workspace with tabs, headers, status chrome, splits, and
themes.

### Phase 4 — M8: make the workspace the default entry

**Planned.** The ordinary command will become `cloo`: it will attach to the global `default`
workspace when its daemon is already running, or create that daemon in the background and attach
when it is not. The first creation inherits the caller's working directory; later invocations only
reattach and never retarget the existing workspace. Detach continues to leave panes alive.

The foreground `cloo server [session]` and explicit `cloo attach [session]` commands remain for
debugging, automation, and named-session work. `cloo <program> [args…]` deliberately preserves the
M0 one-pane, non-persistent path rather than silently adding a program to an existing workspace.

The attached client will make its own operations discoverable: a first-attach status hint exposes
the prefix and split/help shortcuts, `C-b ?` opens an actual command/help surface, and the existing
profile launcher becomes a live way to add a generic, Codex, or Claude pane. Clues live in cloo's
chrome after its prefix, never in text typed for the child shell.

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
- Installing via `npm i -g clooterminal` yields a working `cloo` command on Linux x64; the
  other prebuilt platform targets remain deferred.
- From a clean supported terminal, `cloo` attaches to the default workspace in one command;
  a concurrent invocation joins that same workspace rather than creating a second daemon.
- The first attached frame makes split and help controls discoverable without interpreting a
  command intended for a pane.

---

## Constraints

- **Linux x64 distribution only.** Other Unix platforms remain source-build targets for now;
  Windows is out of scope for v1 and no code should carry Windows compatibility shims.
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
