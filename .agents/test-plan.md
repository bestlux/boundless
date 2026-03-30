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

## Release-oriented validation still required

- Trace-budget enforcement:
  - `scripts/dev/edge-handoff-trace.ps1`
  - `scripts/dev/input-trace-matrix.ps1`
- Recovery automation:
  - `scripts/dev/s4-recovery-automation.ps1`

## Evidence summary

- Two-node smoke: passed.
- Three-node smoke: passed.
- Installer smoke: failed on uninstall metadata (`InstallLocation` resolved as literal `[INSTALLDIR]`).
- Trace/recovery: not executed in this run because those scripts require live endpoint setups they do not self-provision.
