# cloo

A terminal multiplexer in Rust — tmux's functionality, a better-looking terminal.

> **Pre-alpha.** Plain `cloo` runs one local pane: it launches `$SHELL`, renders it, and forwards
> your keystrokes. `cloo attach [session]` connects the composed multipane client to an existing
> daemon, with detach/reattach transport and terminal restoration. The published 0.0.1 release
> predates both runtime paths and only prints its status.
>
> Design doc and roadmap: **https://github.com/AG9898/cloo**

## Usage

```bash
cloo                       # run $SHELL in a single pane
cloo <program> [args...]   # run a program in a single pane
```

## What is implemented in the source tree

A client-server terminal multiplexer: a background daemon owns your shells, and thin clients
attach to it. The daemon/attach transport is integration-tested to preserve a child across client
disconnect and reconnect, and the server-side workspace model supports splits, tabs, profiles,
attention, copy/search, themes, and mouse actions.

The difference is what it looks like. cloo aims to be a functional peer of tmux and zellij while
spending its effort on pane borders and focus treatment, a status bar worth looking at, theming
that inherits your existing palette, and considered motion when panes split and close.

## Remaining workspace work

- M6-07: live overlays, copy highlights, and motion
- M7-03: manual Codex and Claude compatibility matrix
- M7-05: approved external brand application

## Platforms

macOS and Linux. Windows is out of scope for v1.

## License

MIT
