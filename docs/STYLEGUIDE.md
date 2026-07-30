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

External product marks are governed separately by [`BRANDING.md`](BRANDING.md). They are never
rendered in terminal chrome: the terminal keeps this guide's cell-based, ASCII-first treatments
and deliberate 16-colour fallbacks. The marks applied to the repository and npm README are
external documentation assets only; no terminal-renderer or launcher UI path consumes them.

The design is intentionally dark, compact, and monospace. It supports normal terminal work and
many concurrent coding-agent panes without turning the multiplexer into a dashboard.

## Acceptance Contract and Delivery State

The handoff is the authoritative visual target wherever a terminal cell can express it. Terminal
constraints permit explicit adaptations — square corners instead of rounded ones, cell gutters
instead of pixel gaps, no shadows, and the user's terminal font — but they do not permit replacing
the handoff's hierarchy with an unrelated sparse composition. Every intentional adaptation must be
named here.

The live renderer is still short of the complete M9 handoff, but pane geometry is no longer the
header-only scaffold: every attached pane has a complete one-cell top, side, and bottom frame, and
adjacent framed allocations do not overlap. The top row is likewise no longer a bare list of tab
titles — it is the session-aware composition described under [Tab row](#tab-row). The prefix surface
is no longer a static help list either: it is the searchable command palette card 04 asks for, though
that card's golden frames are still M9-21's. The session switcher now uses the verified local daemon
catalog and can move the live client between those sockets. Keyboard and mouse resizing now light
the changed divider and show its visible ratio as card 08 requires. The remaining configuration and
status variants still need their card-specific passes. A helper being byte-tested does not by
itself establish that the attached frame matches the handoff.

The eight handoff cards define the staged acceptance set:

| Card | Required terminal result | Delivery stage |
|---|---|---|
| 01 · Single pane | Session-aware tab bar, fully framed focused pane, themed body, minimal status | Daily workspace |
| 02 · Vertical split | Equal split, one-cell gutter, accent focus frame, dimmed neighbor, rich status | Daily workspace |
| 03 · Nested workspace | Nested geometry, pane titles, application-owned content, bounded live toasts | Daily workspace |
| 04 · Prefix palette | Searchable command surface, scrim, selected row, live keybinding hints | Interactive surfaces |
| 05 · Session switcher | Real daemon session catalog, attachment state, counts, attach action | Interactive surfaces |
| 06 · Config and themes | Runtime theme/focus/status/motion settings and a truthful live preview | Theme completion |
| 07 · Status variants | High-fidelity minimal and powerline compositions with deterministic fallback | Daily workspace |
| 08 · Pane resize | Live ratio label and lit divider while a keyboard or mouse resize is active | Interactive surfaces |

At reference geometry, truecolor captures of these states must be recognizably equivalent to the
handoff in composition, hierarchy, spacing, and semantic color. The child transcript itself remains
application-owned: cloo supplies the pane surface and chrome, while shells, editors, and harnesses
choose the characters and explicit colors inside their grids.

A card is asserted as a complete cell matrix in `crates/cloo-client/tests/visual/`, where a golden
names the roles below rather than literal colors, so one expectation is checked against a truecolor
theme and its 16-color resolution. Chrome that a card has not yet routed through the client theme is
expected at its *reference* Storm value instead, which keeps the remaining gap visible in the
fixture. The status row is still authored that way: it draws the reference palette at every color
depth. Pane headers, pane bodies, and — as of M9-11 — the tab row all resolve through the client
theme instead. A tab-row golden is one complete row of a live composed frame, which is what lets the
whole width ladder be asserted without re-authoring the pane and status rows beneath it at each
width.

## Visual Decisions

- **Focus:** the focused pane has an accent border; unfocused panes are dimmed. Dimming is a
  contrast reduction toward the frame background, not alpha. Implementations must retain
  readable text and must offer a no-dim accessibility configuration.
- **Status:** one always-on row is the default. The high-fidelity minimal form uses flat,
  visually separated segments; the powerline form is available when glyph support is known.
  A compact ASCII line is the narrow or limited-capability fallback, not the reference form.
- **Themes:** `storm` is the reference built-in theme. Theme inheritance follows the user's
  terminal palette when configured, and every treatment has a deliberate 16-color fallback.
- **Motion:** split, close, focus, and overlay transitions target 120ms, are frame-budgeted and
  interruptible, and obey reduce-motion. Motion must never delay input or a resize. See
  [Motion](#motion) below for the implemented contract.

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

For a named theme, a child's `Color::Default` foreground and background resolve to that theme's
default text and pane surface. Explicit child colors remain untouched. This gives otherwise blank
shell cells the same pane ground as the chrome without writing themed cells back into the client
cache or server grid. Frame composition maps a copied pane-body row before applying unfocused-pane
dimming, so dimming operates on the colours the user sees while the cached child cells remain exact.

`terminal` palette inheritance instead leaves frame, surface, raised surface, primary, default
text, and child default cells at the outer terminal's defaults while retaining the same ANSI
semantic colours for borders, focus, and attention. It therefore honours a user's terminal palette
without losing the focused `>` marker or an attention glyph such as `!`; those text signals remain
mandatory in every theme. The client resolves the choice locally, so it never becomes server or
session state.

## Geometry and Chrome

- Render a one-cell gutter between panes. Do not imitate the mock's rounded corners or shadows.
- Draw a complete rectangular pane frame. Its top edge contains the one-row header; its side and
  bottom edges remain visible around the body. Box-drawing glyphs may degrade to ASCII while frame
  color and text markers continue to carry focus.
- A pane header is one framed row: pane index, profile/name, optional task label, and concise state.
- The always-on status bar is one row. It prioritizes session, active tab, attention count, and
  prefix hint; git/client/clock segments yield when width is limited.
- The focused pane uses the accent border. Unfocused panes use a neutral border and reduced
  contrast. In compact mode, preserve the title and state glyph even if the task label truncates.
- Use terminal-safe glyphs with ASCII fallbacks: `>` for selection, `!` for attention, `x` for
  failure, and `*` for working. Powerline separators are optional.

The header row occupies the pane's top border and joins visible side and bottom edges. There is no
second title row. Accent versus neutral frame color carries focus around the complete pane, with
the textual marker retained for terminals where color is unavailable. Its wide shape is:

```
> Z 3 claude - refactor the layout pass          ! needs input
^ ^ ^ ^        ^                                 ^
| | | |        task label (muted)                state glyph + label (semantic)
| | | title (accent + bold when focused, else primary)
| | pane index (muted)
| zoom indicator, present only while this pane is zoomed (warning)
focus marker, a space when unfocused (accent when focused, else border)
```

The daemon reserves one cell on each side of every attached pane allocation before setting the
child's PTY size. Those cells are chrome, not hidden application columns or rows. `PaneArea` keeps
the resulting interior and its surrounding frame together for both composition and mouse routing:
the body alone can reach the application, every edge focuses its pane, and adjacent edge cells are
the draggable gutter band. The local in-process one-pane launch has no attached chrome and keeps
its full PTY size.

Width is spent in a fixed order, so two panes on one screen degrade identically and a narrow header
is testable against an exact string. The marker, frame corners, zoom indicator, index, title, and
state glyph are what a header is. The task label is dropped first, then the state's text label, and
only then is the title truncated — without an ellipsis, since the marker and index already say
which pane is being read. Below even that, the state glyph is the last thing standing. See
[`DECISIONS.md`](DECISIONS.md) RESOLVED-14 and RESOLVED-19.

Dimming an unfocused pane — header and body alike — is an exact blend toward the frame background
for a 24-bit colour, and the terminal's own `DIM` attribute for a palette index or the terminal
default, which cloo cannot know the appearance of. The blend is what keeps a dimmed amber
`needs input` distinguishable from a dimmed grey `quiet`. The no-dim configuration turns the whole
treatment off and leaves focus to the accent and the marker.

### Tab row

The top row begins with a compact session badge, continues with ordered tab chips, and spends
remaining right-aligned width on low-priority workspace metadata. The active tab has a raised
surface and accent underline or lower-edge treatment; `>` also marks it so selection remains
visible without colour. Positions are one-based bar positions rather than stable IDs. Inactive tabs
use muted text, and the row is always filled to the terminal width.

At a narrow width the row keeps a contiguous window around the active tab, yielding inactive tabs
from the far right and then the far left. Right-side metadata yields before tabs, and the session
badge may reduce to its glyph before disappearing. If only the active chip fits, its title
truncates before the `>` or its index do; at the smallest widths the marker is what remains.

M9-11 implements that row through the live frame composer, and it resolves through the client theme
at every colour depth. Its shape at a reference width is:

```
 dev  1 edit >2 build  3 logs              2 panes  1 client
^^^^^ ^^^^^^ ^^^^^^^^                      ^^^^^^^^^^^^^^^^^
|     |      active chip: raised surface, accent, bold, and
|     |      underlined, with its `>` marker retained
|     inactive chip: muted text on the base surface
session badge: the daemon's projected name, dark on the accent
                                           right-aligned workspace metadata
```

The underline *is* the lower-edge treatment: a one-row bar has no second row to draw an edge on, so
the cell attribute carries it, and it survives on a terminal with no colour at all. The badge
reduces to `s` — the same session marker the narrowest status row keeps — before it disappears.

Width yields in one fixed order, which is what makes a narrowing terminal deterministic and lets the
whole ladder be a golden:

1. metadata compacts (`2 panes  1 client` becomes `2p 1c`) and then disappears;
2. inactive tabs yield from the far right and then the far left;
3. the badge reduces to its glyph and then disappears;
4. the active chip's title truncates, and below that `>2 `, `>2`, and finally `>` remain.

The session name and the attached-client count are the daemon's `WorkspaceStatus` projection; the
pane count is the layout this client is already drawing. A field the daemon has not published yet is
omitted — an unnamed session gets no badge and an unknown client count no segment — rather than
shown as a placeholder. Counts are written out with their nouns (`1 pane`, `2 panes`) so a value can
never read as a broken template.

### Status bar

The always-on bottom row has two reference compositions:

- **Minimal:** accent session segment, active and inactive tab summaries, branch/attention and
  prefix hints, then a right-aligned clock. Flat separators keep it usable without a powerline
  font.
- **Powerline:** mode, session, active tab, branch/attention, client count and effective minimum
  size, then the clock, joined by powerline separators when supported.

The active tab's `>` and every attention glyph are textual signals; colour supplements them but
never carries the meaning alone. An empty attention queue is rendered as `0!`, so the count remains
explicit. Every value has the provenance documented in
[`ARCHITECTURE.md`](ARCHITECTURE.md#visual-status-projections); unavailable optional values are
omitted rather than fabricated.

As of M9-07, the attached client caches the daemon's `WorkspaceStatus` projection for the logical
session name, attached-client count, and effective minimum size. That cache is the only source for
those fields in later status compositions: an outer-terminal resize is merely reported until the
daemon answers, and neither pane geometry nor pane text may be used as a substitute. This task does
not yet add those fields to the rendered minimal bar.

Width yields in one fixed order: clock, client geometry, branch, inactive tab summaries, active tab
title, full session name, per-state attention detail, and finally the help suffix. At the narrowest
useful width the row becomes `s>!b`, retaining one ASCII marker for session, tab, attention, and
the configured prefix's own final character. The prefix is client configuration, so two clients
attached to one session may legitimately show different hints. Below four cells the row truncates
that compact form rather than inventing a different layout.

### First attachment and command discovery

The first attached frame must explain enough to act without turning chrome into a dashboard. While
the default workspace has one pane, the status row spends available trailing width on the configured
prefix followed by `split %`, `stack "`, and `help ?`; it yields those clues from the end using the
same width ladder as every other status segment. Once there is more than one pane, the ordinary
session, tab, attention, and prefix/help summary wins that space back. A pending prefix is visibly
distinct from its settled hint and never lets the next key reach the child.

`cloo-client`'s `PrefixHint` is that field as of M8-03, and it is what the status row is handed
rather than something the row derives:

```
session:7 >2 build 0! C-b split % stack " help ?
                      ^   ^        ^        ^
                      |   |        |        the clues yield from the end:
                      |   |        |        help, then stack, then split
                      |   the configured chord's own bindings, keyed
                      the prefix, drawn verbatim — `M-a` if that is what
                      `[keys] prefix` says, never a hard-coded `C-b`
```

Two rules keep this honest. The clues are spent **before any core field yields**, so a narrowing
terminal loses `help ?` while `session:7 >2 build 0!` is still at its widest — the discovery
affordance is the cheapest thing on the row, not a competitor to the session's identity. And a
pending prefix is bracketed as well as accented — `[C-b] split % stack " help ?` — so the state that
decides where the next key goes is legible on a terminal with no colour at all; pending also turns
the clues on whatever the pane count is, because the moment the next chord matters is the moment to
say what it can be.

`<prefix> ?` opens a client-owned overlay rather than pane details, as of M8-04, and that overlay is
the searchable command palette as of M9-17. Its title names the currently effective prefix —
`commands - prefix C-b`, or whatever `[keys] prefix` says — and an empty query lists the short
controls for split, focus, zoom, tabs, copy, and detach, plus the client's own surfaces — the
launcher among them. Every row is read from the live keymap, so a rebound chord appears verbatim and
an action the user unbound has no row at all rather than a key that does nothing, and no query can
bring one back. Pane details stay on `<prefix> i`. `<prefix> a` opens the profile launcher, live as of
M8-06: it lists the profiles the client resolved, confirming one asks the server to add its pane,
and no shell text is parsed as a cloo command. All of these retain the existing dimmed backdrop,
Escape dismissal, exact width ladder, and 16-colour-safe text hints.

### Mouse gestures on chrome

The mouse is a convenience over the chrome, never a second way to reach it. As of M6-02 every
gesture spends its result on a command the keyboard already has, so a terminal that reports no
mouse at all — the documented `sgr_mouse` fallback — loses nothing but the pointer:

| Gesture | What it does | Keyboard equivalent |
|---|---|---|
| Click a pane, or its header | Focus that pane | the four directional focus bindings |
| Drag the gutter, or a header row | Move that divider | — |
| Wheel over a pane | Walk three retained lines of its scrollback | `enter-copy-mode`, then `copy-up`/`copy-down` |

The one-cell gutter and the header row are the same thing to a drag: the header row *is* the lower
pane's top border, so both are dividers and both drag. **A drag changes ratios only** — no pane is
created, closed, reordered, or restarted by one, and a drag past the end stops at the minimum pane
size rather than being refused. Pressing on a divider begins the drag and does nothing else, so
dragging a gutter can never also focus a pane.

While a keyboard or mouse resize is active, the divider uses the accent color and carries a compact
`resize · ratio 0.62` label in the nearest safe header or status segment. The affordance clears
on mouse release; a keyboard resize clears on the next non-resize input or after 750ms. Its geometry
comes from the same client layout used for hit-testing, and only the existing cell-delta resize
action crosses to session state. The label reconstructs the visible ratio from the framed
allocations after the daemon's layout answer — stored ratios still never cross the wire. The default
prefix keeps `h/j/k/l` for focus and resolves arrow chords as one-cell divider resizes around the
focused pane; an arrow aimed at an outer edge does nothing. Terminals without mouse reporting
therefore retain the same operation.

A wheel focuses the pane it is over before it scrolls, because scrollback is the focused pane's, and
a wheel over the tab row or the status bar does nothing at all: there is no scrollback under those
rows, and scrolling the focused pane instead would move a view the user was not pointing at. A pane
whose application is tracking the mouse keeps every one of these for itself; shift is the override
that reaches the chrome inside one.

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

The prefix surface is a searchable command palette, not only a static help list. It opens with the
effective prefix, accepts a query without sending text to a pane, filters live keymap actions and
client surfaces, retains a selected row, and confirms the selected action. An empty query presents
the discoverable command list; `<prefix> ?` may open the same surface with that list visible.

The session switcher lists the daemon's real session catalog rather than synthesizing the current
session. Each row carries the daemon-reported name, tab/pane/client counts, and an `attached` label
for the socket this client is viewing; singular and plural nouns remain explicit. A catalog that has
not answered yet is an empty, closable switcher rather than a fabricated current row. The catalog is
verified again once per second while the surface remains open, with every candidate still bounded by
its private inspection deadline, so a newly started or disappearing daemon updates the rows without
blocking indefinitely.

Confirming another row first completes an ordinary attach handshake to that row's verified socket.
Only after the selected daemon accepts does the client detach its current socket and replace the
frame; a daemon that disappears between inspection and confirmation is removed from the still-open
switcher, leaving the current attachment intact. Confirming the current row simply closes the
surface. No row kills a daemon, and every switcher key remains client-owned.

The session switcher, profile launcher, pane-details view, attention queue, and command palette use
one rendering model. An overlay is a title row, a list, and a hint row — plus the palette's own query
line, which is the only extra chrome row any surface has — each exactly the overlay's
width, drawn over the raised surface with the screen beneath dimmed by the same contrast reduction
an unfocused pane takes:

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
directory, and state — and a task the user never set is absent rather than blank. A command-palette
row is the same three parts spent in the same order — the chord, then what it does, then the `[keys]`
name to write when rebinding it, which is the field that yields first. The palette adds one chrome
row the other overlays do not have: a `/` query line between the title and the list, carrying what
the user has typed and an ASCII `_` text cursor.

```
  commands - prefix C-b 1/2
  / spl_
> % split right split-vertical
  " split down split-horizontal
  esc close enter run up/down move
```

The chord column is accented *and* bold, because the one thing a user opened this surface to find
may not rest on colour; the whole surface is ASCII for the same reason. The rows that name no
`[keys]` action say `client` instead — they are cloo's own surfaces, reached without the wire — so
the surface never presents a key that does nothing as though it were bound.

Typing is the palette's own departure from the shared overlay vocabulary, and it is the only one. A
printable key is query text here, so `j` types a `j`: navigation moves to the arrows and `C-n`/`C-p`,
the hint row says so in place of `j/k move`, and `q` no longer closes the surface. Escape still does,
without exception. Backspace narrows the query back, `C-u` clears it to the discoverable command
list, and the title reports the cursor's position among the *results* rather than among the commands.
A query that matches nothing says `no matches` where that position would have gone, because a blank
box reads as a broken surface rather than as a search that found nothing. The query line yields
before the title and the hint row do, so a palette too short for one still says what it is and how to
leave it. Confirming a row runs the command the cursor is on; a row naming one of cloo's own surfaces
opens that surface in place of the palette rather than sending anything.

A confirmed launch leaves one transient line on the status row, because a daemon that refuses an
identifier its own table does not name refuses it in silence, and a launcher row that appeared to do
nothing is the worse failure. The line is `launch launching <profile>` while the client is waiting
and `launch <profile> did not start` once the deadline has passed with no such pane; both spend
their width in the same fixed order as an overlay row, and both say the outcome in words as well as
in colour. It clears itself when the pane arrives and lingers only briefly when it does not, so it
never sits over a harness the user is typing into.

The attached client layers an open overlay over the already composed tab, frame, pane-body, and
status spans: it dims that existing frame without changing any character, then paints the raised
overlay box above it. Its keys are consumed locally — they never become pane input. The client-local
entries remain `<prefix> ?` for commands/help, `<prefix> s` for sessions, `<prefix> i` for focused
pane details, `<prefix> a` for the profile launcher, and `<prefix> !` for the attention queue —
the queue's chord is the glyph the status row's attention count already wears. Each is claimed only
while the keymap leaves that chord unbound, and all use the same Escape dismissal; all but the
command palette also use the same keyboard vocabulary, and its query is the documented exception.
A palette row naming one of these surfaces opens it in place, so a chord and a search reach the same
overlay.

The attention surfaces, implemented in `cloo-client`'s `chrome` module as of M2-10, make that
contract concrete:

- **Summary.** The status bar's attention count is `summary_cells`: a `<count><glyph>` group per
  actionable state that has waiting panes, coloured by state, in the fixed urgency order
  `needs_input`, `failed`, `ready`. The count is text and the glyph is a shape, so the tally never
  rests on colour alone; the standalone helper renders nothing for an empty queue while the
  always-on status row supplies its explicit `0!` fallback.
- **Queue.** The live `AttentionQueue` holds the newest unacknowledged actionable event per pane —
  `needs_input`, `ready`, or `failed`; progress and the absence of news never enter. Its order is
  deterministic: newest first, a repeat of the same live state coalesces in place, a changed state
  moves its pane to the front, and an acknowledged state does not return until the pane's state
  actually changes (a lull resets that memory). The standalone `chrome::queue_row_cells` renders a
  row through the pane header's exact-width degradation ladder, so a row drawn beside a header
  matches it. The keyboard drives it through `input::queue_action`: navigate, focus the selected
  pane, acknowledge, or dismiss.

  As of M9-15 the live surface is an overlay like every other. `<prefix> !` opens it over the
  attached frame, and it takes the shared overlay treatment — dimmed backdrop, raised box, title
  row, and hint row — with each row spending its width in the same order as the rest: the pane
  number leads, the pane name is the title, and the state (`! needs input`, glyph *and* label,
  coloured through the client theme) is the trailing field that yields first:

  ```
    attention 1/2
  > 3 build x failed
    2 claude ! needs input
    esc close enter focus a ack
  ```

  The hints keep dismissal first and give the middle slot to `a`, the one verb no other overlay
  has. Enter focuses the pane its row names and closes the surface; acknowledging leaves it open,
  because the row disappears only when the daemon's next attention projection says the pane has
  been seen. That is deliberate: acknowledgment is session state, so it crosses the wire as
  `Action::AcknowledgeAttention(pane)` rather than becoming a local view flag two attached clients
  could disagree about. An open queue follows the projection while it is open, keeping the cursor
  on the pane it was on rather than on a position a departing row shifted. A workspace with nothing
  waiting still opens the surface: an empty queue says `attention` with no position claim, and a
  key that appeared to do nothing would be the worse answer.
- **Toasts.** The live `ToastDeck` is bounded and coalesces per pane — a repeated event becomes one notice
  with a growing `(xN)` count moved to newest, and a new pane's toast evicts the oldest when the
  deck is full, so a burst can never grow the stack without limit. Toasts float over the
  upper-right safe area, never obscure the focused pane's active input row indefinitely, and
  auto-dismiss after the configured lifetime.

  As of M9-16 that is the attached client's own stack: at most three notices, each `<pane name>
  <glyph> <label>` with the `(xN)` count only when it repeated, right-aligned one column inside the
  frame's right edge and no wider than 36 columns:

  ```
                                          claude ! needs input
                                          build x failed (x2)
  ```

  The stack occupies the rows *between* the two always-on chrome rows — never the tab row, never the
  status row — and the focused pane's cursor row is skipped rather than drawn over, so a notice may
  pass in front of a harness but never in front of the line being typed into. A frame with no room
  between its chrome rows shows none. Each notice enters through the shared 120ms motion budget as a
  client surface appearing, settles into the chrome's own colours, and clears itself four seconds
  after it was raised or last refreshed; reduce-motion gives it no entrance at all. Nothing about it
  rides a pane's output clock: a notice is raised only by a new actionable attention projection and
  advanced only by the render tick. It is client chrome wherever it lands, it dims with the rest of
  the frame under an open overlay, and it owns no keys — a keystroke while a toast is showing
  belongs to the focused pane.

### Configuration and live preview

The configuration surface and the file it represents must agree. Runtime configuration accepts
theme selection and terminal inheritance, focus dimming, status mode, motion/reduce-motion, and
the existing key prefix. As of M9-02 that is the `[visual]` table of `config.toml`, parsed by
`cloo-core::config` into a `VisualConfig`:

```toml
[visual]
theme = "storm"                # storm | night | gruvbox | nord | terminal
dim_unfocused = true           # false is the no-dim accessibility option
status = "minimal"             # minimal | powerline
motion = true                  # false animates nothing at all
reduce_motion = false          # true settles every transition immediately
```

The values shown are the defaults an absent table yields, and they are the appearance every card in
this guide is drawn at. `terminal` shares the theme namespace with the four named palettes, so one
key names one appearance. `motion` and `reduce_motion` are separate keys and one question:
`VisualConfig::animates()` is false when either asks for stillness.

As of M9-05, an attached client keeps that complete typed value in its `LiveState` from the first
frame onward. The selected theme is resolved against that terminal's capabilities, `dim_unfocused`
drives the existing pane treatment, and `motion` plus `reduce_motion` construct the live motion
model; the status choice is retained for the minimal/powerline composition stages rather than
guessed from terminal output. A successful daemon reload revision makes each client reload its own
resolved file and replace the visual value as one unit. A rejected local document keeps the
preceding appearance, and no palette or accessibility choice enters session state, so two clients
on one workspace may intentionally look different.

The overlay shows effective values and a live preview built by the same
frame helpers as the workspace; it is not a second mock renderer. Invalid or unsupported settings
retain the last valid value and name the refusal — a theme or status name cloo cannot read leaves
the *whole* `[visual]` table at its defaults rather than applying the keys around it, because a
half-applied appearance is one nobody chose. It is opened as a client-local command from the
searchable prefix palette, so no additional reserved prefix chord is required.

## Motion

Motion exists to make a *layout* change legible: focus moving, a pane split, a pane closed, and an
overlay opening or dismissing. Nothing that arrives on a data clock — output, an attention report,
a resize — is ever animated, so a busy session has exactly as much motion as a quiet one.

`cloo-client`'s `motion` module is that vocabulary as of M4-04, and it is described in *frames*
rather than in milliseconds: a transition is seven whole 16ms render ticks, which fits inside the
120ms target without ever asking for a repaint the ~60fps cap would refuse. A phase is a step, not
a duration, so two clients ticking together paint the same cells.

- **Motion is a contrast ramp, never an appearance or a movement.** A transition starts recessed
  toward the frame background — the same blend dimming uses, so text stays readable at every step —
  and settles on the chrome's own colours. Chrome never slides: a header drawn somewhere it was not
  hit-tested would route a click to the wrong pane. A colour that cannot be blended, a palette index
  or the terminal's own default, takes the terminal's `DIM` rendition for the duration instead.
- **A settled phase is the chrome unchanged.** The final frame of a transition is byte-identical to
  the frame a client that animates nothing would draw, which is what makes an interruption safe.
- **Input, a resize, and a state change interrupt by settling.** They never wait for a transition
  and never rewind one: the in-flight transition ends at its end state, which is the frame the
  client was about to draw for the event anyway. No half-finished ramp is ever left on screen.
- **Reduce-motion draws one frame.** With the setting on, a transition settles where it started;
  nothing extra is requested and nothing extra is painted. The setting is client-local, like the
  theme and the effect policy, so two terminals attached to one session may disagree.

Because a transition advances only on the render tick, and a tick that lands on a step already
drawn produces no frame at all, sampling faster than the frame budget costs nothing: a whole
transition is at most eight frames however often a busy loop asks.

On the attached loop, a changed resolved layout starts focus, split, or close motion; input and a
resize settle an in-flight transition before handling their work. The renderer applies the phase
only to chrome and overlay spans. Pane-grid cells, copy highlights, and their text stay at their
ordinary rendition throughout, so motion cannot make child output flicker or alter a selection.

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
