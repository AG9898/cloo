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
| `XDG_RUNTIME_DIR` | Yes | Falls back to `/tmp/cloo-$UID` when unset | Parent of the session socket directory. Sockets live at `$XDG_RUNTIME_DIR/cloo/<session>.sock`, and the directory is created and narrowed to `0700` on every bind. **Read as of M1-01.** An empty value counts as unset. The `/tmp` fallback is already per-uid, so no `cloo/` component is appended to it. |
| `TERM` | Yes | — | Terminal type. Drives capability detection. An unset or `dumb` `TERM` behaves differently depending on whether a negotiation happens ([DECISIONS.md](DECISIONS.md) RESOLVED-12): a client **attaching over the socket** is refused with an actionable error rather than guessing, while a **local pane** — no socket, as shipped in M0-07 — runs with every capability claimed false. A resolvable `TERM` with limited capabilities always takes its documented fallback; refusal is only for the unresolvable case. Attach-side enforcement lands with M1-06. |
| `SHELL` | No | `/bin/sh` | Program spawned in each new pane. **Read as of M0-07.** A bare `cloo` runs it; `cloo <program>` overrides it. The `/etc/passwd` lookup for an unset `SHELL` is not implemented — the POSIX-guaranteed `/bin/sh` is used instead. |
| `XDG_CONFIG_HOME` | No | `~/.config` | Config lookup root. cloo reads `$XDG_CONFIG_HOME/cloo/config.toml`. |
| `COLORTERM` | No | Unset | Set to `truecolor`/`24bit` by capable terminals. Used to enable 24-bit color output. **Read as of M0-07.** Without it the renderer downsamples RGB to the 256-colour palette rather than emitting a sequence the terminal may not understand. |
| `NO_COLOR` | No | Unset | Standard opt-out. When set to any value, cloo renders without color. Must still be legible — see the 16-color constraint in [`ARCHITECTURE.md`](ARCHITECTURE.md). |
| `CLOO_SOCKET` | No | Derived from `XDG_RUNTIME_DIR` | Override the socket path. Intended for tests and for running a second daemon alongside a live one. **Read as of M1-01.** It names a socket file, not a directory, and it takes precedence over the session name — the name is ignored entirely when this is set. An empty value counts as unset. Its parent directory is created if missing; a non-socket already sitting at the path is a refusal, never something cloo deletes. |
| `CLOO_CONFIG` | No | Derived from `XDG_CONFIG_HOME` | Override the config file path. Intended for tests. |
| `RUST_LOG` | No | Unset (no logging) | Standard `tracing`/`env_logger` filter. Development only. |

**Status:** `TERM`, `COLORTERM`, and `SHELL` are wired up as of M0-07, and `XDG_RUNTIME_DIR` and
`CLOO_SOCKET` as of M1-01. `XDG_CONFIG_HOME`, `CLOO_CONFIG`, `NO_COLOR`, and `RUST_LOG` are not.
The table describes the intended surface and is the contract M1 onward implements against — the
config variables land with `cloo-core`'s config loader.

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
