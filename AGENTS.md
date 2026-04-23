# Boundless Repo Instructions

This file adds repo-specific guidance on top of the machine-wide defaults.
Keep it short and update it only when Boundless-specific workflows repeat.

## Repo Shape

- Rust workspace with shared policy crates under `crates/core-*`, daemon runtime in `crates/daemon`, IPC DTO/protobuf surface in `crates/ipc-api`, gRPC adapter in `crates/adapter-ipc-grpc`, CLI in `crates/cli`, Windows platform glue in `crates/platform-windows`, and tray UI in `crates/tray`.
- Windows is the primary product target. Linux CI/build coverage exists, but do not infer feature parity for runtime, tray, input capture, installer, or named-pipe behavior.
- The tray dashboard is the canonical Windows first-run UX. The CLI setup/console paths are automation and diagnostics fallbacks.
- `docs/architecture/network-v1.md` is the current transport ownership map. Keep network/session/backpressure changes aligned with that document or update it in the same change.

## Change Discipline

- Prefer narrow changes that respect the existing crate boundaries.
- Keep daemon runtime state changes covered by focused tests. The highest-risk areas are input capture/handoff, clipboard replay/dedupe, pairing recovery, transport reconnect/backpressure, installer startup, and release metadata.
- Avoid moving workflow logic into the tray unless the UX truly owns it. Shared CLI/tray behavior should usually live in `app-services`, `ipc-api`, `adapter-ipc-grpc`, or a shared helper rather than being duplicated.
- Treat file-transfer, clipboard, anti-idle, and input-routing plumbing as separate from product readiness. Do not claim a feature is user-ready just because protocol/state plumbing exists.
- When editing release or packaging files, check version propagation across `Cargo.toml`, crate manifests, `packaging/windows/package-manifest.json`, MSI asset names, and release-please config.

## Validation

- Small docs-only changes: inspect the diff; no full test run is normally needed.
- Rust behavior changes: run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.
- Faster targeted checks are acceptable while iterating, but state clearly what was and was not validated.
- Windows runtime, tray, input, installer, or packaging changes need the smallest matching PowerShell validation from `scripts/dev` or `scripts/release`.
- The broad local gate is:

```powershell
./scripts/dev/test-suite.ps1 -Profile quick
```

- Smoke profiles escalate by risk:
  - `./scripts/dev/test-suite.ps1 -Profile smoke` for two-node smoke.
  - `./scripts/dev/test-suite.ps1 -Profile full` for three-node smoke.
  - `./scripts/dev/installer-smoke.ps1 -KeepArtifacts` for installer/startup validation.
  - `./scripts/dev/two-node-smoke.ps1 -ExtendedCoverage` when matching the extended Windows CI lane.

## Windows Runtime Triage

- Default local control endpoint on Windows is `npipe://./pipe/boundlessd-api`; tray and CLI defaults should match daemon config.
- For startup/fresh-install symptoms, first verify daemon health with `boundlessctl daemon status` before treating missing UI state as a tray/layout bug.
- A config `layout_matrix` of `self` confirms config creation only; it does not prove daemon health or named-pipe availability.
- Named-pipe failures are diagnostic:
  - `Access is denied. (os error 5)` can mean a stale daemon still owns `\\.\pipe\boundlessd-api`.
  - `The system cannot find the file specified. (os error 2)` usually means no daemon recreated the pipe.
- For input handoff issues, `boundlessctl input capture-start <peer>` plus `boundlessctl daemon status` is the fastest forced-state probe for `input_locked` and `active_capture_target`.
- If a runtime issue disappears after a full tray/daemon shutdown and relaunch, preserve that fact. Do not overfit a code patch without a fresh repro or logs.

## Release And Installer Notes

- Releases are driven by release-please. Conventional Commit intent matters.
- Release-please state is label-sensitive; stale `autorelease: pending` labels on already-published release PRs can block later releases.
- The Windows release path should produce and validate an MSI, not just binaries.
- Installer validation should check tray count, daemon count, and named-pipe/API health, not only file installation.
- Windows signing is currently optional unless release policy variables require it; do not silently turn signing into a hard gate.

## Useful Commands

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/dev/test-suite.ps1 -Profile quick
cargo run -p boundless-cli -- daemon status
cargo run -p boundless-daemon -- print-config-path
```
