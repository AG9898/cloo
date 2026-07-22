# Agent Workflows and Harness Compatibility

> Canonical contract for running coding harnesses inside cloo. This document describes product
> behavior, not a vendor API integration.

---

## Goal

cloo is a local workspace for many independent coding tasks. A pane may run a shell, Codex,
Claude Code, or another interactive program; each keeps its own PTY and survives client detach.
Profiles make common launches quick, while explicit metadata and attention states make the right
pane discoverable without inspecting every transcript.

## Profiles

A profile supplies a local command template and presentation defaults. The v1 built-ins are
`generic`, `codex`, and `claude`; users may add local profiles in configuration. A profile may set
a launch command, default pane name, recommended minimum dimensions, and a state adapter. It must
not require a vendor account, call a cloud API, or make cloo depend on a vendor package.

The user controls the pane name, task label, and working directory at launch. cloo does not guess
a task from process names or transcript text.

As of M2-06 a pane is launched from an explicit profile through
`cloo-server::launch::Launch` — a validated profile plus the name, task label, and directory the
user gave. On the command line that is `cloo --profile <id> [--name ...] [--task ...] [--cwd ...]`;
a bare program (`cloo htop`) is the `generic` profile with its command replaced, named for the
program, and a bare `cloo` is `generic` on the login shell. The three built-ins and any configured
profile reach the same launch path, so a local profile works with no extra code. The launch is
validated before any process exists, so a bad name, an unexpanded `~`, or an unknown profile is a
usage error that spawns nothing; a program that is not on `PATH` is a launch-time failure whose
message names it. Nothing is inferred: `Launch` has no constructor that reads a grid or a process
name, and a task nobody supplied stays absent rather than becoming an invented one. The pane's
identity travels to clients in `ServerMessage::Panes`, projected from the same layout pass that
resolves geometry.

As of M2-04 the model is `cloo-core::profile::Profile` — `id`, `command`, `default_name`,
`min_size`, and an optional `adapter`. The three built-ins are values of that struct rather than
code paths, so adding a harness is configuration and never a patch; the test that would fail if a
vendor ever earned a special case asserts `codex` is reconstructible field for field. A profile's
command is an argv (`LoginShell` or `Program { program, args }`), never a shell string. No built-in
names an adapter: shipping one wired up by default would make an advisory signal look
authoritative.

Validation is pure and vendor-free. It checks that an ID is in a narrow alphabet, that a default
name is printable and bounded, that a command carries no control character or NUL, and that a
recommended minimum is not below cloo's layout floor — a recommendation a split could never honor
would silently mean nothing. It never asks the filesystem whether the program exists; that is a
launch-time failure the server reports.

### Local profiles in configuration

`cloo-core::config::parse` turns the *text* of `config.toml` into a validated `Config` and merges
local profiles over the built-ins. `cloo-server::config` owns the I/O: it resolves `CLOO_CONFIG`,
then `XDG_CONFIG_HOME`, then `$HOME/.config`, and on `SIGHUP` atomically swaps only a complete
validated replacement. A malformed reload leaves the preceding valid configuration active; the
later M4 tasks add keymap, theme, and motion fields to that same boundary.

A profile is an array-of-tables entry; `id` is the only required key:

```toml
[[profile]]
id = "notes"
command = ["hx", "notes.md"]   # omit entirely for the user's login shell
default_name = "notes"         # defaults to the id
min_size = { cols = 60, rows = 15 }
adapter = "my-adapter"
```

`command` is an argv, matching `ProfileCommand`: there is no shell-string form, so an argument
containing a space is one argument. An omitted `command` asks for the login shell; an explicit
empty array is a mistake and is refused rather than read as one. Reusing a built-in's `id`
replaces that built-in **in place**, keeping its position in the launcher, because that position
is part of what the user learned. A repeated ID within one document keeps the first definition, so
the result never depends on which duplicate was seen last.

Two kinds of wrongness get two different answers. Syntax is the document's: malformed TOML or an
unknown key fails the whole parse and the caller keeps the defaults — an ignored typo would be a
setting the user believes is applied. Semantics are each profile's: a well-formed entry that does
not validate is dropped alone with a `ConfigWarning` naming it, and its neighbours still load. One
bad profile must never cost the user the other nine, and nothing is ever silently coerced into
something the document did not say.

## Attention Contract

The server stores a state and its provenance. Generic sources are child lifecycle, terminal bell,
and an explicit user command. Optional local adapters can report `working`, `needs_input`,
`ready`, or `failed`; adapter data is advisory and visibly identified as such. A pane with no
reliable signal remains `unknown`, not falsely marked working.

The attention queue is a navigation surface, not a notification firehose. It lists the newest
unacknowledged event per pane, coalesces repeats, and supports keyboard focus/acknowledge. The
always-on status bar displays a compact count.

M2-04 models this as `cloo-core::pane::Attention` — a state, its source, and whether the user has
acknowledged it. `AttentionSource` is `None`, `Bell`, `Lifecycle`, `User`, or `Adapter(AdapterId)`,
and only the adapter variant reports `is_advisory()`, which is what lets the chrome attribute a
claim instead of presenting it as fact. Only `needs_input`, `ready`, and `failed` enter the queue:
progress and absence of news are not things a human is being asked to act on. Coalescing lives in
`Attention::set` — acknowledgment is cleared when the state *changes* and kept when the same state
is re-reported, so a harness announcing `needs_input` every second cannot refill a queue the user
just cleared. Pane identity (`PaneName`, `TaskLabel`, `WorkingDir`) is validated user text, never
inferred.

M2-07 persists that model in the session actor. Every report reaches it as a `SetAttention`
command on the one session channel and is applied in arrival order, so the coalescing rule above
is enforced by the single writer rather than raced between sources; `AcknowledgeAttention` is its
own command and moves only the seen flag. A report for a pane that has closed is dropped. Each
pane's state and provenance cross to the client as `PaneAttention` in a `ServerMessage::Attention`,
resent only when some pane's attention changes, and an uninstrumented pane is carried as `unknown`
rather than omitted.

M2-08 connects the generic sources to that plumbing. A terminal bell maps to `needs_input` with
`Bell` provenance; a child's exit maps to `ready` on a clean exit and `failed` on any other status,
both with `Lifecycle` provenance and the status read by a non-blocking reap rather than inferred;
an explicit user mark carries `User` provenance. Every one of them is something cloo observes
directly — a control byte, an end-of-file, a command — and never text read out of the rendered
grid, so the no-screen-scraping rule is a property of what a source *is*, not a check bolted on
after. The opt-in adapter interface, the only advisory source, lands in M2-09.

M2-10 renders those two surfaces client-side in `cloo-client`'s `chrome` module. The status bar's
compact count is `summary_cells`, a per-state tally coloured and glyphed in a fixed urgency order.
The navigation surface is `AttentionQueue`: the newest unacknowledged actionable event per pane —
only `needs_input`, `ready`, and `failed` — ordered newest-first, with coalescing and acknowledgment
rules that mirror `cloo-core`'s `Attention::set`, so a re-announced state neither churns the list
nor refills a queue the user just cleared. The keyboard drives focus and acknowledge through
`input::queue_action`. Repeated events also raise a bounded, per-pane-coalescing `ToastDeck`. All of
it is pure rendering over the attention state the server already owns; nothing here reads the grid.
The visual contract is in [STYLEGUIDE.md](STYLEGUIDE.md#overlays-and-notifications).

## Compatibility Tiers

| Tier | Contract |
|---|---|
| Required | UTF-8/color, alternate screen, resize, bracketed paste, extended keys, focus events, SGR mouse routing, raw-mode restoration, and normal scrollback behavior. |
| Negotiated | Clipboard, hyperlinks, notifications, terminal-title changes, and progress effects are typed effects applied only when the attached client permits and supports them. |
| Optional | Inline graphics. They may be unavailable through cloo without breaking the harness. |

A tier is a contract about behaviour, not a requirement the terminal must meet. A client whose
terminal lacks a required capability still attaches and still runs the harness — it takes the
documented fallback for that capability, listed in
[ARCHITECTURE.md](ARCHITECTURE.md#outer-terminal). The one refusal is a `TERM` that cannot be
resolved at all: there is no baseline to negotiate from, so an attach is turned away with an
actionable error rather than degraded silently, while the local in-process pane keeps running with
every capability false ([DECISIONS.md](DECISIONS.md) RESOLVED-12). A harness attaching under a
broken `TERM` therefore gets a loud local error instead of a session that has to be diagnosed
remotely.

The required tier's input half is plumbed end to end as of M1-07. cloo asks the outer terminal for
bracketed paste, focus reporting, and SGR mouse reporting when the client negotiated them, decodes
what comes back into typed events, and re-encodes each one for the pane using the modes the
*harness itself* negotiated — so a harness that never enabled bracketed paste receives pasted text
as ordinary typing rather than delimiters it would print. A harness that tracks the mouse owns
mouse events over its own pane; shift is the override that reaches cloo's chrome without the
harness seeing the click. Extended keys are the one required capability still unclaimed: the
client cannot establish it without a terminal query, so both ends stay on the legacy encoding and
the mismatch case is [DECISIONS.md](DECISIONS.md) OPEN-02. See
[ARCHITECTURE.md](ARCHITECTURE.md#input-routing).

M1-08 gives negotiated outer-terminal features a typed wire vocabulary before any client is
allowed to render one. The emulator recognizes title and OSC 52 clipboard-store requests, while
the vocabulary also names hyperlinks, notifications, progress, and `Graphics(Unavailable)` with
no raw OSC, DCS, or graphics-payload escape hatch. M1-09 drains and fans out each typed request,
then lets every client apply its own default-deny policy: titles require title permission, and OSC
52 stores require both clipboard permission and the terminal capability. Effects with no safe
standalone renderer stay suppressed, so a harness cannot alter the outer terminal by emitting a
control string.

M5-02 routes the user's own copy through that same gate rather than around it. A copy in
copy mode is an explicit `Action`, answered privately to the one client that asked with an
ordinary `ClipboardStore` effect; a client whose policy or terminal cannot store a clipboard
writes nothing and never asks for the text, so a harness pane's scrollback is not put on the wire
for a terminal that would discard it.

Claude Code documents that tmux needs extended keys and passthrough for some of its terminal
features; cloo's required and negotiated tiers cover the equivalent responsibilities. Codex
documents that its terminal pets need graphics support and are unavailable inside tmux and Zellij;
cloo therefore makes no graphics compatibility promise for v1. See the [Claude Code terminal
guide](https://code.claude.com/docs/en/terminal-config) and [Codex pets
guide](https://learn.chatgpt.com/docs/pets).

## Safety and Multi-client Rules

No harness output may bypass the renderer. cloo allowlists and capability-gates typed
outer-terminal effects so a child cannot alter terminal state unexpectedly or create divergent
behavior across attached clients. Graphics and other client-local effects are never authoritative
session state.

The session size remains the minimum of attached clients. A profile's recommended size prevents
bad splits on a single client; it cannot make a small second client safe. Zoom is the normal way to
give an active harness full room.

## Compatibility Matrix

Before a supported profile is claimed compatible, test it manually in an installed terminal:

1. Launch in one pane, then in a split layout.
2. Send normal and large bracketed pastes; verify extended-key shortcuts.
3. Exercise focus, mouse, alternate screen, zoom, and repeated resize.
4. Detach and reattach while the harness is active.
5. Verify an attention event appears once and can be acknowledged.
6. Verify unsupported outer-terminal effects degrade without corrupting the pane.

The deterministic escape-sequence fixture suite described in [`TESTING.md`](TESTING.md) is the
automated gate. Real Codex and Claude Code smoke runs are versioned manual evidence, not CI
dependencies.
