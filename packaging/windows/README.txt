Boundless for Windows
=====================

This installer deploys:
- boundlesstray.exe
- boundlessd.exe
- boundlessctl.exe
- Boundless-Reset.ps1
- README.txt
- LICENSE.txt
- CHANGELOG.md

Recommended flow
----------------
1. Run the MSI installer.
2. Launch Boundless from the Startup shortcut or run boundlesstray.exe.

Install behavior
----------------
- Default install root: %LocalAppData%\Programs\Boundless
- Default startup integration: user Startup-folder shortcut for boundlesstray.exe
- Default Start Menu entry: user Start Menu Programs shortcut for Boundless
- Default desktop entry: user Desktop shortcut for Boundless
- Default uninstall entry: HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Boundless

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
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
