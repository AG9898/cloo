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

## Density and Accessibility

Many agent panes make space scarce. cloo must offer pane zoom and compact chrome before hiding
identity. Minimum pane dimensions are profile-configurable; a split that violates them is
rejected. The user can disable dimming and motion, use the minimal status bar, and select a
16-color-safe theme.

See [`DECISIONS.md`](DECISIONS.md) RESOLVED-06 through RESOLVED-09 for the decisions behind this
guide.
