# Documentation Index

Canonical navigation map for this repository's documentation.

This is the single source of truth for doc locations. When adding, removing, renaming, or
moving any file under `docs/`, update this file in the same commit.

---

## Core Docs (`docs/`)

| Path | Purpose |
|---|---|
| [`docs/INDEX.md`](INDEX.md) | This file — documentation navigation map |
| [`docs/PRD.md`](PRD.md) | Product requirements, scope, users, and success criteria |
| [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) | System topology, runtime boundaries, and component responsibilities |
| [`docs/CONVENTIONS.md`](CONVENTIONS.md) | Coding standards, naming rules, and idiomatic patterns |
| [`docs/DECISIONS.md`](DECISIONS.md) | Architectural decision log (open and resolved) |
| [`docs/ENV_VARS.md`](ENV_VARS.md) | Canonical environment variable and secret matrix |
| [`docs/TESTING.md`](TESTING.md) | Test strategy, how to run, file inventory, and writing new tests |
| [`docs/STYLEGUIDE.md`](STYLEGUIDE.md) | Canonical terminal-chrome visual language and degradation rules |
| [`docs/BRANDING.md`](BRANDING.md) | Canonical external brand-mark system, asset roles, and export rules |
| [`docs/AGENT_WORKFLOWS.md`](AGENT_WORKFLOWS.md) | Coding-harness profiles, attention states, and compatibility contract |
| [`docs/workboard.json`](workboard.json) | Active task queue (canonical board) |
| [`docs/workboard.schema.json`](workboard.schema.json) | JSON Schema contract for workboard structure and required task fields |
| [`docs/workboard.md`](workboard.md) | Workboard semantics, field definitions, and agent usage rules |

<!-- Candidate project-specific docs, once there is something to put in them:
| [`docs/PROTOCOL.md`](PROTOCOL.md) | Wire message reference, if it outgrows ARCHITECTURE.md |
| [`docs/KEYMAP.md`](KEYMAP.md) | Default keybindings and the Action enum |
| [`docs/THEMING.md`](THEMING.md) | Theme format and palette inheritance — expect this at M4 |
-->

> **Note:** the root `DESIGN.md` was migrated into `PRD.md`, `ARCHITECTURE.md`, and
> `DECISIONS.md` on 2026-07-20. Do not recreate it — see the maintenance rule below about
> root-level stubs.

---

## Maintenance Rules

- When adding a doc: add its row to the correct section in the same commit.
- When removing a doc: remove its row and update every doc that linked to it.
- When renaming or moving a doc: update its row and all inbound links in the same commit.
- `AGENTS.md` (root) and any section `README.md` files must be updated in the same commit
  when the docs they reference change.
- Never add a root-level stub doc that only redirects to a path inside `docs/`.
