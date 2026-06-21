# Boundless V5 Quickstart

This guide covers the normal Windows tray flow for installing, pairing, arranging, using, and uninstalling Boundless.

## Install

1. Download the Windows MSI and matching `Boundless-<version>-windows-x64-install.ps1` helper into the same folder.
2. From the intended desktop user's normal, non-elevated PowerShell session, run:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1
   ```

   The helper captures that user's SID before the UAC prompt and passes it to the elevated MSI. If you are already in an elevated shell, use the fallback in [Service Mode](service-mode.md) and pass the intended SID explicitly.

3. Launch Boundless from the Start Menu, Desktop shortcut, or `boundlesstray.exe`.
4. Open the tray icon and choose `Dashboard`.
5. Confirm the daemon is reachable on the Status & Pairing tab.

The default machine-wide install root is `%ProgramFiles%\Boundless`. The tray is the primary entrypoint and starts `boundlessd` when needed.

Installed CLI examples use the full executable path because the MSI does not add Boundless to `PATH`:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
```

## Pair Two Machines

On both machines, open the tray dashboard.

1. On the machine you want to connect from, use Status & Pairing to choose a discovered peer or manual host.
2. Start the guided pairing request.
3. On the target machine, compare the displayed 6-digit code.
4. Approve only if the code and machine identity match what you expect.
5. Return to Status & Pairing and confirm the peer is trusted and connected.

CLI fallback:

```powershell
& $BoundlessCtl pair discover
& $BoundlessCtl pair request <index|machine_id|display-name>
& $BoundlessCtl pair pending
& $BoundlessCtl pair approve <request_id>
```

## Arrange Devices

Use Layout Manager in the tray dashboard.

1. Drag devices onto the grid.
2. Keep `This PC` in the layout exactly once.
3. Use only connected cardinal neighbors for edge switching.
4. Apply the layout.

CLI fallback:

```powershell
& $BoundlessCtl layout set "left,self,right"
& $BoundlessCtl layout preview
```

## Use Input Sharing

Enable Easy Mouse in Settings. Move across a configured screen edge to switch capture target. Use the release/escape hotkey or Settings controls to return control locally if input ownership gets stuck.

Boundless supports hook capture with polling fallback. The tray reports capture mode as hook, polling fallback, unsupported, disabled, or unavailable where the daemon can determine it.

## Clipboard And Files

Enable clipboard sharing in Settings. Text and bitmap clipboard payloads are supported through the clipboard workflow. File transfer has explicit receive-folder and receive-policy controls and defaults to conservative receive behavior.

See [Clipboard And File Workflows](clipboard-file-workflows.md).

## Troubleshooting

Start with:

```powershell
& $BoundlessCtl daemon status
& $BoundlessCtl diagnostics dump
```

Then use [Troubleshooting](troubleshooting.md) for discovery, firewall, stale daemon, named pipe, service, input capture, clipboard, and file transfer cases.

## Uninstall

Use Windows Apps & Features or:

```powershell
Start-Process msiexec.exe -Wait -ArgumentList @('/x', '<path-to-boundless-msi>', '/qn', '/norestart')
```

The MSI uninstall removes the machine-wide install root, shortcuts, and uninstall registry entry. User config and trust state are not silently destroyed by ordinary uninstall; use the packaged reset helper when you intentionally want to reset local state.
