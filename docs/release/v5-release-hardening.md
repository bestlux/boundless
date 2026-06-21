# V5 Release Hardening

This document records the release-hardening contract for the Boundless v5 Windows artifact.

## Implemented Controls

- Release metadata consistency is checked across:
  - workspace `Cargo.toml`,
  - every crate `Cargo.toml`,
  - `CHANGELOG.md`,
  - `.release-please-manifest.json`,
  - `packaging/windows/package-manifest.json`,
  - release asset names,
  - and the release-please `extra-files` list.
- The Windows MSI includes tray, daemon, service host, CLI, reset helper, icon, changelog, license, README, and package manifest payloads.
- The WiX installer is configured to close tray, daemon, and service executable names during upgrade/uninstall.
- `installer-smoke.ps1` validates machine-wide install under `%ProgramFiles%\Boundless`, HKLM installer/uninstall evidence, shortcut targets/icons, installed executable signatures including the service host, optional upgrade-while-running behavior, uninstall cleanup, absence of Boundless processes before harness cleanup, and absence of a registered Boundless service after uninstall.
- MSI-owned packaged-payload updates are the supported update model. The MSI installer owns install, upgrade, repair, and uninstall of tray, daemon, and service payloads it installs; service and tray self-update flows are unsupported/deferred.
- The release workflow packages the Windows MSI as `Boundless-<version>-windows-x64.msi`, uploads it under the `boundless-windows-x64` workflow artifact, and publishes the same MSI name as a GitHub Release asset.
- Windows code signing remains policy-driven:
  - stable releases require signing only when `WINDOWS_SIGN_REQUIRED=true`,
  - unsigned artifacts are explicit when signing variables are not configured,
  - signing scripts never silently convert missing signing credentials into success when policy requires signing.
- Service mode has an elevated runtime smoke harness at `scripts/dev/service-smoke.ps1`, and `scripts/dev/release-readiness.ps1 -IncludeServiceSmoke` runs it as a release gate.

## Honest Limits

- MSI service-mode registration and autostart are not enabled by default; service commands remain CLI/admin-owned against the MSI-owned Program Files service payload.
- Active admin-registered service binary replacement remains explicit service-mode/update validation until the MSI owns service stop/start, rollback, registration, and removal.
- Upgrade from the last supported v4 build requires a previous MSI path passed to `installer-smoke.ps1 -PreviousInstallerPath`.
- The service must not self-update. The tray may later notify or launch an installer, but it is not the authoritative updater for this release-readiness contract.
- Lock-screen and elevated-app service behavior still require Windows runtime evidence before V5 can mark those claims validated.
- Signing validation depends on release environment variables and Windows SDK `signtool.exe` availability.

## Prior MSI Artifact Source

Use GitHub Release MSI assets as prior installer evidence. As of 2026-06-20, read-only release inspection showed:

- latest stable: `v5.0.0`, asset `Boundless-5.0.0-windows-x64.msi`, SHA-256 digest `39c4f4d9e675927f16ee8a9a1be730f6230888acbd6889d79b915eac13e1f645`.
- last v4 stable: `v4.0.2`, asset `Boundless-4.0.2-windows-x64.msi`, SHA-256 digest `7d7d0d71d2e172b57ae5363700b9c94d95b8f7dc875f1936ca7ab084f8e669bd`.

For a future stable release, N-1 MSI validation should download the previous stable release asset, keep the current release MSI from the `boundless-windows-x64` workflow artifact or local package output, and run:

```powershell
./scripts/dev/installer-smoke.ps1 `
  -InstallerPath <current-msi> `
  -PreviousInstallerPath <prior-msi> `
  -KeepArtifacts
```

If the prior MSI is unavailable in the environment, do not synthesize evidence. Record the `n_minus_1_msi_upgrade` gate as skipped with the required prior asset and command shape.

## Required Release Evidence

The release readiness packet must include:

- `scripts/release/assert-release-consistency.ps1` output,
- `scripts/dev/release-readiness.ps1` JSON and Markdown summaries,
- Windows release workflow artifact names,
- installer smoke summary JSON path,
- matching service-host version evidence from `boundless-service.exe --version`,
- signing status for each `.exe` and `.msi`,
- previous installer version used for upgrade validation or explicit skip rationale,
- `service_update_ownership=msi-owned` and `n_minus_1_msi_upgrade` gate status,
- service-mode smoke summary JSON path or deferral rationale.
