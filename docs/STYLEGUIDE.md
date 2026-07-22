# cloo Terminal Style Guide

> Canonical visual language for cloo chrome. Architecture and state ownership remain in
> [`ARCHITECTURE.md`](ARCHITECTURE.md); harness-specific behavior is in
> [`AGENT_WORKFLOWS.md`](AGENT_WORKFLOWS.md).

---

## Source and Scope

This guide translates the supplied high-fidelity handoff in
[`references/design_handoff_cloo_ui/`](../references/design_handoff_cloo_ui/) into a terminal
renderer contract. The HTML mock is a visual reference, not production code: cloo renders cells
and ANSI sequences, not a DOM. When the mock relies on rounded corners, alpha, shadows, or a
specific font, this guide defines the cell-based equivalent.

The design is intentionally dark, compact, and monospace. It supports normal terminal work and
many concurrent coding-agent panes without turning the multiplexer into a dashboard.

## Visual Decisions

- **Focus:** the focused pane has an accent border; unfocused panes are dimmed. Dimming is a
  contrast reduction toward the frame background, not alpha. Implementations must retain
  readable text and must offer a no-dim accessibility configuration.
- **Status:** one always-on row is the default. The minimal flat form is the required fallback;
  segmented powerline styling is optional when glyph support is known.
- **Themes:** `storm` is the reference built-in theme. Theme inheritance follows the user's
  terminal palette when configured, and every treatment has a deliberate 16-color fallback.
- **Motion:** split, close, focus, and overlay transitions target 120ms, are frame-budgeted and
  interruptible, and obey reduce-motion. Motion must never delay input or a resize.

## Storm Palette

| Role | Value | Use |
|---|---|---|
| Frame/gutter | `#0f0f16` | space between panes |
| Surface/pane | `#1a1b26` | chrome and pane base |
| Raised surface | `#24283b` | active tabs and overlays |
| Border | `#2a2e42` | frame and unfocused panes |
| Accent | `#bb9af7` | focus, selection, active controls |
| Primary text | `#c0caf5` | labels and important text |
| Default text | `#a9b1d6` | terminal-friendly chrome text |
| Muted | `#565f89` | secondary text |
| Success | `#9ece6a` | success and ready state |
| Warning | `#e0af68` | caution and pending state |
| Error | `#f7768e` | failure and bell state |
| Info | `#7dcfff` | paths and informational state |

The named theme set is `storm`, `night`, `gruvbox`, and `nord`. On a 16-color terminal, map
accent, success, warning, error, and info to their nearest ANSI semantic colors. Never use color
as the only state signal.

## Geometry and Chrome

- Render a one-cell gutter between panes. Do not imitate the mock's rounded corners or shadows.
- A pane header is one row: pane index, profile/name, optional task label, and concise state.
- The always-on status bar is one row. It prioritizes session, active tab, attention count, and
  prefix hint; git/client/clock segments yield when width is limited.
- The focused pane uses the accent border. Unfocused panes use a neutral border and reduced
  contrast. In compact mode, preserve the title and state glyph even if the task label truncates.
- Use terminal-safe glyphs with ASCII fallbacks: `>` for selection, `!` for attention, `x` for
  failure, and `*` for working. Powerline separators are optional.

The header row *is* the pane's top border: there is no separate border row, and the accent versus
neutral colour of that one row is what carries focus. Its shape, implemented in
`cloo-client`'s `chrome` module as of M2-03, is:

```
> Z 3 claude - refactor the layout pass          ! needs input
^ ^ ^ ^        ^                                 ^
| | | |        task label (muted)                state glyph + label (semantic)
| | | title (accent + bold when focused, else primary)
| | pane index (muted)
| zoom indicator, present only while this pane is zoomed (warning)
focus marker, a space when unfocused (accent when focused, else border)
```

Width is spent in a fixed order, so two panes on one screen degrade identically and a narrow
header is testable against an exact string. The marker, zoom indicator, index, title, and state
glyph are what a header is. The task label is dropped first, then the state's text label, and only
then is the title truncated — without an ellipsis, since the marker and index already say which
pane is being read. Below even that, the state glyph is the last thing standing. See
[`DECISIONS.md`](DECISIONS.md) RESOLVED-14.

Dimming an unfocused pane — header and body alike — is an exact blend toward the frame background
for a 24-bit colour, and the terminal's own `DIM` attribute for a palette index or the terminal
default, which cloo cannot know the appearance of. The blend is what keeps a dimmed amber
`needs input` distinguishable from a dimmed grey `quiet`. The no-dim configuration turns the whole
treatment off and leaves focus to the accent and the marker.

## Agent Workspace States

Pane chrome and the attention queue use the following labels. State text and a glyph are always
present; color supplements them.

| State | Default presentation | Meaning |
|---|---|---|
| `unknown` | `? unknown` | no reliable activity signal |
| `working` | `* working` | set by an opt-in adapter or user |
| `needs_input` | `! needs input` | requires a decision or response |
| `ready` | `+ ready` | completed with unread result |
| `failed` | `x failed` | child exited unsuccessfully or adapter reported failure |
| `quiet` | `- quiet` | no active attention condition |

Focus is not an attention state. A focused but quiet pane uses the accent border; an unfocused
pane needing input retains its state glyph and semantic color after dimming.

## Overlays and Notifications

The prefix palette, session switcher, profile launcher, attention queue, and pane-details view
share one overlay language: dim the background, retain a clear selected row, provide keyboard
hints, and dismiss with Escape. Toasts are concise, stack in a bounded queue, and never cover a
focused harness input indefinitely. Coalesce repeated events from the same pane.

The attention surfaces, implemented in `cloo-client`'s `chrome` module as of M2-10, make that
contract concrete:

- **Summary.** The status bar's attention count is `summary_cells`: a `<count><glyph>` group per
  actionable state that has waiting panes, coloured by state, in the fixed urgency order
  `needs_input`, `failed`, `ready`. The count is text and the glyph is a shape, so the tally never
  rests on colour alone; an empty queue renders nothing.
- **Queue.** `AttentionQueue` holds the newest unacknowledged actionable event per pane —
  `needs_input`, `ready`, or `failed`; progress and the absence of news never enter. Its order is
  deterministic: newest first, a repeat of the same live state coalesces in place, a changed state
  moves its pane to the front, and an acknowledged state does not return until the pane's state
  actually changes (a lull resets that memory). A queue row reuses the pane header's exact-width
  degradation ladder, so a row and a header look identical and the selected row wears the same
  accent a focused pane does. The keyboard drives it through `input::queue_action`: navigate,
  focus the selected pane, acknowledge, or dismiss.
- **Toasts.** `ToastDeck` is bounded and coalesces per pane — a repeated event becomes one notice
  with a growing `(xN)` count moved to newest, and a new pane's toast evicts the oldest when the
  deck is full, so a burst can never grow the stack without limit.

## Density and Accessibility

Many agent panes make space scarce. cloo must offer pane zoom and compact chrome before hiding
identity. Minimum pane dimensions are profile-configurable; a split that violates them is
rejected. The user can disable dimming and motion, use the minimal status bar, and select a
16-color-safe theme.

Zoom exists as of M2-02 and is always a temporary, reversible view: the zoomed pane fills the tab
and the rest are hidden, never closed or resized away. Because a hidden pane is still running and
still accumulating output, the chrome must say so rather than let a zoom read as a single-pane
session — the tab shows a zoom indicator, and the pane count stays visible. The state reaches the
client as `LayoutSnapshot::zoomed`; as of M2-03 the zoomed pane's own header carries a `Z` marker,
and the tab row picks it up in M3.

See [`DECISIONS.md`](DECISIONS.md) RESOLVED-06 through RESOLVED-09 for the decisions behind this
guide.
