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

`install` defaults to a `boundless-service.exe` next to the running `boundlessctl.exe`, but it refuses user-writable source locations such as `%LocalAppData%`, `%AppData%`, and `%TEMP%`. Administrators must install from an admin-protected location such as `%ProgramFiles%\Boundless` until the installer owns a reviewed elevated/service option.

Normal service installation is blocked until Boundless implements an explicit service named-pipe ACL. A deliberately named `--unsafe-allow-unreviewed-control-pipe` override exists only for local development on a trusted machine.

These commands require Windows and administrative permission where SCM requires it. Non-Windows builds report service mode as unsupported.

## Security Boundary

The service-mode control boundary is not complete until Windows named-pipe ACLs are explicit and tested.

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

The Windows packaging manifest, WiX payload, package script, and release signing list include `boundless-service.exe`. The service is not silently installed by the per-user MSI; service installation remains an explicit admin action until the installer owns a reviewed elevated/service option.

Installer smoke still needs to assert the service payload and signature explicitly. Until then, packaging is wired but not runtime-validated.

## Current Validation Evidence

Milestone V5-3 added:

- a service host binary using the Windows Service Control Manager entrypoint,
- CLI status/start/stop/uninstall commands plus an install command that is blocked by default until explicit pipe ACLs land,
- release packaging inputs for the service binary,
- release signing coverage for the service binary,
- source-path rejection so a LocalSystem service is not registered from a user-writable per-user install directory,
- compile-time validation of the service host and CLI surface.

Runtime validation still needs an elevated Windows service install/start/stop/uninstall pass before the matrix row can move beyond `cli-ready`.
