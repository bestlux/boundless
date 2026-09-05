# V5 Release Hardening

> Historical release-hardening record retained for earlier candidate evidence. Its version, sequencing, and capability statements are not current status. Use [Project Status](../project-status.md), the [Windows roadmap](../v5-roadmap.md), and [Release Readiness](release-readiness.md) for the current contract.

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
- `installer-smoke.ps1` validates machine-wide install under `%ProgramFiles%\Boundless`, HKLM installer/uninstall evidence, shortcut targets/icons, installed executable signatures including the service host, MSI-owned service registration, AutoStart, service daemon health, repair recovery after deleting `BoundlessService`, optional upgrade-while-running behavior, N-1 app and service payload replacement when a previous MSI is supplied, service stop before uninstall, uninstall cleanup, absence of Boundless processes before harness cleanup, absence of a registered Boundless service after uninstall, and removal of the Program Files service binary.
- MSI-owned packaged-payload updates are the supported update model. The MSI installer owns install, upgrade, repair, and uninstall of tray, daemon, service payloads, and `BoundlessService`; service and tray self-update flows are unsupported/deferred.
- The release workflow packages the Windows MSI as `Boundless-<version>-windows-x64.msi`, emits the SID-selecting helper as `Boundless-<version>-windows-x64-install.ps1`, uploads both under the `boundless-windows-x64` workflow artifact, and publishes both as GitHub Release assets.
- Windows code signing remains policy-driven:
  - stable releases require signing only when `WINDOWS_SIGN_REQUIRED=true`,
  - unsigned artifacts are explicit when signing variables are not configured,
  - signing scripts never silently convert missing signing credentials into success when policy requires signing.
- Service mode has an elevated runtime smoke harness at `scripts/dev/service-smoke.ps1`, and `scripts/dev/release-readiness.ps1 -IncludeServiceSmoke` runs it as a release gate.

## Honest Limits

- The MSI service path requires an explicit `BOUNDLESS_ALLOWED_USER_SID` property. A bootstrapper or installer UI that safely selects the desktop user SID remains follow-up work.
- Manual CLI service installation remains a developer fallback for unpackaged builds and requires an admin-protected service binary path.
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
- repair evidence proving MSI recovery of the service registration and daemon health,
- N-1 upgrade evidence proving both app payload and service payload replacement,
- `service_update_ownership=msi-owned` and `n_minus_1_msi_upgrade` gate status,
- service-mode smoke summary JSON path or deferral rationale.
