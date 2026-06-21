# Migration Guide

This guide covers migration from Boundless v4 and from Mouse Without Borders.

## From Boundless V4 To V5

1. Capture current state:

   ```powershell
   $BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
   & $BoundlessCtl daemon status
   & $BoundlessCtl diagnostics dump
   ```

2. Check for a legacy script install at `%LocalAppData%\Programs\Boundless`.
3. If `Boundless-Install.ps1` exists in that directory, remove the old script-based install before running the MSI. The first MSI releases intentionally block over legacy script-installed layouts.
4. Install the v5 MSI.
5. Open the tray dashboard.
6. Confirm paired peers, layout, feature toggles, hotkeys, and file-transfer settings.
7. Run the reset helper only when you intentionally want to clear local state:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File "$env:ProgramFiles\Boundless\Boundless-Reset.ps1" -NetworkOnly
   powershell -NoProfile -ExecutionPolicy Bypass -File "$env:ProgramFiles\Boundless\Boundless-Reset.ps1" -All
   ```

Release validation for upgrade-while-running requires:

```powershell
./scripts/dev/installer-smoke.ps1 `
  -InstallerPath <current-msi> `
  -PreviousInstallerPath <last-v4-msi> `
  -KeepArtifacts
```

The MSI is the authoritative updater for packaged Boundless payloads. The service does not self-update, and the tray does not own update application; any future tray notification flow must launch an MSI installer rather than replacing tray, daemon, or service payloads itself.

## From Mouse Without Borders To Boundless

Boundless does not import Mouse Without Borders keys or configuration directly.

Migration steps:

1. Install Boundless on each Windows machine.
2. Pair machines through the tray dashboard using challenge confirmation.
3. Recreate your layout in Layout Manager.
4. Enable the specific features you want, such as Easy Mouse, clipboard sharing, wrap mouse, and corner blocking.
5. Configure file receive policy explicitly. Trusted-peer auto-accept is global in v5; per-peer auto-accept remains future work.
6. Keep Mouse Without Borders installed only while comparing behavior; avoid running both tools for active input sharing at the same time.

## Capability Differences

Boundless v5 aims to exceed Mouse Without Borders in trust visibility, layout validation, diagnostics, explicit receive policy, and auditable service lifecycle controls.

Still-deferred or preview areas are tracked in [Mouse Without Borders Parity](../parity/mouse-without-borders.md). Do not treat service lock-screen/elevated-app behavior, silent firewall mutation, or public drag/drop transfer as release-grade until the parity matrix and readiness packet say they are validated.
