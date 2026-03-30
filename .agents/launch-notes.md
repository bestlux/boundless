# Launch Notes

## Branch posture

- This branch finishes the daemon-first breaking refactor program for the current alpha line.
- `boundlessd` is now the runtime core and the primary architectural artifact.
- Tray remains the default Windows UX, but CLI and tray are now thinner local adapters over the same daemon-owned control plane.

## Delivered architecture

- Boundary crates are live, not placeholders:
  - `app-services` owns shared control-plane contracts and shared desktop helper logic.
  - `daemon-host` owns daemon lifecycle/bootstrap seams.
  - `adapter-ipc-grpc` serves the live v2 local control plane.
  - `platform-windows` owns Windows clipboard and low-level input helper/runtime pieces that were extracted from daemon modules.
  - `peer-transport` owns transport runtime state, queue/credit helpers, wake/backoff helpers, and transport-local limits.
- Legacy unary daemon services were removed.
- `AppState` is no longer a giant implementation file; it is now split across focused state/ops modules, although it still remains the dominant facade.
- Tray and CLI no longer duplicate core nearby-pairing socket workflows, and they now consume shared desktop helper logic for daemon launch, endpoint parsing, and layout shaping.

## Release-gate evidence captured on this machine

- Passed:
  - `cargo check --workspace`
  - `cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `scripts/dev/two-node-smoke.ps1 -TimeoutSeconds 60`
  - `scripts/dev/three-node-smoke.ps1 -TimeoutSeconds 90`
- Failed:
  - `scripts/dev/installer-smoke.ps1 -Version 2.1.0`
    - Packaging succeeded and produced an MSI.
    - Validation failed on uninstall metadata with `Unexpected uninstall InstallLocation: [INSTALLDIR]`.
    - WiX also emitted `WIX1077` on `ARPINSTALLLOCATION` in `packaging/windows/installer/Package.wxs`.
- Not executed here:
  - Trace-budget enforcement
  - Recovery automation
  - Those scripts require live endpoint setups that they do not self-provision in the same way as the smoke scripts.

## Readiness verdict

- Architectural refactor scope: complete for this branch.
- Release readiness: not ready until the installer metadata issue is fixed and trace/recovery gates are executed with valid endpoint setups.
