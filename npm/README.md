# cloo

A terminal multiplexer in Rust — tmux's functionality, a better-looking terminal.

> **This is a placeholder release.** cloo is in active design and there is no working binary yet.
> Installing this package does nothing. It exists to reserve the name for the project below.
>
> Follow development at **https://github.com/AG9898/cloo**

## What it will be

cloo is a client-server terminal multiplexer: a background daemon owns your shells, and thin
clients attach to it. Detach a session, close your terminal, reattach later and find everything
still running — the same core deal as tmux and zellij.

The difference is what it looks like. cloo aims to be a functional peer of tmux and zellij while
spending its effort on pane borders and focus treatment, a status bar worth looking at, theming
that inherits your existing palette, and considered motion when panes split and close.

## Planned features

- Session detach / reattach, multiple clients on one session
- Split panes on a binary tree layout, with resize
- Tabs
- tmux-style prefix keybindings, fully rebindable
- Copy mode with scrollback search and system clipboard via OSC 52
- Mouse: click to focus, drag to resize, scroll to scrollback
- TOML config with live reload, and themes

## Platforms

macOS and Linux. Windows is out of scope for v1.

## License

MIT
