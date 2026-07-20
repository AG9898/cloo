# Handoff: cloo — terminal multiplexer UI

## Overview
cloo is a client-server terminal multiplexer (tmux/zellij peer) written in Rust. Its entire
reason to exist is looking better than tmux — all chrome is rendered **client-side**: pane
borders, focus treatment, status bar, theming, and motion. This package is the visual reference
for that chrome: a set of high-fidelity mock screens showing the intended look of panes, splits,
the status bar, and the supporting UI (command palette, session switcher, config, toasts).

Source repo context: `AG9898/cloo` (`docs/PRD.md`, `docs/ARCHITECTURE.md`, `docs/DECISIONS.md`).
Four visual decisions were still open in the repo; this mock commits them:
- **OPEN-01 focus treatment** → **accent border + dimmed neighbors** (both).
- **OPEN-02 status bar** → **always-on**, shown in two variants (minimal + powerline).
- **OPEN-03 theming** → base16-style named themes, palette inheritance, 16-color safe.
- **OPEN-04 motion** → frame-budgeted (see Interactions).

## About the Design Files
The file in this bundle (`cloo Mock.dc.html`) is a **design reference created in HTML** — a
prototype showing intended look and behavior, **not production code to copy directly**. cloo's
real client renders to a terminal grid in Rust, not the DOM. The task is to **translate this
visual language into the Rust terminal renderer** (`cloo-client`): the colors, borders, focus
dimming, status-bar composition, and overlay layouts documented below, expressed as cell/ANSI
drawing. Where a treatment can't survive a plain 16-color TTY, degrade it deliberately (this is
a hard project constraint).

`cloo Mock.dc.html` is a "Design Component" — open it in a browser alongside `support.js`
(both included) to view. It is a single scrolling board of 8 labeled cards.

## Fidelity
**High-fidelity.** Final colors (exact hex below), typography (JetBrains Mono), spacing, borders,
and focus/dim treatment are all intentional and specified. Recreate them faithfully in the
terminal renderer, adapting px measurements to terminal cells where needed (e.g. an 8px gutter →
a 1-cell gutter; an 11px title bar → a 1-row pane header).

---

## Design Tokens

### Palette — "Storm" (Tokyo-Night family)
| Role | Hex | Use |
|---|---|---|
| App background (behind panes / gutters) | `#0f0f16` | the gap ground between panes |
| Frame background | `#16161e` | outer terminal frame |
| Pane background | `#1a1b26` | pane body |
| Surface (tab/status bars) | `#1a1b26` | chrome rows |
| Surface raised (active tab, panels) | `#24283b` / `#211f2e` | active tab, overlays |
| Pane title bar (focused) | `#1e1c2b` | focused pane header |
| Pane title bar (unfocused) | `#1c1d29` | dimmed pane header |
| Border (default) | `#2a2e42` | frame + unfocused pane |
| Border (soft inner) | `#22263a` / `#23263a` | dividers, dimmed pane border |
| **Accent (focus / highlight)** | **`#bb9af7`** | focused border, active status segment, cursor, selection |
| Foreground (bright) | `#c0caf5` | primary text |
| Foreground (default) | `#a9b1d6` | terminal body text |
| Muted | `#565f89` | secondary labels, comments |
| Muted deep | `#414868` / `#3b4261` | line numbers, inactive tabs |
| Green (success / prompt) | `#9ece6a` | `$` prompt, ok, INFO ✓ |
| Yellow (warn / hashes) | `#e0af68` | WARN, git hashes, modified |
| Red (error / bell) | `#f7768e` | ERROR, bell toast |
| Cyan (paths / info) | `#7dcfff` | paths, session names, types, INFO |
| Blue (secondary) | `#7aa2f7` | functions, detach toast |
| Orange (numbers) | `#ff9e64` | numeric literals |

Dim treatment for unfocused panes: `opacity: 0.48` (≈ config `dim_unfocused = 0.48`). In the
terminal this is a contrast reduction toward the background, not a real alpha.

### Named themes (swatch sets, card 06)
- **storm** (active): `#bb9af7 #7aa2f7 #7dcfff #9ece6a #f7768e`
- **night**: `#9d7cd8 #7aa2f7 #73daca #9ece6a #f7768e`
- **gruvbox**: `#d3869b #83a598 #8ec07c #fabd2f #fb4934`
- **nord**: `#b48ead #81a1c1 #88c0d0 #a3be8c #bf616a`

### Typography
- **Monospace (all terminal content + chrome):** JetBrains Mono — 400/500/600/700.
  - Terminal body: 13px / line-height 1.6 (12px/1.6 in dense right-stack panes).
  - Pane title bar: 11px / 500.
  - Tab labels: 12px / 500–600. Status bar: 11px / 600.
- **UI sans (board labels only, not part of the product):** Inter.

### Spacing / geometry (px in mock → terminal intent)
- Pane gutter / gap: **8px** → 1 cell.
- Pane border-radius: **7–8px** (frame 11px). Terminals can't round cells; express focus with
  border weight/color instead. **The mock's rounding is a browser affordance, not a requirement.**
- Tab bar height 40px; pane title bar 27–28px (→ 1 row); status bar 28–30px (→ 1 row).
- Frame shadow (mock only): `0 8px 24px -16px rgba(0,0,0,.5)` — cosmetic, drop in terminal.

---

## Screens / Views
All 8 cards are "bare chrome" — no OS window, edge-to-edge, as if screenshotting cloo itself.

### 01 · Single full-screen pane
- **Purpose:** baseline — one shell, one tab, one focused pane.
- **Layout:** tab bar (top) → single pane filling the area with 8px inset → minimal status bar.
- **Components:** tab bar with session badge `◈ main` (accent-on-surface pill) + tabs
  (`1 shell` active with a 2px accent underline via `box-shadow: inset 0 -2px 0 #bb9af7`,
  others muted); right-aligned `1 pane · 60fps`. Focused pane has a `#bb9af7` 1px border,
  a title bar (`[1] zsh  …  ~/dev/cloo · 48231`) with an accent index badge, and a colored
  shell transcript (green `$`, yellow git hashes, cyan paths, green `ok`) ending in a blinking
  accent block cursor. Minimal status bar: accent `◈ main` segment, tab summary, `⎇ main`,
  `C-b`, clock `14:32`.

### 02 · Vertical split · focus treatment
- **Purpose:** demonstrate the focus decision (OPEN-01).
- **Layout:** two equal panes side by side, 8px gutter.
- **Left (focused):** full contrast, accent border, title `[1] cargo watch -x test`, cargo test
  output. **Right (unfocused):** `opacity 0.48`, neutral `#23263a` border, muted index badge.
- **Status bar:** powerline variant — segmented `NORMAL` (accent) → `◈ main` → `⎇ main +2` →
  right-aligned `2 clients · min 132×38` + clock. (In the mock the powerline chevrons are flat
  colored segments; in the terminal, use the `` powerline glyph if the font provides it, else
  flat segments — must degrade on 16-color.)

### 03 · Nested dev layout · pane titles · toasts
- **Purpose:** the real working layout + notifications.
- **Layout:** left editor pane (`flex 1.55`), right column (`flex 1`) split into two stacked
  panes (logs, shell) with an 8px gap. This is the binary split tree: `Split(V, 0.6, Leaf,
  Split(H, 0.5, Leaf, Leaf))`.
- **Editor (focused):** nvim on `layout.rs`, Rust syntax coloring (keywords `#bb9af7`, types
  `#7dcfff`, `self` `#f7768e`, numbers `#ff9e64`, comments `#565f89`, line numbers `#3b4261`),
  plus a nvim-style mode line (accent `NORMAL` block + file + `rust · utf-8`).
- **Logs (unfocused, dimmed):** `cargo run -p cloo-server` with leveled lines — INFO `#7dcfff`,
  WARN `#e0af68`, BELL `#f7768e`.
- **Shell (unfocused, dimmed):** `cloo ls` session list.
- **Toasts** (top-right, stacked, staggered slide-in): BELL (red left-border), ⧉ DET / detach
  (blue), ⤢ RSZ / resize (muted). Panel `#1c1d2a`, 3px colored left border, title + subtext.

### 04 · Prefix palette · keybinding hints
- **Purpose:** command palette invoked after the prefix (C-b).
- **Layout:** layout behind at `opacity 0.32` under a `rgba(9,9,14,.72)` scrim; centered panel
  560px, `#191a24` on `#33374d` border.
- **Components:** header = accent `C-b` chip + live query `split` + blinking cursor +
  `4 of 24`. Rows: selected row has `#221f31` fill + 2px accent left border + `▸`; each row is
  `name … [key chip]` (e.g. `Split pane right … C-b |`). Footer hint row: `↑↓ move · ⏎ run ·
  ⇥ next group · esc dismiss`.
- **Command → chord reference (from repo Action enum shape):** Split right `C-b |`, Split down
  `C-b -`, Even ratios `C-b =`, Swap pane `C-b s`, plus focus `h/j/k/l`, close `x`, new tab `c`,
  rename `,`, copy-mode `[`, detach `d`.

### 05 · Session switcher
- **Purpose:** pick among sessions the daemon owns.
- **Layout:** same scrim; centered 600px panel.
- **Components:** header `Sessions … 3 alive`. Rows: selected = accent fill + `▸`; each row is
  `name · tabs/panes · status · age`. Status dot green `#9ece6a` (attached ×2) or muted
  `#414868` (detached). Footer: `⏎ attach · d detach · ⌫ kill · n new · esc close`.

### 06 · Config & theming
- **Purpose:** TOML config + live theme preview.
- **Layout:** two columns split by a 1px rule. Left: `config.toml` with TOML syntax coloring
  (tables `#7dcfff`, keys `#bb9af7`, strings `#9ece6a`, bools/numbers `#ff9e64`, comments
  `#565f89`). Right: THEMES list (each row = name + 5 swatch chips; active row has accent ring)
  then a LIVE PREVIEW showing a focused (accent border) + dimmed pane pair.
- **Config keys shown:** `[theme] name/accent/inherit_terminal`, `[focus] border/dim_unfocused`,
  `[status] mode/segments`, `[motion] enabled/duration_ms/reduce`, `[keys] prefix`.
  Path: `~/.config/cloo/config.toml`, live-reloaded on SIGHUP.

### 07 · Status bar · two variants
- **Minimal:** single flat row — accent `◈ main` segment, then tab cells (active on `#211f2e`,
  others muted), right side `⎇ main +2`, `C-b`, clock, separated by `#262a3d` dividers.
- **Powerline:** segmented — `NORMAL` (accent) · `◈ main` (`#2a2e42`) · `1 dev` (`#232537`) ·
  `⎇ main +2` (`#1e2030`, green) · right `2 clients` + clock (accent). Both must render legibly
  on a 16-color TTY (that's why minimal exists).

### 08 · Pane resize · drag gutter
- **Purpose:** show resize affordance.
- **Layout:** two panes at ratio 0.62 / 0.38 with an 8px gutter. The gutter holds a **lit accent
  divider** (`3px × 56px`, `#bb9af7`) marking the active drag target. Title reads `resize ·
  ratio 0.62`. Right pane shows an htop-style bar readout. Resize edits **ratios, not cell
  counts** (repo RESOLVED-03).

---

## Interactions & Behavior
- **Focus change:** focused pane = accent 1px border; all others drop to ~0.48 contrast. Switch
  with `C-b h/j/k/l` or click-to-focus (mouse, SGR 1006).
- **Split/close:** animated, **frame-budgeted and interruptible**, with a reduce-motion setting
  (`[motion] duration_ms = 120`). Never exceed the frame budget (~60fps damage cap).
- **Cursor:** blink ~1.1s steps(1) (accent block).
- **Command palette / session switcher:** open over a dimmed+scrim backdrop; arrow keys move
  selection (accent fill + left border + `▸`), `⏎` runs/attaches, `esc` dismisses.
- **Toasts:** slide in from the right, staggered ~70ms, for bell / detach / resize events;
  auto-dismiss.
- **Resize:** grab the gutter (mouse drag) or `C-b` arrows; active divider lights accent; ratios
  update live and a single layout pass re-issues `TIOCSWINSZ`.
- **Multi-client:** two clients render at the **minimum** of both sizes; status bar shows
  `N clients · min WxH`.

## State (product side, for reference)
Server owns all state (PTYs, grids, scrollback, layout tree). Client caches the visible grid +
layout geometry only and decides all chrome. Relevant to this UI: focused `PaneId`, layout tree
(ratios), active tab, session list, theme selection, motion/reduce flag, transient toast queue,
palette/switcher open + query + selection index.

## Assets
None external. All glyphs are Unicode (`◈ ● ▸ ⎇ ⧉ ⤢ ⏎ ⌫ ↑↓ ⇥`) — no emoji, no image files.
Icons in the real client should follow the repo's terminal-glyph approach; the repo guide also
references Lucide for any GUI surfaces. Font: JetBrains Mono (Google Fonts) — swap for the user's
configured terminal font in the real client (palette inheritance is a stated goal).

## Files
- `cloo Mock.dc.html` — the 8-card visual reference (this bundle).
- `support.js` — runtime required to open the .dc.html in a browser.
- Repo source of truth (not in bundle): `AG9898/cloo` → `docs/PRD.md`, `docs/ARCHITECTURE.md`,
  `docs/DECISIONS.md`, `docs/CONVENTIONS.md`.

## Note on the bound design system
This mock intentionally does **not** use the project's "Modernist" design system: that is a
light, red-on-white grid system for GUI/marketing surfaces, whereas cloo is a dark monospace
terminal interface. Use Modernist only if you build a cloo landing page or docs site; the
terminal chrome follows the Storm palette above.
