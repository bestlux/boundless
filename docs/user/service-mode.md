# Service Mode

Service mode is an explicit administrator workflow for running the Boundless daemon as `BoundlessService`.

The per-user MSI installs `boundless-service.exe` as a payload, but it does not silently register or start a Windows service. The tray/daemon path remains the normal install path unless an administrator opts into service mode.

## Current Boundary

- Service installation is an explicit admin action.
- `service install` rejects service binaries under user-writable locations such as `%LocalAppData%`, `%AppData%`, `%TEMP%`, and Downloads.
- The service control named pipe uses an explicit ACL for `SYSTEM`, local Administrators, and the Windows user SID that installed the service.
- Service mode has separate LocalSystem runtime state from the normal per-user daemon. Pairing, layout, and feature settings should be configured while the service is the active daemon.
- BND-NEXT-9A readiness proves MSI ownership of packaged payload updates and N-1 MSI upgrade evidence. It does not prove replacement of an active admin-registered service binary copied to `C:\Program Files\Boundless`; that remains explicit service-mode/update validation unless the service is registered against an MSI-owned install location.
- The service does not self-update, and tray-owned update application is unsupported/deferred.
- Elevated-app and lock-screen input control still need Windows runtime evidence before they are release-grade claims in v5.

## Commands

Installed CLI examples use the full executable path because the MSI does not add Boundless to `PATH`:

```powershell
$BoundlessCtl = "$env:LOCALAPPDATA\Programs\Boundless\boundlessctl.exe"
& $BoundlessCtl service status
```

Copy the service binary to an admin-protected directory before installation, then run the install from an elevated PowerShell session:

```powershell
New-Item -ItemType Directory -Force -Path "C:\Program Files\Boundless" | Out-Null
Copy-Item "$env:LOCALAPPDATA\Programs\Boundless\boundless-service.exe" "C:\Program Files\Boundless\boundless-service.exe" -Force
& $BoundlessCtl service install `
  --binary "C:\Program Files\Boundless\boundless-service.exe"
```

Start, stop, and uninstall:

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
