# Test Plan

## Merge-blocking validation used on this branch

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Focused crate checks while landing each slice:
  - daemon state/control-plane work
  - `platform-windows`
  - `peer-transport`
  - CLI/tray adapter rewiring

## Release-oriented validation executed

- `scripts/dev/two-node-smoke.ps1 -TimeoutSeconds 60`
- `scripts/dev/three-node-smoke.ps1 -TimeoutSeconds 90`
- `scripts/dev/installer-smoke.ps1 -Version 2.1.0`
- `scripts/dev/edge-handoff-trace.ps1 -EndpointA http://127.0.0.1:56052 -LabelA node2 -DurationSeconds 20 -EnforceBudgets`
- `scripts/dev/edge-handoff-trace.ps1 -EndpointA http://127.0.0.1:56051 -LabelA node1 -DurationSeconds 20 -EnforceBudgets`
- `scripts/dev/input-trace-matrix.ps1 -TracePaths @(...edge-handoff-trace-node1.log, ...edge-handoff-trace-node2.log) -Scenario edge_handoff -Topology 2-node-loopback`
- `scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:56051 -EndpointB http://127.0.0.1:56052 -ResponderHost 127.0.0.1 -ResponderPairingPort 56201 -Mode full`
- `scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:56051 -EndpointB http://127.0.0.1:56052 -ResponderHost 127.0.0.1 -ResponderPairingPort 56201 -Mode lockout-only`

## Evidence summary

- Two-node smoke: passed.
- Three-node smoke: passed.
- Installer smoke: passed after removing the invalid `ARPINSTALLLOCATION` property from `packaging/windows/installer/Package.wxs`.
- Trace budgets: passed for both one-way receive/apply runs; matrix rows=`2`, pass=`2`, fail=`0`.
- Recovery automation: passed for reject, expiry, successful recovery, and a separate lockout-only run.
