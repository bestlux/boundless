Boundless for Windows
=====================

This installer deploys:
- boundlesstray.exe
- boundlessd.exe
- boundless-service.exe
- boundlessctl.exe
- Boundless-Install.ps1
- Boundless-Reset.ps1
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
- Config: %LocalAppData%\Boundless\config.json
- Data:   %LocalAppData%\Boundless
- Logs:   %LocalAppData%\Boundless\logs
- Security: %LocalAppData%\Boundless\security

Recovery
--------
- Boundless-Reset.ps1 -NetworkOnly
- Boundless-Reset.ps1 -All

Notes
-----
- The MSI blocks over an existing legacy script-installed Boundless layout. Remove the old script-based install first, then rerun the installer.
- The MSI fails closed without BOUNDLESS_ALLOWED_USER_SID, and the helper refuses to infer a user from an already-elevated shell by default, so elevation does not silently authorize the wrong Windows account.
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
