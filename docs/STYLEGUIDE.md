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

### Token resolution and palette inheritance

The named palettes are complete tables for the twelve roles above. A terminal that negotiated
truecolor receives those exact RGB values; otherwise the client resolves tokens *before*
rendering to this fixed 16-color-safe table, rather than asking a 256-colour quantizer to guess:

| Token roles | ANSI fallback |
|---|---|
| frame, surface | black (`0`) |
| raised surface, border, muted | bright black (`8`) |
| accent | bright magenta (`13`) |
| primary | bright white (`15`) |
| default text | white (`7`) |
| success, warning, error, info | bright green (`10`), bright yellow (`11`), bright red (`9`), bright cyan (`14`) |

`terminal` palette inheritance instead leaves frame, surface, raised surface, primary, and default
text at the outer terminal's defaults while retaining the same ANSI semantic colours for borders,
focus, and attention. It therefore honours a user's terminal palette without losing the focused
`>` marker or an attention glyph such as `!`; those text signals remain mandatory in every theme.
The client resolves the choice locally, so it never becomes server or session state.

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

### Tab row

The top row is a compact ordered tab bar, rendered as ` 1 shell >2 build`: positions are one-based
bar positions rather than stable IDs, and `>` marks the active tab in addition to its accent and
bold treatment. The text marker is mandatory so selection remains visible without colour. Tabs are
separated by one space and the row is always filled to the terminal width.

At a narrow width the row keeps a contiguous window around the active tab, yielding inactive tabs
from the far right and then the far left. If only the active chip fits, its title truncates before
the `>` or its index do; at the smallest widths the marker is what remains. `tab_row_cells` and
`tab_row_span` in `cloo-client::chrome` are pure cell functions, so this ladder is byte-for-byte
testable like the pane header.

### Status bar

The always-on bottom row is a minimal flat line, not a powerline segment. Its full form is
`session:7 >2 build 2! 1x C-b ?`: session identity, active one-based tab and title, the actionable
attention tally, then the prefix-and-help hint. The active tab's `>` and every attention glyph are
textual signals; colour supplements them but never carries the meaning alone. An empty attention
queue is rendered as `0!` in this row, so the count remains explicit.

Width yields in one fixed order: drop the active tab title, shorten `session:7` to `s7`, collapse a
per-state tally to its total (`3!`), then drop `?` from the `C-b ?` hint. At the narrowest useful
width the row becomes `s>!b`, retaining one ASCII marker for session, tab, attention, and the
`C-b` prefix. Below four cells no renderer can preserve all four fields, so the row is truncated
from that compact form rather than making up a different layout. `status_bar_cells` and
`status_bar_span` are pure cell functions and are rendered through the ordinary span path, whose
non-truecolor fallback down-samples colours while leaving these ASCII signals intact.

### Copy mode

Copy mode paints three roles over a pane's own cells and never replaces a character: a search
`match` is the info colour with an underline, the `selection` is the accent, and the copy `cursor`
is the selection reversed. Precedence runs match, then selection, then cursor, so a cursor inside a
selected match is still visible. Each role differs from the others by an *attribute* as well as a
colour, which is what keeps the three apart when colour is unavailable — the same rule the
attention glyphs follow.

Its status row is `COPY 1234:7 SEL /retry 1 matches`: the mode label, the copy cursor's retained
line and column, a selection marker, the active regex, and the match count. Width yields in one
fixed order — drop the match count, then the query, then the selection marker, then the position —
leaving `COPY` as the last thing standing, truncated only on a pane too narrow to hold it.

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

The session switcher, the profile launcher, and the pane-details view are that language written
once, in `cloo-client`'s `overlay` module as of M3-04. An overlay is a title row, a list, and a
hint row, each exactly the overlay's width, drawn over the raised surface with the screen beneath
dimmed by the same contrast reduction an unfocused pane takes:

```
  sessions 1/3
  7 main 3 panes attached
> 8 review 1 panes
  esc close enter switch j/k move
```

The selected row wears `> ` as well as the accent, because selection may never rest on colour
alone; an unselected row keeps the same two columns so a row never shifts as the cursor moves. A
row spends its width in the pane header's fixed order — the marker and the lead field are what a
row *is*, trailing fields yield from the end, and the title truncates last — so an overlay degrades
like the rest of the chrome rather than inventing its own layout. The hints yield the same way, but
they are ordered with dismissal *first*, so the last hint standing on a narrow overlay is the one
that says how to close. Escape is bound in every overlay without exception.

A launcher row is built from a configured profile and from nothing else: there is no free-text
command field, and a profile that fails validation is not offered rather than offered and refused
at launch. The pane-details view shows only what the server reported — profile, name, task, working
directory, and state — and a task the user never set is absent rather than blank.

The attention surfaces, implemented in `cloo-client`'s `chrome` module as of M2-10, make that
contract concrete:

- **Summary.** The status bar's attention count is `summary_cells`: a `<count><glyph>` group per
  actionable state that has waiting panes, coloured by state, in the fixed urgency order
  `needs_input`, `failed`, `ready`. The count is text and the glyph is a shape, so the tally never
  rests on colour alone; the standalone helper renders nothing for an empty queue while the
  always-on status row supplies its explicit `0!` fallback.
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
