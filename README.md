# cloo

A terminal multiplexer in Rust — tmux's functionality, a better-looking terminal.

> **Status: pre-alpha, planning only.** There is no code yet. The design is settled and written
> up in [docs/](./docs/) — start with [ARCHITECTURE.md](./docs/ARCHITECTURE.md) for the system
> shape, [PRD.md](./docs/PRD.md) for scope and milestones, and
> [DECISIONS.md](./docs/DECISIONS.md) for what has been settled and what is still open.

## What it is

cloo is a client-server terminal multiplexer: a background daemon owns your shells, and thin
clients attach to it. Detach a session, close your terminal, reattach later and find everything
running — the same core deal as tmux and zellij.

## Why another one

Not because tmux is missing features. Because it looks like 2007.

cloo aims to be a functional peer of tmux and zellij while spending its effort somewhere they
don't: pane borders and focus treatment, a status bar worth looking at, real theming that
inherits your existing palette, and considered motion when panes split and close.

Everything that isn't visible to you gets bought off the shelf. The terminal emulation layer is a
dependency, not a rewrite, so the work goes into the part you actually look at all day.

## Planned features

- Session detach / reattach, multiple clients on one session
- Split panes on a binary tree layout, with resize
- Tabs
- tmux-style prefix keybindings, fully rebindable
- Copy mode with scrollback search and system clipboard via OSC 52
- Mouse: click to focus, drag to resize, scroll to scrollback
- TOML config with live reload, and themes

## Install

Not yet installable. When it is, the plan is:

```sh
npm install -g clooterminal   # prebuilt binaries
cargo install cloo            # from source
```

Either way the command is `cloo`. The npm package is `clooterminal` because npm's
similarity filter rejects `cloo` as too close to existing package names.

## Platforms

macOS and Linux. Windows is out of scope for v1.

## License

MIT — see [LICENSE](./LICENSE).
