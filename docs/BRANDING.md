# cloo Brand System

> Canonical external identity system for cloo. The terminal's cell-based visual language remains
> in [`STYLEGUIDE.md`](STYLEGUIDE.md); this document governs brand marks on surfaces where raster
> or vector assets are appropriate.

## Purpose and boundary

cloo is a calm, capable terminal workspace for concurrent coding work. Its identity takes the
terminal prompt, a rounded terminal frame, and the Storm palette out of the terminal without
turning the running multiplexer into a graphical application.

The terminal renderer never uses these image assets. It preserves the style guide's compact
ASCII-first signals, solid theme tokens, and 16-colour fallback. Brand gradients, rounded outer
frames, and any raster export are for external surfaces: application metadata, documentation,
release material, a future website, and social profiles.

This is one brand with role-specific marks—not several products. `workspace`, `command`, and
`agent signal` are internal asset roles, never public product names or independent app icons.

## Mark hierarchy

| Role | Source asset | Use it for | Do not use it for |
|---|---|---|---|
| **Product** | [`cloo-product.svg`](assets/brand/cloo-product.svg) | The primary app icon, GitHub/social avatar, README hero, large release art | A small favicon where its internal detail will collapse |
| **Workspace** | [`cloo-workspace.svg`](assets/brand/cloo-workspace.svg) | Persistent-session, multi-pane, and multi-client storytelling | A competing primary logo or a terminal-chrome icon |
| **Command** | [`cloo-command.svg`](assets/brand/cloo-command.svg) | 16–24 px favicon, install/CLI badges, compact docs navigation | A standalone master brand without the cloo wordmark nearby |
| **Agent signal** | [`cloo-agent-signal.svg`](assets/brand/cloo-agent-signal.svg) | Agent-workflow feature art, integrations, and restrained animated/empty states | App packaging, favicon, terminal attention state, or another product family |

The **product mark** is the only mark that represents `cloo` without supporting copy. It is the
rounded terminal face chosen from the R2C3 face-concept refinement. The workspace mark derives
from contact-sheet icon 6, the command mark from icon 10, and the agent signal from the R1C2
refinement.

## Source kit

All sources use a `512 × 512` view box and are editable SVGs. They are the masters; never derive
new masters from the exploratory PNG sheets or an image-generation preview.

| Asset | Variant | Intended background |
|---|---|---|
| [`cloo-product.svg`](assets/brand/cloo-product.svg) | Full colour product app tile | Storm frame `#0f0f16` |
| [`cloo-product-mono.svg`](assets/brand/cloo-product-mono.svg) | One colour; set `color` in the consuming surface | Transparent / caller-controlled |
| [`cloo-workspace.svg`](assets/brand/cloo-workspace.svg) | Full colour companion tile | Storm frame `#0f0f16` |
| [`cloo-command.svg`](assets/brand/cloo-command.svg) | Full colour compact prompt | Transparent / caller-controlled |
| [`cloo-command-mono.svg`](assets/brand/cloo-command-mono.svg) | One colour; set `color` in the consuming surface | Transparent / caller-controlled |
| [`cloo-agent-signal.svg`](assets/brand/cloo-agent-signal.svg) | Full colour secondary motif | Storm frame `#0f0f16` |

The SVGs intentionally carry descriptive `<title>` and `<desc>` metadata. Embed them with useful
alternative text when the mark communicates meaning; use an empty alt only when immediately
adjacent text already names cloo.

## Construction and colour

The shared visual DNA is deliberate:

- A square-friendly silhouette, one rounded terminal frame, and a 45-degree prompt chevron.
- Compact, geometric strokes and flat surfaces—never a 3D terminal, a robot, or neon effects.
- On external dark-brand surfaces only, the ordered gradient is `#bb9af7` → `#7aa2f7` →
  `#7dcfff` (accent, blue, info).
- In product UI-adjacent material, prefer solid Storm accent `#bb9af7` with `#c0caf5` on
  `#0f0f16` / `#1a1b26`. This ties the brand to the style guide rather than competing with it.
- Every mark must remain recognizable in a one-colour reproduction. Use the provided product and
  command monochrome sources before creating a bespoke recolour.

Colour is decoration, not meaning. In particular, an agent or attention state must continue to
use the textual glyphs defined in the style guide; no external mark may substitute for `!`, `*`,
or `x` inside the terminal.

## Wordmark and lockups

Use the lowercase spelling **cloo**. Until a custom wordmark is explicitly commissioned, set it
as text in JetBrains Mono SemiBold (or the selected surface's closest well-spaced monospace), not
as AI-generated lettering or outlined copy. Keep a mark-to-wordmark lockup horizontal, with a gap
at least equal to the prompt stem's stroke width; do not append `>_` to the wordmark by default.

Use the product mark with the wordmark for a large first impression. The command mark may precede
`cloo` where space is constrained. At favicon scale, use the command mark alone rather than
squeezing the product mark below its legibility threshold.

## Applications

| Surface | Mark and treatment |
|---|---|
| App/launcher icon, avatar, README hero | Product mark on the dark Storm tile |
| Favicon and tiny package/docs marker | Command mark; prefer one-colour where the host supplies the background |
| Feature art about detach/reattach, sessions, panes, or multi-client work | Workspace mark paired with an explanatory heading |
| Agent profiles, adapters, attention workflow explanation | Agent signal as supporting art, never as status UI |
| Terminal chrome, overlays, status bar, help text | No image mark. Follow [`STYLEGUIDE.md`](STYLEGUIDE.md) and use textual terminal-safe glyphs only. |

## Guardrails

- Do not create a new product logo from a role-specific mark.
- Do not replace terminal chrome's accessible text glyphs with a brand icon.
- Do not use the external gradient as a terminal colour token or a substitute for a 16-colour
  fallback.
- Do not alter the chevron angle, outer-frame proportion, or stroke relationship when making
  exports.
- Do not use a generative-image render as a shipping logo. It is useful for exploration only;
  the editable SVG source is authoritative.

## Release/export checklist

The source kit is intentionally vector-first. When a repository-controlled surface needs raster
assets, export from the matching SVG and verify at its actual size:

- Product mark: `16`, `32`, `64`, `128`, `256`, `512`, and `1024` px; use the command mark rather
  than the product mark if the 16 px result loses the terminal face.
- Command mark: `16`, `24`, `32`, and `48` px, both full-colour and one-colour as the host allows.
- Provide light-background/one-colour treatment only from the mono source, with tested contrast.
- Confirm a meaningful `alt` string and keep the original SVG alongside any derived PNG, ICO, or
  platform-specific package asset.

The [brand-direction board](../output/logo-explorations/cloo-brand-direction-board.png) that
established this hierarchy is an exploratory preview only. The approved source system is the SVG
kit in `docs/assets/brand/`.
