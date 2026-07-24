<p align="center">
  <img src="assets/cloo-product-128.svg" alt="cloo product mark: a rounded terminal with a prompt, cursor, and underscore" width="96">
</p>

# cloo

A terminal multiplexer in Rust — tmux's functionality, a better-looking terminal.

> **Pre-alpha.** The published 0.0.1 package is a name reservation and does not include a binary.
> The prepared 0.0.4 release will install the `cloo` command on Linux x64 once a maintainer
> publishes it.
>
> Follow development at **https://github.com/AG9898/cloo**

**On the name:** the project is `cloo` and the command will be `cloo`. This package is published
as `clooterminal` because npm's package-name similarity filter rejects `cloo`. `npm i -g
clooterminal` puts a `cloo` command on your PATH.

## Source-tree status

cloo is a client-server terminal multiplexer: a background daemon owns your shells, and thin
clients attach to it. The source tree implements and tests the daemon/session model, the attach
transport, multipane workspace primitives, chrome composition, attached-client render loop, and
terminal compatibility foundations. Plain `cloo` launches one local pane; `cloo server [session]`
starts a foreground daemon and `cloo attach [session]` joins it.

The difference is what it looks like. cloo aims to be a functional peer of tmux and zellij while
spending its effort on pane borders and focus treatment, a status bar worth looking at, theming
that inherits your existing palette, and considered motion when panes split and close.

## Prepared distribution

The prepared package supports Linux x64 only. macOS and Linux ARM users can build from source;
those prebuilt npm packages are not distributed yet.

## Platforms

Linux x64. Windows is out of scope for v1.

## License

MIT
