# Boundless V5 Service Mode Contract

This document defines the v5 service-mode boundary behind the Mouse Without Borders parity matrix.

## Current Service Shape

V5 introduces a dedicated Windows service host binary, `boundless-service.exe`. It reuses the normal daemon runtime and serves the control plane through the configured named pipe.

The normal per-user daemon remains `boundlessd.exe`. The service host exists so installation, start, stop, and uninstall are explicit operations instead of pretending a console daemon can be registered directly with the Windows Service Control Manager.

## CLI Lifecycle Surface

`boundlessctl service` owns the administrative lifecycle:

```powershell
boundlessctl service status
boundlessctl service install --binary C:\Path\To\boundless-service.exe
boundlessctl service install --auto-start
boundlessctl service start
boundlessctl service stop
boundlessctl service uninstall
```

`install` defaults to a `boundless-service.exe` next to the running `boundlessctl.exe`, but it refuses user-writable source locations such as `%LocalAppData%`, `%AppData%`, `%TEMP%`, and Downloads. The machine-wide MSI is the primary path: it lays down the admin-protected payload under `%ProgramFiles%\Boundless`, registers `BoundlessService`, and sets AutoStart using an explicit `BOUNDLESS_ALLOWED_USER_SID`.

During manual CLI fallback installation, `boundlessctl` resolves the installing Windows user's SID and passes it to the service. During MSI installation, the secure `BOUNDLESS_ALLOWED_USER_SID` property supplies the selected desktop user SID. In both paths, the service validates the SID shape before reporting `Running` and hosts the control pipe with an explicit ACL for `SYSTEM`, local Administrators, and that selected user.

These commands require Windows and administrative permission where SCM requires it. Non-Windows builds report service mode as unsupported.

## Security Boundary

The service-mode control boundary now has explicit Windows named-pipe ACLs for the intended local principals.

V5 must not claim a hard local privilege boundary until release evidence proves:

- a non-authorized local user cannot invoke privileged daemon actions,
- per-user tray and CLI clients can still reach the intended service endpoint,
- localhost TCP fallback shows a warning and is not silently treated as equivalent to named-pipe service control,
- service-to-user interactions are documented and do not imply remote administration.

## Elevated And Lock-Screen Behavior

`boundless-service.exe` is a service entrypoint, not proof of elevated-app or lock-screen input control by itself. Windows session isolation may require a service-to-user agent model before these scenarios can work.

The v5 readiness packet must classify elevated-app and lock-screen behavior as:

- `validated`, with exact environment and command evidence, or
- `not implemented`, with the OS/session boundary that blocks it and the public claim removed.

## Packaging

The Windows packaging manifest, WiX payload, package script, and release signing list include `boundless-service.exe`. The machine-wide MSI owns `BoundlessService` registration, AutoStart, stop/start lifecycle, and uninstall removal from the MSI-owned Program Files payload.

Installer smoke asserts the service payload/signature, service-host version output, registered service path, AutoStart config, selected SID argument, daemon health, and that uninstall leaves no registered Boundless service when run from an elevated Windows shell. The separate service smoke harness remains a manual/developer fallback for the CLI service path.

## Current Validation Evidence

Milestone V5-3 added:

- a service host binary using the Windows Service Control Manager entrypoint,
- CLI status/start/stop/uninstall commands plus an install command that rejects user-writable service binary paths,
- explicit named-pipe ACL construction for the normal daemon and service daemon,
- service installation that binds the control pipe to `SYSTEM`, Administrators, and the installing user SID,
- an elevated service smoke harness for install/start/status/daemon-health/stop/uninstall evidence,
- release packaging inputs for the service binary,
- release signing coverage for the service binary,
- source-path rejection so a LocalSystem service is not registered from a user-writable per-user install directory,
- compile-time validation of the service host and CLI surface.

Runtime validation still needs elevated Windows install/repair/N-1 evidence before release signoff. Elevated-app and lock-screen behavior remain separately gated by Windows runtime evidence.
