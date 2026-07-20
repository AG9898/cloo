# Agent Workflows and Harness Compatibility

> Canonical contract for running coding harnesses inside cloo. This document describes product
> behavior, not a vendor API integration.

---

## Goal

cloo is a local workspace for many independent coding tasks. A pane may run a shell, Codex,
Claude Code, or another interactive program; each keeps its own PTY and survives client detach.
Profiles make common launches quick, while explicit metadata and attention states make the right
pane discoverable without inspecting every transcript.

## Profiles

A profile supplies a local command template and presentation defaults. The v1 built-ins are
`generic`, `codex`, and `claude`; users may add local profiles in configuration. A profile may set
a launch command, default pane name, recommended minimum dimensions, and a state adapter. It must
not require a vendor account, call a cloud API, or make cloo depend on a vendor package.

The user controls the pane name, task label, and working directory at launch. cloo does not guess
a task from process names or transcript text.

## Attention Contract

The server stores a state and its provenance. Generic sources are child lifecycle, terminal bell,
and an explicit user command. Optional local adapters can report `working`, `needs_input`,
`ready`, or `failed`; adapter data is advisory and visibly identified as such. A pane with no
reliable signal remains `unknown`, not falsely marked working.

The attention queue is a navigation surface, not a notification firehose. It lists the newest
unacknowledged event per pane, coalesces repeats, and supports keyboard focus/acknowledge. The
always-on status bar displays a compact count.

## Compatibility Tiers

| Tier | Contract |
|---|---|
| Required | UTF-8/color, alternate screen, resize, bracketed paste, extended keys, focus events, SGR mouse routing, raw-mode restoration, and normal scrollback behavior. |
| Negotiated | Clipboard, hyperlinks, notifications, terminal-title changes, and progress effects are typed effects applied only when the attached client permits and supports them. |
| Optional | Inline graphics. They may be unavailable through cloo without breaking the harness. |

Claude Code documents that tmux needs extended keys and passthrough for some of its terminal
features; cloo's required and negotiated tiers cover the equivalent responsibilities. Codex
documents that its terminal pets need graphics support and are unavailable inside tmux and Zellij;
cloo therefore makes no graphics compatibility promise for v1. See the [Claude Code terminal
guide](https://code.claude.com/docs/en/terminal-config) and [Codex pets
guide](https://learn.chatgpt.com/docs/pets).

## Safety and Multi-client Rules

No harness output may bypass the renderer. cloo allowlists and capability-gates typed
outer-terminal effects so a child cannot alter terminal state unexpectedly or create divergent
behavior across attached clients. Graphics and other client-local effects are never authoritative
session state.

The session size remains the minimum of attached clients. A profile's recommended size prevents
bad splits on a single client; it cannot make a small second client safe. Zoom is the normal way to
give an active harness full room.

## Compatibility Matrix

Before a supported profile is claimed compatible, test it manually in an installed terminal:

1. Launch in one pane, then in a split layout.
2. Send normal and large bracketed pastes; verify extended-key shortcuts.
3. Exercise focus, mouse, alternate screen, zoom, and repeated resize.
4. Detach and reattach while the harness is active.
5. Verify an attention event appears once and can be acknowledged.
6. Verify unsupported outer-terminal effects degrade without corrupting the pane.

The deterministic escape-sequence fixture suite described in [`TESTING.md`](TESTING.md) is the
automated gate. Real Codex and Claude Code smoke runs are versioned manual evidence, not CI
dependencies.
