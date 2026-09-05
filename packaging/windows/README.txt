Boundless for Windows
=====================

Extract the release's Boundless-<version>-windows-x64.zip and double-click
Install.cmd as the intended desktop user. The complete MSI is included.
The launcher runs the matching Boundless-<version>-windows-x64-install.ps1; it captures
the intended user's SID before UAC and verifies the installed service, API, and
tray before reporting success.

Quiet installs and helpers launched from an already-elevated shell verify the
registered product, service, and API but deliberately leave tray launch to the
intended desktop user's normal session.

This installer deploys:
- boundlesstray.exe
- boundlessd.exe
- boundless-service.exe
- boundlessctl.exe
- boundless-input-injector.exe
- Boundless-Install.ps1
- Boundless-Reset.ps1
- Boundless-ConnectivityDiagnostics.ps1
- README.txt
- LICENSE.txt
- CHANGELOG.md

Recommended flow
----------------
1. Double-click Install.cmd from the extracted bundle. From the intended desktop user's normal, non-elevated PowerShell session, the equivalent command is:

   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1

   The helper captures that user's SID, asks the current session's tray to run
   its normal fail-open Quit path, and then uses one UAC prompt for a bounded
   BoundlessService stop plus MSI execution. If the tray or service does not
   stop within its safety bound, the helper fails before starting the MSI
   rather than entering a FilesInUse loop or force-killing the service.

   While UAC and MSI are active, the helper owns the tray's existing
   per-session single-instance mutex so a Start Menu relaunch cannot race the
   upgrade. The elevated phase copies the matching helper and MSI into a new
   administrator-only ProgramData staging directory, verifies both hashes,
   installs only from that immutable staging boundary, and removes it before
   reporting success.

2. Launch Boundless from the Start Menu, desktop shortcut, or boundlesstray.exe.

Elevated application input
--------------------------
- Elevated input is explicit and session-scoped. Enabling it launches only
  boundless-input-injector.exe and presents one cancellable UAC prompt; the
  tray, clipboard path, network runtime, and settings UI remain unelevated.
- Cancelling the prompt keeps normal-window input available and does not retry.
  Tray restart, injector crash, repair, and upgrade also require a new explicit
  enable action rather than producing an unsolicited UAC prompt.
- Unsigned dogfood builds display "Unknown publisher" in UAC. That is a known
  dogfood limitation, not trusted-publisher evidence. Production signing must
  be configured before this capability is presented as generally ready.
- The UAC consent or credential screen, Windows secure desktop, lock screen,
  Winlogon, other user sessions, and alternate-administrator credential flows
  are not supported. Approve UAC locally with the target computer's hardware.

Fallback/debug flow
-------------------
- If the helper cannot infer the intended desktop user safely, it fails closed.
- From an already-elevated shell, pass the intended user's SID explicitly:

  powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1 -InstallerPath .\Boundless-<version>-windows-x64.msi -AllowedUserSid S-...

  Keep using the matching helper even when the shell is already elevated. It
  owns tray quiescence, bounded service shutdown, immutable MSI staging, and
  post-install verification; invoking raw msiexec bypasses those safeguards.

- Use the helper's -UseCurrentUserWhenElevated switch only when the elevated account is intentionally the desktop user that should control Boundless.

Install behavior
----------------
- Default install root: %ProgramFiles%\Boundless
- Default service integration: registers and starts BoundlessService as LocalSystem with AutoStart, using the supplied BOUNDLESS_ALLOWED_USER_SID for the control-pipe ACL
- Upgrade shutdown ownership: the elevated helper pre-stops BoundlessService
  once and waits for Stopped; MSI ServiceControl remains an idempotent
  verification/repair contract and never races a concurrent helper stop
- Upgrade input ownership: the helper holds the intended session's existing
  tray-owner mutex from preflight through MSI completion, then releases it
  before post-install tray launch
- Elevated-input upgrade ownership: the installed injector is closed before
  payload replacement and is not automatically relaunched after repair or
  upgrade; the user explicitly enables it again when needed
- Elevated installer handoff: the helper and MSI are copied into an
  administrator-only ProgramData staging directory and hash-verified before
  the service is stopped or msiexec reopens the package
- Same-version MSI upgrades are enabled for dogfood rebuilds; post-install
  version and runtime checks still have to pass
- Default startup integration: deferred; the machine-wide MSI does not create a Startup-folder shortcut yet
- Default Start Menu entry: machine-wide Start Menu Programs shortcut for Boundless
- Default desktop entry: machine-wide desktop shortcut for Boundless
- Default uninstall entry: HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\Boundless
- Installer evidence: HKLM\Software\Boundless\Installer

State roots
-----------
- Interactive user fallback config: %LocalAppData%\Boundless\config.json
- Interactive user fallback data:   %LocalAppData%\Boundless
- Installed service state: %WINDIR%\System32\config\systemprofile\AppData\Local\Boundless
- Installed service logs:  %WINDIR%\System32\config\systemprofile\AppData\Local\Boundless\logs

Recovery
--------
- Boundless-Reset.ps1 -NetworkOnly
- Boundless-Reset.ps1 -All
- With the installed service running, -NetworkOnly uses the daemon API to clear peers; -All rotates local trust/identity, clears peers, and requires a service restart before pairing again.
- If the daemon API is unavailable and you need to remove installed service state manually, run from an elevated PowerShell: Boundless-Reset.ps1 -All -ForceLocalCleanup -IncludeServiceProfile

Connectivity diagnostics
------------------------
- Boundless-ConnectivityDiagnostics.ps1 is read-only. It checks listener/process ownership and optional remote TCP reachability for 16100/16200 plus legacy/MWB observations on 15100/15101/15200.
- TCP 16100 is the default transport port. TCP 16200 is the default nearby pairing port.
- In JSON output, firewall_hint reports read-only evidence for enabled inbound allow rules for %ProgramFiles%\Boundless\boundless-service.exe, Private profile, TCP 16100 and 16200, and LocalSubnet-or-narrower remote scope. It also flags broad/Public/Any-style matching rules. Legacy/MWB ports remain diagnostics-only.
- Example:

  powershell -NoProfile -ExecutionPolicy Bypass -File "%ProgramFiles%\Boundless\Boundless-ConnectivityDiagnostics.ps1" -RemoteHost 10.10.0.187

- Old schema 2-6 configs using default 15100 migrate to 16100 with an exact config.json.pre-v7.bak recovery copy. Custom ports remain unchanged. Nearby pairing uses network_port + 100. Disable competing MWB input sharing during Boundless qualification.
- Boundless does not silently create firewall rules. If you add rules manually, use an elevated shell only after explicit approval, restrict them to the Private profile, and scope them to %ProgramFiles%\Boundless\boundless-service.exe.
- Do not expose Boundless ports on Public networks or through router port forwarding.

Notes
-----
- The bundled helper retires recognized legacy per-user payloads and matching shortcuts/uninstall registration into a private recovery archive before MSI installation. It preserves user config/trust and stops on unknown layouts or unsafe running processes. A per-user-to-service transition requires pairing again because the service has its own identity/state.
- The MSI fails closed without BOUNDLESS_ALLOWED_USER_SID, and the helper refuses to infer a user from an already-elevated shell by default, so elevation does not silently authorize the wrong Windows account.
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
