# Multi-Display Validation Runbook (Slice 2)

This runbook is the execution plan for v1 Slice 2: real multi-display validation.

## Scope

- Validate runtime behavior on two real Windows machines with multiple monitors.
- Confirm edge handoff behavior matches layout and feature toggles.
- Confirm `switch_all` behavior remains deterministic in live sessions.
- Capture reproducible artifacts for any failures.

## Test Matrix

1. Topology A
- Machine A: 2 monitors, primary on left.
- Machine B: 1 monitor.
- Layout: `right,self` from Machine A perspective.

2. Topology B
- Machine A: 1 monitor.
- Machine B: 2 monitors, primary on right.
- Layout: `self,right`.

3. Topology C
- Machine A: 2 monitors.
- Machine B: 2 monitors.
- Layout: `left,self,right` where left/right are distinct peers in 3-node local simulation or repeated with a third machine when available.

## Preconditions

- Latest `main` build on all involved machines.
- Pairing/trust imported both directions.
- Peers connected (`boundlessctl peer list` shows `connected=true`).
- `share_input=true`, `easy_mouse=true`, `wrap_mouse=true` unless the test case says otherwise.

## Core Scenarios

1. Edge handoff in enabled mode
- Set capture target to neighbor peer.
- Move cursor to handoff edge repeatedly.
- Expected: capture target transitions once per edge crossing and does not flap.

2. Edge handoff disabled by `easy_mouse=false`
- Disable `easy_mouse`.
- Repeat edge movement.
- Expected: no capture target transition via edge movement.

3. Edge handoff with `wrap_mouse=false`
- Enable `easy_mouse`, disable `wrap_mouse`.
- Repeat edge movement.
- Expected: handoff behavior follows non-wrap policy without unintended switches.

4. `switch_all` hotkey behavior in live session
- Configure hotkey and press repeatedly.
- Expected: rotation follows layout-first order, skipping disconnected peers.

5. Reconnect recovery
- Trigger reconnect (`diagnostics run-action reconnect`).
- Expected: peers return connected and capture/input routing can be resumed.

## Evidence Collection Commands

Run on both machines before and after each scenario:

```powershell
boundlessctl daemon status
boundlessctl peer list
boundlessctl layout show
boundlessctl feature list
boundlessctl input capture-target
boundlessctl transport events --limit 200
```

Preferred capture helper (run from either machine with reachable endpoints):

```powershell
scripts/dev/multi-display-capture.ps1 `
  -Scenario "edge_enabled_topology_a" `
  -Phase before `
  -EndpointA "http://<machine-a-api-host>:50051" `
  -EndpointB "http://<machine-b-api-host>:50051" `
  -LabelA machine-a `
  -LabelB machine-b
```

Repeat with `-Phase after` (or `-Phase failure`) for each scenario.

For timeline-style edge debugging while reproducing movement live:

```powershell
scripts/dev/edge-handoff-trace.ps1 `
  -EndpointA "http://<machine-a-api-host>:50051" `
  -EndpointB "http://<machine-b-api-host>:50051" `
  -LabelA machine-a `
  -LabelB machine-b `
  -DurationSeconds 45
```

For deterministic action checks:

```powershell
boundlessctl diagnostics run-action toggle_easy_mouse
boundlessctl diagnostics run-action switch_all
boundlessctl diagnostics run-action reconnect
```

## Failure Logging

For each failure capture:

- Exact wall-clock timestamp.
- Scenario ID from this runbook.
- Expected vs actual behavior.
- `transport events --limit 200` output from both machines.
- Daemon stdout/stderr logs from both machines.
- Layout/feature snapshot at failure time.

## Exit Criteria

- All core scenarios pass across Topology A and Topology B.
- No stuck input-owner/capture-target state after reconnect cycles.
- No reproducible flapping or unintended edge handoff transitions.
- Any remaining issues are documented with repro steps and severity.
