# Review Notes

## Highest-risk remaining gaps

- `AppState` still exists as the dominant facade even after the decomposition work.
- `pairing_wire.rs` still owns a large daemon-side nearby-pairing workflow surface.
- `.agents/` remains ignored by repo defaults, so new planning artifacts still need explicit staging discipline when they change.
- Rust incremental compilation on this machine continues to emit Windows finalization warnings (`Access is denied. (os error 5)`), though they did not block validation.

## Architectural review verdict

- The daemon-first refactor goal is materially achieved.
- The new boundary crates are now real ownership seams rather than empty placeholders.
- CLI and tray are notably thinner and now share helper logic instead of duplicating endpoint/layout/launch behavior.
- The remaining risks are now release hardening and endgame cleanup, not architecture rescue.

## Latest executed validation

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `scripts/dev/two-node-smoke.ps1 -TimeoutSeconds 60`
- `scripts/dev/three-node-smoke.ps1 -TimeoutSeconds 90`
- `scripts/dev/installer-smoke.ps1 -Version 2.1.0`
- `scripts/dev/edge-handoff-trace.ps1 -EndpointA http://127.0.0.1:56052 -LabelA node2 -DurationSeconds 20 -EnforceBudgets`
- `scripts/dev/edge-handoff-trace.ps1 -EndpointA http://127.0.0.1:56051 -LabelA node1 -DurationSeconds 20 -EnforceBudgets`
- `scripts/dev/input-trace-matrix.ps1 -TracePaths @(...edge-handoff-trace-node1.log, ...edge-handoff-trace-node2.log) -Scenario edge_handoff -Topology 2-node-loopback`
- `scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:56051 -EndpointB http://127.0.0.1:56052 -ResponderHost 127.0.0.1 -ResponderPairingPort 56201 -Mode full`
- `scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:56051 -EndpointB http://127.0.0.1:56052 -ResponderHost 127.0.0.1 -ResponderPairingPort 56201 -Mode lockout-only`
