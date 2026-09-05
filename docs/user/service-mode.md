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
- Ordinary elevated-app input is an explicit experimental capability, described below. Lock-screen and secure-desktop control are unsupported.

## User-Session Input Broker

The LocalSystem service cannot observe or inject interactive desktop input from
session 0, so mouse/keyboard sharing in service mode is brokered by the tray:

- While no broker is attached, the service truthfully reports
  `service_session_unsupported` and injects nothing.
- When the tray runs on the allowed user's normal unlocked desktop and detects a
  service-mode daemon, it attaches as the input broker over the same
  ACL-restricted control pipe. Input status then reports
  `user_session_broker` in the tray Sharing details and `boundlessctl` snapshots.
- The service remains the trust, routing, and network authority. The broker only
  captures local input in the user session and injects authenticated incoming
  frames there.
- Broker authorization is verified by the service against the actual pipe
  client identity (account SID and Windows session resolved from the pipe
  handle), never against anything the caller reports about itself.
  Administrators and SYSTEM keep pipe access for diagnostics, but broker
  attach/exchange fails closed for them, for non-interactive (session 0)
  clients, for any account other than the configured allowed user, and for
  stale/replaced broker tokens; a rejected caller cannot replace a live
  allowed-user broker. The service reverts to `service_session_unsupported`
  within a few seconds if the broker goes silent.
- The normal broker's scope is the unlocked desktop of the selected allowed user. Lock screen, secure desktop, UAC prompts, and other users' sessions are not captured or controlled. Ordinary elevated applications require the separate opt-in below.

## File operations under the selected user

The service owns machine-level startup and peer transport, but a user-selected file or export path is not an instruction to use LocalSystem filesystem permissions. The hardening implementation captures an unelevated token for the configured user's current console session and scopes filesystem access to that user. Missing, changed, or unauthorized sessions fail the user operation rather than falling back to System authority.

This is a restricted file-operation boundary inside the existing service architecture. It is not a claim that the entire daemon has moved to an unprivileged process. See the [security model](../security-trust-model.md) for remaining boundaries and the [roadmap](../v5-roadmap.md) for further runtime separation.

## Experimental elevated application input

The packaged input injector can be enabled explicitly in Sharing for ordinary administrator-launched applications belonging to the same selected split-token administrator account. It has its own limited input/release protocol and Windows elevation prompt. It does not grant arbitrary remote commands or file access.

Unsigned dogfood builds show **Unknown Publisher** in Windows and identify the feature as experimental in Boundless. Cancellation, a crash, sign-in, or automatic retry must not cause a new elevation prompt. A new explicit enable action is required. This does not support UAC consent/credential desktops, lock screen, other user sessions, or a standard user controlling a different administrator account.

Process identity and lifetime matter: a newly started broker cannot inherit a dead process's uncertain input delivery. Boundless must release conservatively and require a fresh handoff before continuing. The exact contract and tests are in [User-session input broker](../architecture/user-session-input-broker.md). Installed Windows validation remains necessary before a public elevated-input claim.

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
