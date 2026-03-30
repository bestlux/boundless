# Review Notes

## Highest-risk remaining gaps

- `AppState` still exists as the dominant facade even after the decomposition work.
- `pairing_wire.rs` still owns a large daemon-side nearby-pairing workflow surface.
- Trace-budget and recovery gates still need real execution evidence.
- Installer validation exposed a concrete packaging defect:
  - uninstall `InstallLocation` resolves to literal `[INSTALLDIR]`
  - WiX warns on `ARPINSTALLLOCATION` in `packaging/windows/installer/Package.wxs`
- `.agents/` remains gitignored in this repo, so these artifacts are local program records unless repo policy changes.
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
- `scripts/dev/installer-smoke.ps1 -Version 2.1.0` (failed on installer metadata)
