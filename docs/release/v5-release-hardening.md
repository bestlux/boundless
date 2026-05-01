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
- `installer-smoke.ps1` validates install, shortcut targets/icons, installed executable signatures including the service host, uninstall registry entry, optional upgrade-while-running behavior, uninstall cleanup, absence of Boundless processes before harness cleanup, and absence of a registered Boundless service after uninstall.
- Windows code signing remains policy-driven:
  - stable releases require signing only when `WINDOWS_SIGN_REQUIRED=true`,
  - unsigned artifacts are explicit when signing variables are not configured,
  - signing scripts never silently convert missing signing credentials into success when policy requires signing.

## Honest Limits

- MSI service-mode installation is not enabled by default; service commands remain CLI/admin-owned until IPC ACL and service privilege-boundary validation are complete.
- Upgrade from the last supported v4 build requires a previous MSI path passed to `installer-smoke.ps1 -PreviousInstallerPath`.
- Lock-screen and elevated-app service behavior still require Windows runtime evidence before V5 can mark those claims validated.
- Signing validation depends on release environment variables and Windows SDK `signtool.exe` availability.

## Required Release Evidence

The v5 readiness packet must include:

- `scripts/release/assert-release-consistency.ps1` output,
- `scripts/dev/v5-readiness.ps1` JSON and Markdown summaries,
- Windows release workflow artifact names,
- installer smoke summary JSON path,
- signing status for each `.exe` and `.msi`,
- previous installer version used for upgrade validation or explicit skip rationale,
- service-mode validation status or deferral rationale.
