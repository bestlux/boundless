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
1. Run the MSI installer from an elevated prompt or through Windows elevation.
2. Launch Boundless from the Start Menu, desktop shortcut, or boundlesstray.exe.

Install behavior
----------------
- Default install root: %ProgramFiles%\Boundless
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
- The MSI installs boundless-service.exe as a payload under Program Files, but it does not register or start BoundlessService yet.
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
