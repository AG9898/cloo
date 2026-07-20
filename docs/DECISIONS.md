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
recorded below. One question raised during M0 implementation is open.

### OPEN-01 — Does an unresolvable `TERM` refuse the attach or fall back?

**Question:** When a client cannot resolve `TERM` — unset, or `dumb` — does it refuse to attach,
or does it attach claiming no capabilities?

**Context:** [ENV_VARS.md](ENV_VARS.md) states the strict reading: "a client that cannot resolve
`TERM` refuses to attach rather than guessing." M0-07 implemented the permissive one instead —
`cloo-client`'s detection claims *no* capabilities and the pane still runs — on the grounds that
refusing to launch a shell over an unset `TERM` is user-hostile, and that claiming nothing is
conservative rather than a guess. There is no attach path yet, so nothing is currently
inconsistent in behavior; the two readings only collide once M1-02 lands one.

The wider contradiction is that M1-06's acceptance criterion reads "unsupported combinations
choose documented fallbacks," which is the permissive reading, while the ENV_VARS contract is the
strict one. Whoever implements M1-06 will have to pick, and should not have to pick silently.

**Options under consideration:**
1. **Refuse at attach, keep M0-07's local fallback** — the strict contract applies only where a
   negotiation actually happens; a local pane with no socket keeps running. Tradeoff: two
   behaviors for the same environment, which has to be explained wherever it surfaces.
2. **Fall back everywhere, amend ENV_VARS** — one behavior: never refuse over `TERM`, degrade to
   the documented minimum. Tradeoff: a harness attaching under a broken `TERM` gets a degraded
   session rather than a clear, immediate error, which is harder to diagnose remotely.
3. **Refuse everywhere, amend M0-07** — one behavior in the other direction. Tradeoff: reverses a
   shipped, tested M0 behavior and makes `cloo` unusable in environments where `TERM` is simply
   unset, such as some CI shells.

**Blocking:** Nothing yet. M1-06 (negotiate baseline terminal capabilities) and M7-01 (harden
terminal detection) both need this settled before their capability-failure paths are written.

**See also:** [ENV_VARS.md](ENV_VARS.md) `TERM` row, `crates/cloo-client/src/outer.rs`,
workboard tasks M1-06 and M7-01.

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

**Affects:** [`STYLEGUIDE.md`](STYLEGUIDE.md), `cloo-client`, M3 status-bar scope.

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
