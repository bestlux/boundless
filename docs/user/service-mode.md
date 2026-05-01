# Service Mode

Service mode is a preview/admin workflow, not the default Boundless install path.

The per-user MSI installs `boundless-service.exe` as a payload, but it does not silently register or start a Windows service. The tray/daemon path remains the normal user experience.

## Current Boundary

- Service installation is an explicit admin action.
- `service install` is blocked by default until named-pipe ACL and privilege-boundary validation are complete.
- Elevated-app and lock-screen input control are not release-grade claims in v5.
- Administrators should install service binaries only from an admin-protected directory such as `%ProgramFiles%\Boundless`.
- Do not install a LocalSystem service from `%LocalAppData%`, `%AppData%`, `%TEMP%`, Downloads, or another user-writable location.

## Commands

Installed CLI examples use the full executable path because the MSI does not add Boundless to `PATH`:

```powershell
$BoundlessCtl = "$env:LOCALAPPDATA\Programs\Boundless\boundlessctl.exe"
& $BoundlessCtl service status
```

Development-only install attempts require the explicit unsafe flag and should be limited to a trusted local machine:

```powershell
& $BoundlessCtl service install `
  --binary "C:\Program Files\Boundless\boundless-service.exe" `
  --unsafe-allow-unreviewed-control-pipe
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

2. Stop the normal tray and daemon processes.
3. Retry `service stop` or `service uninstall` from an elevated shell.
4. Capture diagnostics:

   ```powershell
   & $BoundlessCtl diagnostics dump
   ```

Keep service reports separate from ordinary tray/daemon issues because service-mode privilege and IPC behavior is still under review.
