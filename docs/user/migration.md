# Migration Guide

This guide covers peer compatibility, older Boundless installations, and moving from Mouse Without Borders.

## Peer protocol upgrade

The Windows hardening work adds protocol **4.5.0** for temporary, consented paired-test probes. It is incompatible with protocol 4.4.0 peers under Boundless' exact-version transport policy. Update both PCs to the same build before testing. Configuration schema **7** preserves durable pairing and preferences while dropping saved connection observations. Live connection state is rediscovered after startup; a remembered pairing is not evidence that its peer is currently connected.

Schema 2–6 configs using the old default transport port **15100** migrate to **16100**, with nearby pairing on **16200**. Saved manual peer endpoints using the old default also migrate. Other ports stay unchanged; an explicit 15100 in schema 7 stays unchanged. Older schemas cannot distinguish a deliberately chosen 15100 from the old default, so both migrate once. Before rewriting a supported old config, Boundless creates `config.json.pre-v7.bak` beside it from its exact original bytes; an existing backup is not overwritten. Invalid configs or a failed backup stop migration before the active file is rewritten.

Older binaries do not understand the new protocol/configuration contract. Before an installed upgrade, preserve a private backup of the existing configuration and identity/trust state for rollback. Do not post that backup in an issue. Rolling back requires the compatible pre-upgrade state on both PCs or a build that understands the newer schema; do not edit the version fields to force an old binary to accept it.

Check the release's preview status and [Project Status](../project-status.md) for the implementation and validation boundary.

## From Boundless V4 To V5

1. Capture current state:

   ```powershell
   $BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
   & $BoundlessCtl daemon status
   & $BoundlessCtl diagnostics dump
   ```

2. Check for older per-user or manual service state:

   ```powershell
   Test-Path "$env:LocalAppData\Programs\Boundless"
   Get-Service BoundlessService -ErrorAction SilentlyContinue
   ```

3. The bundled helper recognizes supported per-user script installs under `%LocalAppData%\Programs\Boundless`, verifies their marker/manifest/owner, then moves the old payload into a private recovery archive before the MSI. It retires only matching old shortcuts and the user's matching uninstall entry. It does not run the old uninstall script or delete configuration, identity, or trust. Unrecognized layouts, reparse points, or an old process that cannot stop safely block migration with an explanation. Keep the printed recovery location until qualification finishes.
4. If `BoundlessService` was installed manually from a copied service binary, uninstall that manual service from an elevated shell before using the MSI-owned service path. The v5 MSI is the owner of future service registration, repair, upgrade, and uninstall.
5. Extract the Windows release ZIP and double-click **Install.cmd** as the intended desktop user. The equivalent command is:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1
   ```

   The helper captures that user's SID before UAC. If you must install from an
   already-elevated shell, pass the intended SID explicitly:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1 -AllowedUserSid S-...
   ```

6. Open the tray dashboard.
7. Confirm paired peers, layout, feature toggles, hotkeys, and file-transfer settings.

   The machine service owns its state under `%WINDIR%\System32\config\systemprofile\AppData\Local\Boundless`; a former per-user daemon used `%LocalAppData%\Boundless`. The installer preserves that older user state but does not copy its identity into the service. Pair again and reapply preferences after that transition. An ordinary MSI-to-MSI upgrade retains the existing service state.
8. Run the reset helper only when you intentionally want to clear local state:

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

Release validation for full-service MSI ownership also requires repair evidence and uninstall cleanup evidence. `installer-smoke.ps1` deletes the service registration, runs MSI repair, verifies service and daemon recovery, then uninstalls and fails if the service registration or Program Files service binary remains.

The MSI is the authoritative updater for packaged Boundless payloads and the `BoundlessService` lifecycle. The service does not self-update, and the tray does not own update application; any future tray notification flow must launch an MSI installer rather than replacing tray, daemon, or service payloads itself.

## From Mouse Without Borders To Boundless

Boundless does not import Mouse Without Borders keys or configuration directly.

Migration steps:

1. Install Boundless on each Windows machine.
2. Pair machines through the tray dashboard using challenge confirmation.
3. Recreate your layout in Arrange PCs.
4. Enable the specific features you want, such as Easy Mouse, clipboard sharing, wrap mouse, and corner blocking.
5. Configure file receive policy explicitly. Trusted-peer auto-accept is global in v5; per-peer auto-accept remains future work.
6. Keep Mouse Without Borders installed only while comparing behavior; avoid running both tools for active input sharing at the same time.

## Capability Differences

Boundless v5 aims to exceed Mouse Without Borders in trust visibility, layout validation, diagnostics, explicit receive policy, and auditable service lifecycle controls.

Still-deferred or preview areas are tracked in [Mouse Without Borders Parity](../parity/mouse-without-borders.md). Do not treat service lock-screen/elevated-app behavior, silent firewall mutation, or public drag/drop transfer as release-grade until the parity matrix and readiness packet say they are validated.
