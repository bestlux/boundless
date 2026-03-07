Boundless for Windows
=====================

This package contains:
- boundlesstray.exe
- boundlessd.exe
- boundlessctl.exe
- Boundless-Install.ps1
- Boundless-Uninstall.ps1
- Boundless-Reset.ps1

Recommended flow
----------------
1. Extract this archive to a writable folder.
2. Run Boundless-Install.ps1 from PowerShell.
3. Launch Boundless from the Startup shortcut or run boundlesstray.exe.

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
- Boundless-Uninstall.ps1 -RemoveState

Notes
-----
- The tray and CLI default to the local named-pipe API endpoint.
- If your daemon is configured for TCP, launch the tray or CLI with an explicit endpoint.
