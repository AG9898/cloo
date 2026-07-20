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

All four open decisions are visual, and all four are **deliberately deferred to M2**, when
there are actual borders on screen to judge. Resolving them earlier means guessing.

### OPEN-01 — Focus signaling and border treatment

**Question:** How does a focused pane distinguish itself — border weight/color, dimming of
unfocused panes, or both?

**Context:** This is the single most visible surface in cloo and the most direct expression of
why the project exists. Dimming reads well in isolation but fights with applications that set
their own backgrounds, which is common (editors, pagers, TUIs). Border-only is safer but less
immediately legible at a glance across many panes.

**Options under consideration:**
1. **Border only** — weight and/or color change on the focused pane. Tradeoff: safe, composes
   with any app, but weaker peripheral signal.
2. **Dim unfocused panes** — reduce contrast on everything but the focus. Tradeoff: strongest
   signal, but conflicts with app-set backgrounds and can look broken in some TUIs.
3. **Both, with dimming configurable** — border always, dimming opt-in. Tradeoff: more config
   surface and two code paths to keep correct.

**Blocking:** Nothing currently blocked. Becomes blocking at M2 when splits land.

**See also:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — chrome is rendered client-side, so this
decision touches only `cloo-client`.

---

### OPEN-02 — Status bar scope

**Question:** How much chrome does the status bar carry, and is it always-on or contextual?

**Context:** Always-on costs a permanent row of vertical space, which is real estate users
notice. Contextual (appearing on tab switch, prefix press, or transient events) preserves space
but risks feeling unpredictable. tmux is always-on by default; zellij ships a fairly heavy bar.

**Options under consideration:**
1. **Always-on, minimal** — one row, tabs plus session name. Tradeoff: predictable, costs a row.
2. **Contextual** — appears on prefix or tab change, fades out. Tradeoff: reclaims the row, but
   motion and timing must be right or it feels twitchy.
3. **Configurable, always-on default** — Tradeoff: more surface to test at M4.

**Blocking:** Nothing currently blocked. Becomes blocking at M3 when tabs land.

---

### OPEN-03 — Theming and palette inheritance

**Question:** Which built-in themes ship, and how does cloo inherit the user's existing
terminal palette rather than clashing with it?

**Context:** A multiplexer that imposes its own colors on top of a carefully configured terminal
is exactly the kind of thing that makes people uninstall it. base16 and terminal-palette
inheritance are the obvious mechanisms. The constraint that every choice must survive a plain
16-color TTY interacts directly with this.

**Blocking:** Nothing currently blocked. Becomes blocking at M4.

---

### OPEN-04 — Motion vocabulary

**Question:** Which transitions get animated (split, close, focus change, tab switch), and what
is the frame budget for each?

**Context:** Motion is the thing no existing multiplexer does, so it is a genuine
differentiator — and it is the easiest possible place to make cloo feel slow. Animation must be
frame-budgeted and interruptible, with a reduce-motion setting. See the Known Risks section of
[`ARCHITECTURE.md`](ARCHITECTURE.md).

**Blocking:** Nothing currently blocked. Becomes blocking at M2 (split/close) and M4 (polish).

---

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
