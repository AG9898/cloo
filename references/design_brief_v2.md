# Design brief: cloo terminal chrome, second handoff

> This is a prompt for a design tool, not a cloo specification. The canonical visual
> contract remains [`docs/STYLEGUIDE.md`](../docs/STYLEGUIDE.md); nothing here overrides
> it. It supersedes the brief that produced
> [`design_handoff_cloo_ui/`](design_handoff_cloo_ui/).

## What you're designing for

cloo is a client-server terminal multiplexer in Rust (a tmux/zellij peer). It renders
into a **character cell grid** inside someone else's terminal emulator. It is not a
terminal emulator and never draws pixels.

Repo: https://github.com/AG9898/cloo

An earlier handoff exists at `references/design_handoff_cloo_ui/`. It was authored as
an HTML/CSS mock and then translated into cells. That is the problem I want fixed. The
mock specified border-radius, box-shadow, rgba alpha, 4–44px gaps, letter-spacing, and
five font sizes on one board — none of which a cell grid has. Translation therefore
became a series of judgment calls, and the result reads as a downgrade rather than as
the design.

**This handoff must be authored in the target medium's own units.** Every element sits
at integer cell coordinates. If you cannot express it as a grid of characters with
per-cell colors and attributes, it is not a design here — it is a wish.

## Read these first, in this order

1. `docs/STYLEGUIDE.md` — the current contract. The "Intentional terminal adaptations"
   table is the list of places the medium genuinely refused something.
2. `crates/cloo-client/tests/visual.rs` — the goldens. **This is what actually ships
   today**, cell for cell. Start here to see the real baseline, not the old mock.
3. `crates/cloo-client/tests/visual/harness.rs` — the `ExpectedFrame` format. This is
   your deliverable format; see below.
4. `docs/DECISIONS.md` — resolved decisions. RESOLVED-06 through 09 fix focus, status,
   theming, and motion. RESOLVED-14 fixes header width order. RESOLVED-20 establishes
   that rounded corners are available. Do not re-open these.
5. `docs/PRD.md` — scope and non-goals.
6. `docs/ARCHITECTURE.md`, section "Visual status projections" — the only data that
   actually exists to put on screen.
7. `crates/cloo-client/src/chrome.rs`, `overlay.rs`, `theme.rs` — the renderer.

## The medium: hard constraints

- A frame is `cols × rows` cells. One cell = one character. There is exactly **one cell
  size**; you cannot make a title bigger than body text.
- Each cell carries: one character, a 24-bit foreground, a 24-bit background, and any
  of `BOLD DIM ITALIC UNDERLINE REVERSE HIDDEN STRIKETHROUGH`. That is the complete
  expressive surface.
- **No alpha, no z-layers, no shadows, no sub-cell offsets.** An overlay does not float
  above content; it replaces cells.
- Spacing is quantized to whole cells. "Generous padding" means 2 cells, not 12px.
- The font belongs to the user's terminal. Assume a monospace font at 1 column per
  character; do not rely on any specific typeface.

### What the grid *can* do, and should

These are underused today and are where most of the fidelity gain lives:

- **Rounded corners**: `╭ ╮ ╰ ╯` are ordinary box-drawing glyphs. Available now.
- **Half-blocks and quadrants**: `▀ ▄ ▌ ▐ ░ ▒ ▓ ▘ ▝ ▖ ▗`. A `▀` with different fg and
  bg gives two independently colored half-cells — this is how you get sub-cell edges,
  soft dividers, and gradient ramps.
- **Simulated shadow**: a band of darker-background cells offset one row down and one
  column right reads convincingly as a drop shadow. Requires no alpha.
- **Truecolor surface elevation**: subtle bg steps between frame → surface → raised
  surface do most of the perceived depth work.
- **Powerline `U+E0B0` and Nerd Font icons**: allowed, but only behind an opt-in, and
  every such glyph needs a specified non-glyph fallback.

## Palette and token vocabulary — a closed list

Design against these twelve semantic roles, never raw hex. This is `ThemeToken`:

`Frame  Surface  RaisedSurface  Border  Accent  Primary  DefaultText  Muted
 Success  Warning  Error  Info`

Storm (the reference theme) resolves them as documented in `docs/STYLEGUIDE.md`. Four
named themes share this vocabulary, and every role has a mandated 16-color fallback.

**Every treatment must survive two degradations**, and you must specify both:

1. Truecolor → 16 colors (the fixed table is in the style guide).
2. Unicode → ASCII (`+ - |`, `>`, `!`, `*`, `x`).

Color may never be the *only* carrier of a state. Each state needs a glyph or an
attribute too.

## Deliverable format

Not HTML. For each surface, at each width, give:

1. **A character grid** — exact `cols × rows`, monospace, every cell accounted for.
2. **A parallel style-key grid** — same dimensions, one key character per cell.
3. **A legend** mapping each key to `(fg token, bg token, attributes)`.

This is exactly the repo's own `ExpectedFrame` format, so it drops straight into a
golden. It looks like this:

```
row: "  commands - prefix C-b 1/2             "
key: "rrAAAAAAAAAAAAAAAAAAAAAmmmmddddddddddddd"

A = (Accent,      RaisedSurface, BOLD)
r = (DefaultText, RaisedSurface, NONE)
m = (Muted,       RaisedSurface, NONE)
d = (DefaultText, Surface,       NONE)
```

If you also want a viewable artifact, render those grids to an HTML page where each
cell is a `<span>` with the resolved colors — but the grid is the source of truth and
the HTML is generated from it, never the reverse.

## Surfaces and widths

Cover the eight existing card states (see the table in `docs/STYLEGUIDE.md`): single
pane, vertical split, nested workspace, prefix palette, session switcher, config and
themes, status variants, pane resize.

Design each at **80×24** first — that is the floor everyone has and where a dense
multipane layout is hardest. Then 120×32 and 200×50.

cloo's design is substantially about **deterministic degradation**, so a single hero
width is not a design. For every surface, specify the order in which elements yield as
width shrinks, down to the documented ASCII floor. The existing width ladders in
`docs/STYLEGUIDE.md` are the model.

## Truthfulness rule

Design only with values the daemon actually publishes. `docs/ARCHITECTURE.md`'s visual
status projections are the complete list. A field the daemon has not sent is **omitted**,
never shown as a placeholder or a plausible-looking sample. If a design needs a value
that does not exist, say so explicitly as a proposed protocol addition rather than
drawing it as though it were there.

## Aesthetic target — and the constraint that outranks it

The reference feel is **opencode / the Charm (Bubble Tea + Lipgloss) family**: rounded
borders, restrained palette, muted secondary text, subtle truecolor elevation, tasteful
icon glyphs.

**Density wins. This is not a tie to be broken by taste.**

opencode is a single-flow, full-width TUI that can spend 2–4 cells of padding on every
edge. cloo is a multiplexer whose entire reason to exist is showing many live agent
panes at once. At 80×24 with four panes, each pane is roughly 38×9 — Charm-style padding
would consume about a quarter of the user's actual work.

So the rule is: **take opencode's softness and restraint, not its spacing budget.**

- Chrome cells are cells the user's work does not get. Every one must earn itself.
- A treatment that looks better at 200×50 but costs a row at 80×24 is rejected.
- Prefer color, attribute, and glyph choices — which are free — over spacing, which is
  not. Softness should come from `╭╮╰╯`, half-block edges, and surface elevation, not
  from whitespace.
- Where you do spend a cell on padding, state what the user gave up for it.
- Judge every design at 80×24 with four panes before judging it anywhere else. If it
  only sings at the hero width, it has failed.

## Do not

- Re-open resolved decisions (focus treatment, always-on status, theming model, motion).
- Specify anything with a pixel measurement, a font size, an opacity, or a radius.
- Introduce a treatment with no 16-color and no ASCII answer.
- Design chrome for data that does not exist.
- Spend a cell on air that a color or a glyph could have spent instead.
