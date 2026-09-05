# Superseded snapshot — 2026-09-04

This is preserved historical source from commit 9bad1b5c36e31aa81f8764e72fed66785f85c05a. It is not current implementation guidance, release policy, or evidence of later fixes. The original body follows unchanged; its relative links retain their original locations. Use the [original source](https://github.com/bestlux/boundless/blob/9bad1b5c36e31aa81f8764e72fed66785f85c05a/docs/parity/v5-input-handoff.md) to follow historical references, or the current docs index for the active contract.

---

# V5 Input Handoff Contract

V5 input handoff is the product center of Boundless. This milestone adds the durable input policy and diagnostics surface needed before tray settings and release validation can claim final parity.

## Implemented In This Milestone

- `input_handoff` is a persisted runtime config section with:
  - `block_screen_corners`
  - `corner_block_px`
  - `relative_mouse`
  - `hide_cursor_at_edge`
  - `draw_cursor_marker`
- `boundlessctl input config` prints the effective handoff policy.
- `boundlessctl input set-config` updates one or more handoff policy fields while preserving unspecified values.
- `boundlessctl input status` prints owner, configured capture target, active capture target, lock status, capture backend mode, inject queue depth, inject high-water mark, and handoff policy.
- The daemon records the active capture backend mode as runtime state instead of only a historical transport event.
- Corner blocking is enforced in the daemon edge-switch path and in the shared `core-input` edge-switch helper.
- Diagnostics dumps include input handoff policy, capture backend mode, pending inject depth, and inject high-water mark.

## Current Runtime Behavior

Edge switching remains controlled by:

- `share_input`
- `easy_mouse`
- `wrap_mouse`
- the configured layout matrix
- active peer connectivity
- `input_handoff.block_screen_corners`
- `input_handoff.corner_block_px`

When corner blocking is enabled, horizontal switches are blocked near the top and bottom edge corners, and vertical switches are blocked near the left and right edge corners. This reduces accidental switches into remote machines from close, resize, taskbar, and corner UI gestures.

## Honest Limitations

- `relative_mouse` is persisted and visible, but absolute/relative runtime mode selection still needs Windows mixed-DPI validation before it can be marked validated.
- `hide_cursor_at_edge` is persisted and visible, but cursor hiding is not yet applied by the Windows runtime.
- `draw_cursor_marker` is persisted and visible, but no tray or overlay cursor marker is shipped yet.
- Fullscreen suppression and fullscreen allowlists are not implemented.
- Direct per-machine hotkeys and multi-cast input mode remain incomplete.
- Release validation still needs real Windows trace evidence from `scripts/dev/edge-handoff-trace.ps1` and `scripts/dev/input-trace-matrix.ps1`.

## Validation Targets

Before V5 can mark input handoff validated, the readiness packet must include:

- unit tests for policy persistence, config migration, corner blocking, capture backend mode, and queue counters,
- two-node edge handoff trace output,
- mixed-DPI and multi-display trace output,
- escape/release recovery evidence,
- hook mode and polling fallback evidence,
- input queue overflow/drop evidence,
- and a clear statement for any skipped fullscreen, cursor hiding, or cursor marker scenario.
