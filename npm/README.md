# cloo

A terminal multiplexer in Rust — tmux's functionality, a better-looking terminal.

> **This is a placeholder release.** Installing this package does nothing; it exists only to
> reserve the name. The current source tree does have a runnable local-pane binary, but the npm
> package will not expose it until supported-target packaging is complete.
>
> Follow development at **https://github.com/AG9898/cloo**

**On the name:** the project is `cloo` and the command will be `cloo`. This package is published
as `clooterminal` because npm's package-name similarity filter rejects `cloo`. Once binaries
ship, `npm i -g clooterminal` will put a `cloo` command on your PATH.

## Source-tree status

cloo is a client-server terminal multiplexer: a background daemon owns your shells, and thin
clients attach to it. The source tree implements and tests the daemon/session model, the attach
transport, multipane workspace primitives, chrome composition, and terminal compatibility
foundations. Its user-facing CLI still runs a single local pane while the attached-client render
loop is connected.

The difference is what it looks like. cloo aims to be a functional peer of tmux and zellij while
spending its effort on pane borders and focus treatment, a status bar worth looking at, theming
that inherits your existing palette, and considered motion when panes split and close.

## Remaining before release

- Live attached-client render loop, overlays, copy highlights, and motion
- Manual Codex and Claude compatibility matrix
- Supported-target packaging and release media

## Platforms

macOS and Linux. Windows is out of scope for v1.

## License

MIT
