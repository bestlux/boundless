Boundless for Windows
=====================

This installer deploys:
- boundlesstray.exe
- boundlessd.exe
- boundless-service.exe
- boundlessctl.exe
- Boundless-Reset.ps1
- README.txt
- LICENSE.txt
- CHANGELOG.md

Recommended flow
----------------
1. Run the MSI installer from an elevated prompt with the intended desktop user's SID:

   msiexec /i Boundless-<version>-windows-x64.msi BOUNDLESS_ALLOWED_USER_SID=S-...

2. Launch Boundless from the Start Menu, desktop shortcut, or boundlesstray.exe.

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
- The MSI fails closed without BOUNDLESS_ALLOWED_USER_SID so elevation does not silently authorize the wrong Windows account.
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
