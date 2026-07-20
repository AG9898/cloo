# ENV_VARS.md — Environment Variable Reference

This is the single source of truth for all environment variable configuration.
If any other doc mentions a variable, it should link here rather than restate it.

> **cloo has no secrets.** It is a local single-user tool with no accounts, no network tier,
> and no credentials. There is no `.env` file, and none of the variables below are sensitive.
> If that ever changes, secret values come from the environment only and never from source.

---

## Variable Matrix

Everything here is **read**, not owned — cloo consumes standard environment variables set by
the user's shell and desktop session. cloo defines none of its own except `CLOO_*`.

| Variable | Required | Default | Description |
|---|---|---|---|
| `XDG_RUNTIME_DIR` | Yes | Falls back to `/tmp/cloo-$UID` when unset | Parent of the session socket directory. Sockets live at `$XDG_RUNTIME_DIR/cloo/<session>.sock`. |
| `TERM` | Yes | — | Terminal type. Drives capability detection at attach. A client that cannot resolve `TERM` refuses to attach rather than guessing. |
| `SHELL` | No | User's login shell from `/etc/passwd` | Program spawned in each new pane. |
| `XDG_CONFIG_HOME` | No | `~/.config` | Config lookup root. cloo reads `$XDG_CONFIG_HOME/cloo/config.toml`. |
| `COLORTERM` | No | Unset | Set to `truecolor`/`24bit` by capable terminals. Used to enable 24-bit color output. |
| `NO_COLOR` | No | Unset | Standard opt-out. When set to any value, cloo renders without color. Must still be legible — see the 16-color constraint in [`ARCHITECTURE.md`](ARCHITECTURE.md). |
| `CLOO_SOCKET` | No | Derived from `XDG_RUNTIME_DIR` | Override the socket path. Intended for tests and for running a second daemon alongside a live one. |
| `CLOO_CONFIG` | No | Derived from `XDG_CONFIG_HOME` | Override the config file path. Intended for tests. |
| `RUST_LOG` | No | Unset (no logging) | Standard `tracing`/`env_logger` filter. Development only. |

**Status:** none of these are wired up yet — the current binary reads no environment at all.
The table describes the intended surface and is the contract M0–M1 should implement against.

---

## Local Development Setup

No setup required. There is no `.env` file and nothing to copy.

```bash
cargo build --workspace
cargo run -p cloo -- --help
```

To run a development daemon without disturbing a live session, override the socket:

```bash
CLOO_SOCKET=/tmp/cloo-dev.sock cargo run -p cloo
```

---

## Per-Environment Summary

cloo has one environment: the user's machine. There is no staging or production tier.

| Variable | Local dev | Installed use |
|---|---|---|
| `XDG_RUNTIME_DIR` | Required (with fallback) | Required (with fallback) |
| `TERM` | Required | Required |
| `CLOO_SOCKET` | Common — isolates a dev daemon | Rare |
| `CLOO_CONFIG` | Common — isolates test config | Rare |
| `RUST_LOG` | Common | Not used |
