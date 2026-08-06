# cloo — terminal chrome handoff v2

**Authored in the target medium's own units.** Every element sits at integer cell
coordinates with a per-cell 24-bit foreground, 24-bit background, and attribute flags.
This is the repo's own `ExpectedFrame` format — it drops straight into a golden at
`crates/cloo-client/tests/visual.rs`.

> The character grid is the **source of truth**. The HTML viewer in this bundle is
> generated from it, never the reverse. Nothing here is specified in pixels, font sizes,
> opacity, or radius.

This supersedes the first handoff (`design_handoff_cloo_ui/`), which was authored as an
HTML/CSS mock and lost fidelity in translation to cells.

## What's in this bundle

| File | What it is |
| --- | --- |
| `frames/card02_split_80x24.txt` | The worked exemplar: row grid + parallel style-key grid + legend, plus the header and status degradation ladders. |
| `viewer.html` | Cell-accurate rendering — each cell is a `<span>` with the resolved Storm colors. Open in any browser. |
| `README.md` | This file. |

The exemplar is **Card 02 (vertical split) at 80×24 with four panes** — the width
everyone has and where a dense multipane layout is hardest. It locks the system: the
legend keys, the elevation/dimming model, the rounded frame, and the yield order. The
remaining seven surfaces get the same treatment at 80×24, then 120×32 and 200×50.

## The medium — hard constraints

- A frame is `cols × rows` cells; one cell = one character. There is exactly one cell
  size — a title cannot be bigger than body text.
- Each cell carries: one character, a 24-bit fg, a 24-bit bg, and any of
  `BOLD DIM ITALIC UNDERLINE REVERSE HIDDEN STRIKETHROUGH`. That is the whole surface.
- No alpha, no z-layers, no shadows, no sub-cell offsets. An overlay replaces cells.
- Spacing is quantized to whole cells. "Generous padding" is 2 cells, not 12px.

Softness comes from `╭╮╰╯`, half-block edges, and truecolor surface elevation
(Frame → Surface → RaisedSurface) — not from whitespace. **Density wins:** at 80×24 with
four panes, each pane is ~38×9, so every chrome cell is a cell the user's work does not
get.

## Palette — the twelve semantic roles (Storm, truecolor)

Design against roles, never raw hex. This is `ThemeToken`.

| Role | Storm hex | Role | Storm hex |
| --- | --- | --- | --- |
| Frame | `#0f0f16` | Muted | `#565f89` |
| Surface | `#1a1b26` | Success | `#9ece6a` |
| RaisedSurface | `#24283b` | Warning | `#e0af68` |
| Border | `#2a2e42` | Error | `#f7768e` |
| Accent | `#bb9af7` | Info | `#7dcfff` |
| Primary | `#c0caf5` | DefaultText | `#a9b1d6` |

**Every treatment survives two degradations, both specified:**

1. **Truecolor → 16 colors.** frame/surface→0, raised/border/muted→8, accent→13,
   primary→15, default→7, success→10, warning→11, error→9, info→14.
2. **Unicode → ASCII.** `╭╮╰╯│─` → `+ + + + | -`; state glyphs `? ! x +` are already
   ASCII; powerline/Nerd glyphs are opt-in only, each with a named fallback.

Color is never the only carrier of a state — every state also has a glyph and/or an
attribute (`! needs-input` amber+bold, `x failed` red+bold, `+ ready` green+bold).

## The focus / elevation model

- **Focused pane:** Accent rounded frame, Bold title, Surface body, live prompt `>`.
- **Unfocused panes:** the same layout, DIM — foreground and background blended toward
  Frame — so they recede while keeping their state hue and glyph. This is RESOLVED-06
  (focus = accent frame + dimmed neighbors), expressed in cells.
- **Depth** is truecolor elevation (Frame → Surface → RaisedSurface on the active tab),
  not shadow. A one-row/one-column darker band is available as a simulated shadow where
  a surface genuinely floats, but the split layout does not spend cells on it.

## Truthfulness

Every value on screen is one the daemon actually publishes (ARCHITECTURE.md, "Visual
status projections"): session name, client count, pane count, tab summaries, the
attention queue (newest-first), prefix hint, clock, and git branch + dirty count. A
field the daemon has not sent is **omitted**, never shown as a placeholder.

Three treatments that would need data that does not exist today are named as **proposed
protocol additions** (per-pane cpu/mem, a bounded agent-progress %, and a last-activity
timestamp on the attention queue). They are proposals, not drawn as if real.

## Resolved decisions honored

RESOLVED-06 focus, -07 always-on status, -08 theming model, -09 motion, -14 header
width order, -20 rounded corners available. None re-opened.
