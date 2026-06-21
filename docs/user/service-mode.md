# Service Mode

Service mode is an explicit administrator workflow for running the Boundless daemon as `BoundlessService`.

The machine-wide MSI installs `boundless-service.exe` as a payload under `%ProgramFiles%\Boundless`, but it does not silently register or start a Windows service. The tray/daemon path remains the normal install path unless an administrator opts into service mode.

## Current Boundary

- Service installation is an explicit admin action.
- `service install` rejects service binaries under user-writable locations such as `%LocalAppData%`, `%AppData%`, `%TEMP%`, and Downloads.
- The service control named pipe uses an explicit ACL for `SYSTEM`, local Administrators, and the Windows user SID that installed the service.
- Service mode has separate LocalSystem runtime state from the normal per-user daemon. Pairing, layout, and feature settings should be configured while the service is the active daemon.
- BND-NEXT-9B-2 puts the packaged service binary under `%ProgramFiles%\Boundless`, but it does not prove replacement of an active admin-registered service. SCM registration, autostart, stop/start during upgrade, and rollback behavior remain explicit BND-NEXT-9B-3 work.
- The service does not self-update, and tray-owned update application is unsupported/deferred.
- Elevated-app and lock-screen input control still need Windows runtime evidence before they are release-grade claims in v5.

## Commands

Installed CLI examples use the full executable path because the MSI does not add Boundless to `PATH`:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl service status
```

Run service installation from an elevated PowerShell session and point it at the MSI-owned service payload:

```powershell
& $BoundlessCtl service install `
  --binary "$env:ProgramFiles\Boundless\boundless-service.exe"
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
