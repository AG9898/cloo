# PRD — cloo

> **Status** (2026-07-20)
>
> | Track | State |
> |---|---|
> | Shipped | Nothing functional. `cloo` 0.0.1 on crates.io and `clooterminal` 0.0.1 on npm are name-reservation placeholders — the binary prints help and exits 1. |
> | In Progress | M0 — cargo workspace scaffold and a PTY rendering through `cloo-term`. See `docs/workboard.json`. |
> | Planned | M1 — detach and reattach. This is the first real demo. |

---

## Objective

cloo is a terminal multiplexer for developers who already live in tmux or zellij and want the
same capabilities without the 2007 aesthetic. It is a client-server multiplexer: a daemon owns
your shells, thin clients attach and detach, sessions survive a closed terminal.

The product bet is narrow and worth stating plainly. cloo does not aim to beat tmux on features
— it aims to be a functional peer that is markedly better to look at and to move around in.
Every scoping decision follows from that: anything invisible to the user is bought off the
shelf, so the effort concentrates on the part you stare at all day.

---

## Users

- **Primary: the author.** cloo is a daily-driver replacement for tmux. Living in it from M4
  onward is the mechanism that keeps the project honest, and dogfooding is a requirement rather
  than a nice-to-have.
- **Secondary: tmux and zellij users** who are fluent with a multiplexer, are not looking to
  learn a new mental model, and would switch for a better-looking one. This is why keybindings
  are tmux-shaped by default.

There is no admin role, no accounts, and no multi-tenancy. cloo is a single-user local tool.

---

## Scope

### Phase 1 — M0–M1: prove the ownership model

- Spawn a PTY, run a shell, feed output through `cloo-term`, dump the grid.
- Daemonize; Unix socket; one full-screen pane.
- Client raw mode, damage rendering, input forwarding, terminal restore on exit.
- `SIGWINCH` → `Resize`.
- **Delivery boundary:** run a shell, kill the client, reattach, find it alive.

Proving this before anything visual is the point. If the ownership model is wrong, M1 is when
that should surface — not after splits are built on top of it.

### Phase 2 — M2–M4: make it livable and make it cloo

- **M2 splits.** Binary layout tree, focus movement, resize, close-and-collapse. Prefix keymap
  hardcoded. First real visual work — borders and focus treatment now exist and need designing.
- **M3 tabs.** Multiple named tabs per session, with a status bar.
- **M4 config + theming.** TOML at `~/.config/cloo/config.toml`, keybinds parsed into the
  `Action` enum, theme definitions, live reload on `SIGHUP`. The dedicated visual-identity pass.

### Phase 3 — M5–M7: v1 completion

- **M5 copy mode + search.** Server-side, since scrollback lives there: vim-ish motions,
  selection, regex search with match highlighting, clipboard out via OSC 52 through the client.
- **M6 mouse.** SGR mode 1006. Click-to-focus, border drag to resize, wheel to scrollback, plus
  pass-through to apps that requested mouse themselves.
- **M7 hardening + packaging.** True color, bracketed paste, focus events, alt-screen edges,
  reconnect races, `$TERM`/terminfo. Then the npm wrapper with prebuilt per-platform binaries.

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
