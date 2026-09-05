# Set up two Windows PCs

Install the same Boundless build on both PCs, open the tray dashboard, pair them, and arrange their positions. Boundless is currently a development preview; start with two PCs on a trusted local network.

## Install

1. Download the Windows MSI and matching `Boundless-<version>-windows-x64-install.ps1` helper from the same release into one folder.
2. In the intended desktop user's normal PowerShell session, run:

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\Boundless-<version>-windows-x64-install.ps1
   ```

   The helper identifies the desktop user before the Windows elevation prompt. The current installer still requires this helper; simply double-clicking the MSI without its required user property is not the supported path. See [Service Mode](service-mode.md) for the administrative fallback.

3. Launch Boundless from the Start Menu or desktop shortcut. Choose **Dashboard** from its tray icon if the window is hidden.
4. Check **Home**. If the background runtime is unavailable, open **Support** before attempting pairing.

The installer puts binaries in `%ProgramFiles%\Boundless` and installs `BoundlessService`. Keep one service-owned runtime; starting a second per-user daemon is not a repair step. The MSI does not add the CLI to `PATH`.

## Pair

On both PCs, open Boundless. From **Home → Add a PC**, choose the other PC from discovery or enter its host address. Follow the code-entry flow shown by Boundless and verify the intended PC before completing it. Pairing establishes trust; it does not imply the peer is online or ready to accept input.

Once Home shows the paired PC as connected, open **Arrange PCs**. An offline paired PC keeps its trust and reconnects when available. Do not forget or reset it merely because it is asleep.

## Arrange and use

Arrange the PCs to match their physical positions, keep **This PC** in the layout exactly once, and apply the layout. Adjacent edges determine where input moves. In **Sharing**, enable input sharing and Easy Mouse as needed, then move across the corresponding screen edge.

To return control locally, press **Ctrl twice on the local keyboard**. In the normal installed service + tray-broker mode, **Pause input** on Home releases the local capture hook immediately and requests input sharing to stop; that local release does not wait for a working daemon connection. A standalone developer daemon owns its own hooks, so its pause requires daemon acknowledgment. Resume deliberately when ready.

The normal supported desktop is the selected user's unlocked Windows session. An administrator-launched application has additional input restrictions; see [Service Mode](service-mode.md). UAC prompts and the lock screen are outside the supported control surface.

## Clipboard and files

Enable clipboard sharing in **Sharing**. Use **Files** for explicit sends and transfer progress. Receiving files requires a receive folder and an explicit receive-policy opt-in. See [Clipboard and File Workflows](clipboard-file-workflows.md) before enabling it.

## Get help

**Support** shows versions, connection help, and redacted report export. It also exposes the temporary permission used for [paired testing](../performance/paired-testing.md). No report is sent automatically.

CLI fallback:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl --json daemon status
& $BoundlessCtl diagnostics dump --open-folder
```

Use `diagnostics dump --offline --open-folder` if the background runtime cannot be reached. See [Troubleshooting](troubleshooting.md) for the next check.

## Exit or uninstall

Closing the dashboard hides it to the tray. Use the tray's **Quit** action to exit the interactive app. The installed service has its own lifetime.

Uninstall through Windows Installed Apps. Ordinary uninstall preserves configuration and trust; use the reset workflow only when you intend to remove that state. The [migration guide](migration.md) explains upgrades and older installation layouts.
