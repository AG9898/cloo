# DECISIONS.md — Architectural Decision Log

> **Open decisions:** Do not resolve without explicit instruction from the project owner.
>
> **To resolve an open decision:**
> 1. Move the block to the Resolved section.
> 2. Fill in the `Resolved` date and `Decision` / `Why` fields.
> 3. Update any docs affected ([ARCHITECTURE.md](ARCHITECTURE.md), [CONVENTIONS.md](CONVENTIONS.md), etc.).
> 4. Update this file in the same commit.
>
> **To add a new open decision:** copy the template below and assign the next OPEN-XX number.
> To add a resolved decision: copy the resolved template and assign the next RESOLVED-XX number.

---

## Open Decision Template

```
### OPEN-XX — <Short Decision Title>

**Question:** <What needs to be decided? State it as a precise question.>

**Context:** <Why does this matter? What are the constraints or tradeoffs involved?
What existing code or docs does this affect?>

**Options under consideration:**
1. **Option A** — description. Tradeoff: ...
2. **Option B** — description. Tradeoff: ...

**Blocking:** <What tasks or features are blocked until this is resolved? Or "Nothing currently blocked.">

**See also:** <links to related docs or tasks>
```

---

## Resolved Decision Template

```
### RESOLVED-XX — <Short Decision Title>

**Resolved:** YYYY-MM-DD

**Decision:** <The choice that was made. State it precisely.>

**Why:** <The rationale. What constraints, data, or priorities drove this choice?>

**Alternatives rejected:** <What was considered and why it was ruled out.>

**Affects:** <Which parts of the system or which docs are impacted. Link them.>
```

---

## Open Decisions

The visual decisions once deferred to M2 were resolved by the project owner on 2026-07-20 and are
recorded below, as was OPEN-01, raised during M0 implementation and resolved the same day as
RESOLVED-12. One question was raised during M1-07 and is open.

### OPEN-02 — What happens when the outer terminal and the pane disagree about key encoding

**Question:** If the user's terminal reports the Kitty extended keyboard protocol but the pane's
application has not enabled it — or the reverse — should cloo transcode keys between the two
encodings, or keep the two ends in the same encoding and never translate?

**Context:** M1-07 shipped the mode plumbing for both ends: `cloo-client::input::OuterModes`
pushes a Kitty flag set to the outer terminal only when `TermCaps::extended_keys` was negotiated,
and `cloo_term::Emulator::modes` reports whether the *application* has pushed one. Today the two
never disagree in practice, because `attach_caps` cannot establish `extended_keys` without writing
a query and reading a reply, so the client never asks for it and the fallback is the legacy
encoding on both sides. The moment cloo learns to query, the mismatch becomes reachable, and an
application reading legacy keys would receive Kitty-encoded ones it will print rather than act on.

A second, narrower gap sits underneath this one. `cloo-term` drives the emulator with a
`VoidListener`, so anything the emulator wants to write *back* to the child — a device-attributes
reply, a Kitty keyboard-mode report — is discarded. An application that probes for support gets
silence and falls back, which is safe but is a fallback rather than an answer.

**Options under consideration:**
1. **Transcode in the client** — decode Kitty key events and re-emit legacy ones when the
   application has not asked for the extended encoding. Tradeoff: the transcoder is a real
   keyboard model, and it is the component most likely to be wrong in a way that only shows up
   under one harness.
2. **Match the pane** — push the extended protocol to the outer terminal only while the focused
   pane's application has it enabled, and pop it otherwise. Tradeoff: the outer terminal's mode
   then changes on focus switches, and two panes in different modes make it churn.
3. **Never negotiate extended keys at all** — keep the legacy encoding end to end. Tradeoff:
   gives up the key disambiguation that `docs/AGENT_WORKFLOWS.md` lists as a required capability
   for an interactive harness.

**Blocking:** Nothing currently blocked. M1-07 shipped with the modes plumbed and the client not
asking for extended keys, which is the conservative state; whoever teaches `attach_caps` to query
the terminal must resolve this first.

**See also:** [`ARCHITECTURE.md`](ARCHITECTURE.md) input routing,
[`AGENT_WORKFLOWS.md`](AGENT_WORKFLOWS.md) compatibility contract,
`crates/cloo-client/src/input.rs`.

## Resolved Decisions

### RESOLVED-01 — Client-server architecture with server-owned state

**Resolved:** 2026-07-19

**Decision:** A background daemon owns all PTYs, grids, scrollback, and layout state. Clients
are thin: they hold a copy of the visible cell grid, receive damage updates, diff, and render.

**Why:** This is the most important structural call in the project. Multi-client attach becomes
nearly free because the server fans out damage. All interesting logic sits in one testable
place. A client crash can never lose session state.

**Alternatives rejected:** Forwarding raw PTY bytes to clients — cheaper in socket traffic, but
it scatters state, makes multi-client attach hard, and puts logic in the least testable process.

**Costs accepted:** More socket traffic than the naive design; terminal capabilities must be
negotiated at attach rather than assumed.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — entire topology.

---

### RESOLVED-02 — Buy terminal emulation, do not build it

**Resolved:** 2026-07-19

**Decision:** Take the whole emulation layer as a dependency. Primary choice
`alacritty_terminal`, pinned to an exact version, wrapped behind a `cloo-term` crate that is the
only thing in the workspace permitted to import it.

**Why:** An earlier draft had the ANSI/CSI parser hand-rolled. That was reversed deliberately.
Emulation is the single largest chunk of work in a multiplexer, it is where the brutal
long-tail bugs live (wide chars, combining marks, ZWJ emoji, alt-screen edge cases, DCS
passthrough), and **none of it is visible to users**. It contributes nothing to the thing that
makes cloo different.

`alacritty_terminal` is battle-tested, well documented by the standards of this space, and has
a comparatively lean dep tree. Its `Term` handles grid, scrollback, alt screen, SGR, and
selection.

**Risk accepted:** `alacritty_terminal` explicitly does not promise a stable public API. Two
mandatory mitigations: pin the exact version, and keep it behind the `cloo-term` boundary so
swapping backends is a contained job.

**Alternatives rejected:** Hand-rolled parser (enormous, invisible, bug-prone). `wezterm-term`
retained as the designated fallback — more deliberately public API, heavier dep tree.
Re-evaluate at M2 if the pin hurts; do not agonize before then.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md), [`CONVENTIONS.md`](CONVENTIONS.md) — the
import boundary is a hard never-rule.

---

### RESOLVED-03 — Binary split tree for layout

**Resolved:** 2026-07-19

**Decision:** Layout is a binary tree of `Leaf(PaneId)` and `Split { dir, ratio, left, right }`.
Splits store **ratios, not cell counts**.

**Why:** Ratios are what make layout survive a terminal resize sanely. The tree makes
split/collapse trivial: splitting replaces a leaf, closing collapses a parent.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — Layout section. `cloo-core`.

---

### RESOLVED-04 — tmux-style prefix keybindings

**Resolved:** 2026-07-19

**Decision:** tmux-style prefix key, `C-b` by default, fully rebindable. Keybinds parse into an
`Action` enum.

**Why:** The secondary user is a fluent tmux user who is not looking to learn a new mental
model. Matching tmux's shape removes the largest switching cost.

**Affects:** `cloo-core` keymap. [`PRD.md`](PRD.md) — Users.

---

### RESOLVED-05 — npm package is `clooterminal`, command stays `cloo`

**Resolved:** 2026-07-20

**Decision:** Publish to npm as `clooterminal`. The crates.io package remains `cloo`, and the
installed command is `cloo` on both channels via the npm `bin` field.

**Why:** npm rejected `cloo` with a 403 — its similarity filter flagged it as too close to
`clone`, `cli`, `clsx`, `clui`, `co`, `coz`, `cron`, `cbor`, `csso`, and `color`. The filter
runs at publish time, so the name appeared available on a registry lookup right up until the
publish attempt. `clooterminal` cleared it.

The package name is a distribution label, not the brand — `bin` decouples what users install
from what they type, so nothing about the product identity changes.

**Alternatives rejected:** `@ag9898/cloo` (scoped names bypass the filter entirely and preserve
the exact name, but read as a personal project and make the install string clunkier);
`cloomux` and `clooterm` (both registry-clear but no better than `clooterminal`).

**Note:** an earlier draft of the design doc recorded `cloo` as "free on both npm and
crates.io" and named the npm alias `cloo-terminal`. Both statements were wrong and are
superseded by this entry.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — Deployment Targets. `npm/package.json`.
Root `README.md` install section.

---

### RESOLVED-06 — Focus uses an accent border and dimmed neighbors

**Resolved:** 2026-07-20

**Decision:** Render the focused pane with the active theme accent border and reduce contrast on
unfocused panes. Dimming is configurable and must preserve text readability; focus is not an
attention signal.

**Why:** In a dense agent workspace, an accent border alone is too weak to locate the active pane
quickly. The combination follows the approved visual handoff while a no-dim option protects
accessibility and applications with strong backgrounds.

**Alternatives rejected:** Border-only focus was safer but too subtle at high pane counts;
mandatory dimming would make cloo hostile to some TUIs and users.

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md), `cloo-client` pane chrome and config.

---

### RESOLVED-07 — Always-on status bar with a minimal default

**Resolved:** 2026-07-20

**Decision:** cloo always renders a one-row status bar. The required default is a minimal flat
layout; a segmented powerline presentation is a configurable enhancement with a glyph fallback.

**Why:** Agent workflows need a persistent session, tab, prefix, and attention summary. One
predictable row is a worthwhile density cost and avoids a contextual UI that appears too late.

**Alternatives rejected:** A contextual bar hides the information most useful when coordinating
many panes. A permanently heavy powerline bar wastes cells and depends too much on font glyphs.

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md), `cloo-client`, M3 status-bar scope. Implemented in
M3-03 as a pure `status_bar_cells`/`status_bar_span` row: session, active tab, actionable tally,
and `C-b ?` yield through documented ASCII forms rather than relying on a powerline font or colour.

---

### RESOLVED-08 — Storm reference theme with palette inheritance

**Resolved:** 2026-07-20

**Decision:** Ship `storm` as the reference theme and support `night`, `gruvbox`, and `nord` as
named theme sets. Configuration may inherit the user terminal palette; all themes must map to a
deliberate 16-color fallback.

**Why:** The approved handoff supplies a coherent dark monospace visual language, while palette
inheritance prevents cloo chrome from fighting an existing terminal setup.

**Alternatives rejected:** A fixed palette would look polished only in isolation; deferring the
palette would leave focus and attention styling without a canonical semantic mapping.

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md), M4 theme configuration and renderer tokens.
Implemented in M4-03: `cloo-core` owns the `storm`, `night`, `gruvbox`, and `nord` token tables
plus the terminal-palette choice, while `cloo-client` resolves that data to exact RGB or deliberate
ANSI semantic roles before rendering. The client-local resolution means two attachments can use
different terminal palettes without changing session state.

---

### RESOLVED-09 — Short, interruptible motion vocabulary

**Resolved:** 2026-07-20

**Decision:** Focus, split, close, and overlay transitions target 120ms, remain within the render
frame budget, are interruptible, and obey a reduce-motion setting. Input and resize always win.

**Why:** Motion should make layout changes legible without delaying an active harness or creating
visible renderer backlog.

**Alternatives rejected:** No motion loses a key part of cloo's visual identity; longer or
uninterruptible animations make a terminal multiplexer feel slow.

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md), `cloo-client`, M2 and M4 implementation.

---

### RESOLVED-10 — Explicit, provenance-aware harness state

**Resolved:** 2026-07-20

**Decision:** Pane attention state is server-owned, explicitly set, and carries its source.
Lifecycle events, bells, manual marks, and opt-in local adapters are valid sources; rendered
terminal text is never a source.

**Why:** Agent TUIs change quickly and may be localized, themed, or running in an alternate
screen. Screen-scraping would be fragile and would incorrectly make a client-rendered view part
of authoritative session state.

**Alternatives rejected:** Process-name inference and transcript matching are convenient-looking
but unreliable. Requiring every harness to implement an adapter would make generic panes worse.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md), [`AGENT_WORKFLOWS.md`](AGENT_WORKFLOWS.md),
`cloo-core`, `cloo-proto`, and `cloo-server`.

---

### RESOLVED-11 — Capability-gated outer-terminal effects

**Resolved:** 2026-07-20

**Decision:** Interpret notifications, titles, clipboard writes, hyperlinks, and graphics as
typed, versioned outer-terminal effects. Each attached client applies only the allowlisted effects
its capabilities and local policy permit; arbitrary OSC/DCS passthrough is forbidden.

**Why:** A server-owned grid and multiple differently capable clients cannot safely relay raw
escape sequences. Typed effects preserve deliberate degradation and terminal restoration.

**Alternatives rejected:** Blind passthrough is incompatible with chrome ownership and can leak
terminal state. Treating all outer-terminal features as unsupported would unnecessarily degrade
notifications and accessibility-friendly clipboard workflows.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md), [`AGENT_WORKFLOWS.md`](AGENT_WORKFLOWS.md),
`cloo-term`, `cloo-proto`, `cloo-server`, and `cloo-client`.

### RESOLVED-12 — An unresolvable `TERM` refuses the attach but not a local pane

**Resolved:** 2026-07-20

**Decision:** The refusal applies where a negotiation actually happens. A client **attaching over
the socket** with an unset or `dumb` `TERM` is refused with an actionable error and does not
attach. A **local pane** — the in-process path with no socket, as shipped in M0-07 — keeps
running with every capability claimed false.

This is a rule about `TERM` being *unresolvable*, not about capabilities being *limited*. A client
that resolves `TERM` but lacks a given capability still takes its documented fallback, exactly as
[RESOLVED-11](#resolved-11--capability-gated-outer-terminal-effects) and M1-06 describe. The two
rules compose: refuse when there is nothing to negotiate from, degrade when there is.

**Why:** The strict contract in [ENV_VARS.md](ENV_VARS.md) was always written about attach, and it
earns its keep there — an agent harness attaching under a broken `TERM` should get a loud, local
error rather than a silently degraded session that has to be diagnosed remotely. But `TERM` is
routinely unset in CI shells, `docker exec`, and cron, and refusing to launch a shell in those
environments is user-hostile for no safety gain: a local pane has no second client whose
capabilities could disagree, and claiming nothing is conservative rather than a guess.

**Alternatives rejected:** Falling back everywhere would have made capability failures silent at
exactly the point they are hardest to diagnose. Refusing everywhere would have reversed a shipped,
tested M0 behavior and made `cloo` unusable wherever `TERM` is merely unset.

**Cost accepted:** The same environment produces two different behaviors depending on whether a
socket is involved. That has to be explained wherever it surfaces — the `TERM` row in
[ENV_VARS.md](ENV_VARS.md) and the attach error message are the two places that must carry it.

**Affects:** [`ENV_VARS.md`](ENV_VARS.md) `TERM` row, [`ARCHITECTURE.md`](ARCHITECTURE.md)
capability negotiation, `crates/cloo-client/src/capabilities.rs`, and workboard tasks M1-06 and
M7-01. Implemented in M1-06: `attach_caps` refuses, `caps_from_env` degrades, and the local pane
calls the second.

### RESOLVED-13 — The client decodes input, the server encodes it

**Resolved:** 2026-07-21

**Decision:** A paste, a focus change, and a mouse event each cross the wire as *what happened* —
`ClientMessage::Paste`, `Focus`, and `Mouse` — never as bytes for a child. The server encodes them
for the pane from the modes the pane's own application negotiated, which it reads out of the
emulator and reports back to the client as `ServerMessage::Modes { pane, modes }`.

**Why:** Whether a paste is wrapped in paste brackets, whether a click is reported at all, and in
which encoding are all decided by private mode sets the *child* wrote. Only the emulator sees
them, and the emulator is the server's. A client that pre-encoded would be guessing at state it
does not hold, and two clients of one session could guess differently. The reverse direction is
just as fixed: the client is the only side that knows the geometry it drew, so hit testing and the
chrome-versus-application ownership decision stay there.

**Alternatives rejected:** Having the client send pre-bracketed bytes on `Input` would have needed
no new messages, but makes correctness depend on the client's copy of a mode it cannot observe.
Having the server hit-test would have put chrome geometry into session state, which is the one
thing the client-side-chrome rule exists to prevent.

**Affects:** [`ARCHITECTURE.md`](ARCHITECTURE.md) wire protocol and input routing,
`crates/cloo-proto/src/message.rs`, `crates/cloo-server/src/session.rs`,
`crates/cloo-client/src/input.rs`. Implemented in M1-07; `PROTOCOL_VERSION` bumped to 2.

---

### RESOLVED-14 — A pane header spends width in a fixed order, and dims by blending

**Resolved:** 2026-07-21

**Decision:** The pane header is one row that is also the pane's top border: its foreground is the
theme accent when the pane is focused and the neutral border colour otherwise. Width is spent in a
fixed order of preference — focus marker, zoom indicator, pane index, title, and state glyph are
what a header *is*; the task label is dropped first when space runs out, then the state's text
label, and only then is the title truncated. Below that, the state glyph is the last thing
standing. Dimming an unfocused pane is an exact blend toward the frame background for a 24-bit
colour, and the terminal's own `DIM` attribute for a palette index or the terminal default.

**Why:** A degradation order that is decided per situation is a degradation order that differs
between two panes on one screen. Fixing it makes a narrow pane's header testable against an exact
string, and it keeps the two signals the style guide requires — identity and state — alive at every
width. Blending is what "contrast reduction toward the frame background, not alpha" means in cells;
it is also what keeps a dimmed pane's amber `needs input` distinguishable from a dimmed grey
`quiet`, which stacking `DIM` on an unknown palette entry would not. A palette index is the *user's*
colour and cloo cannot know what it looks like, so there it defers to the terminal's own faint
rendition rather than guessing.

**Alternatives rejected:** Dropping the state label before the task label would have saved the
longer string, but the style guide requires state text and a glyph together wherever they fit, and
a task label is the more recoverable of the two — it is also in the attention queue and the pane
details view. Ellipsizing a truncated title costs a cell that a narrow header does not have, and
the marker plus index already tell the user which pane they are reading.

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md) geometry and chrome, `crates/cloo-client/src/chrome.rs`,
`crates/cloo-client/src/renderer.rs`. Implemented in M2-03; no wire change, because chrome is
rendered entirely client-side.
