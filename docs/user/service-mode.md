# Service Mode

Service mode runs the Boundless daemon as the Windows `BoundlessService`. The
machine-wide MSI is the primary service installation path.

## Current Boundary

- Service installation is owned by the elevated machine-wide MSI.
- The MSI installs the service binary under `%ProgramFiles%\Boundless`, registers
  `BoundlessService` as LocalSystem, sets AutoStart, starts the service during
  install, stops it for upgrade/uninstall, and removes it on uninstall.
- The preferred install helper captures the intended desktop user's SID before
  UAC and supplies the secure `BOUNDLESS_ALLOWED_USER_SID=S-...` MSI property.
  The MSI still requires that property and fails closed instead of guessing the
  elevating administrator account.
- `service install` rejects service binaries under user-writable locations such as `%LocalAppData%`, `%AppData%`, `%TEMP%`, and Downloads.
- The service control named pipe uses an explicit ACL for `SYSTEM`, local Administrators, and the selected Windows user SID.
- Service mode has separate LocalSystem runtime state from the normal per-user daemon. Pairing, layout, and feature settings should be configured while the service is the active daemon.
- The service does not self-update, and tray-owned update application is unsupported/deferred.
- Elevated-app and lock-screen input control still need Windows runtime evidence before they are release-grade claims in v5.

## Commands

Install from the intended desktop user's normal, non-elevated PowerShell session:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1
```

Fallback/debug path from an elevated prompt with the intended desktop user's SID:

```powershell
msiexec /i .\Boundless-<version>-windows-x64.msi `
  BOUNDLESS_ALLOWED_USER_SID=S-...
```

Do not run the helper from an already-elevated shell and accept its current user
by default. It refuses that path unless you pass `-AllowedUserSid`,
`-AllowedUserName`, or `-UseCurrentUserWhenElevated` explicitly.

Installed CLI examples use the full executable path because the MSI does not add Boundless to `PATH`:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl service status
```

Manual CLI service installation remains a developer fallback for unpackaged
builds. Copy the service binary to an admin-protected directory first, then run
the install from an elevated PowerShell session:

```powershell
New-Item -ItemType Directory -Force -Path "C:\Program Files\Boundless" | Out-Null
Copy-Item ".\target\release\boundless-service.exe" "C:\Program Files\Boundless\boundless-service.exe" -Force
& $BoundlessCtl service install `
  --binary "C:\Program Files\Boundless\boundless-service.exe"
```

Manual fallback start, stop, and uninstall:

```powershell
& $BoundlessCtl service start
& $BoundlessCtl service stop
& $BoundlessCtl service uninstall
```

## Recovery

If service startup or uninstall fails:

1. Check service status:

   ```powershell
   & $BoundlessCtl service status
   ```

2. Stop the normal tray and daemon processes so only one daemon owns `npipe://./pipe/boundlessd-api`.
3. Retry `service stop` or `service uninstall` from an elevated shell.
4. Capture diagnostics:

   ```powershell
   & $BoundlessCtl diagnostics dump
   ```

Keep service reports separate from ordinary tray/daemon issues because service mode uses a different Windows account, config root, and named-pipe ACL from the per-user daemon.
