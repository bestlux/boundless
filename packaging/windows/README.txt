Boundless for Windows
=====================

This installer deploys:
- boundlesstray.exe
- boundlessd.exe
- boundless-service.exe
- boundlessctl.exe
- Boundless-Install.ps1
- Boundless-Reset.ps1
- Boundless-ConnectivityDiagnostics.ps1
- README.txt
- LICENSE.txt
- CHANGELOG.md

Recommended flow
----------------
1. From the intended desktop user's normal, non-elevated PowerShell session, run the install helper that ships beside the MSI:

   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1

   The helper captures that user's SID before the UAC prompt and passes it to the elevated MSI.

2. Launch Boundless from the Start Menu, desktop shortcut, or boundlesstray.exe.

Fallback/debug flow
-------------------
- If the helper cannot infer the intended desktop user safely, it fails closed.
- From an already-elevated shell, pass the intended user's SID explicitly:

  msiexec /i Boundless-<version>-windows-x64.msi BOUNDLESS_ALLOWED_USER_SID=S-...

- Use the helper's -UseCurrentUserWhenElevated switch only when the elevated account is intentionally the desktop user that should control Boundless.

Install behavior
----------------
- Default install root: %ProgramFiles%\Boundless
- Default service integration: registers and starts BoundlessService as LocalSystem with AutoStart, using the supplied BOUNDLESS_ALLOWED_USER_SID for the control-pipe ACL
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
- Boundless-ConnectivityDiagnostics.ps1 is read-only. It checks local listener/process ownership for TCP 15100, 15101, and 15200 and, when a remote host is supplied, remote TCP reachability for the same ports.
- TCP 15100 is the default Boundless transport port. TCP 15200 is the default nearby pairing port. TCP 15101 is included to diagnose Mouse Without Borders / PowerToys side-by-side listener ownership during dogfood.
- In JSON output, firewall_hint reports read-only evidence for the expected policy shape: enabled inbound allow rules for %ProgramFiles%\Boundless\boundless-service.exe, Private profile, TCP 15100 and 15200, and LocalSubnet-or-narrower remote scope. It also flags broad/Public/Any-style matching rules. TCP 15101 remains diagnostics-only, not a default firewall requirement.
- Example:

  powershell -NoProfile -ExecutionPolicy Bypass -File "%ProgramFiles%\Boundless\Boundless-ConnectivityDiagnostics.ps1" -RemoteHost 10.10.0.187

- If Mouse Without Borders or another process owns required Boundless TCP 15100 or 15200 during side-by-side dogfood, configure the same alternate Boundless network_port on every participating machine before pairing. Nearby pairing uses network_port + 100, so network_port 16100 pairs on TCP 16200. Mouse Without Borders on diagnostics-only TCP 15101 is evidence to record, not a Boundless pairing or transport collision by itself.
- Boundless does not silently create firewall rules. If you add rules manually, use an elevated shell only after explicit approval, restrict them to the Private profile, and scope them to %ProgramFiles%\Boundless\boundless-service.exe.
- Do not expose Boundless ports on Public networks or through router port forwarding.

Notes
-----
- The MSI blocks over an existing legacy script-installed Boundless layout. Remove the old script-based install first, then rerun the installer.
- The MSI fails closed without BOUNDLESS_ALLOWED_USER_SID, and the helper refuses to infer a user from an already-elevated shell by default, so elevation does not silently authorize the wrong Windows account.
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
