[CmdletBinding()]
param(
    [string]$Version = "0.0.0-dev",
    [string]$OutputRoot = "",
    [switch]$KeepArtifacts
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -ge 7) {
    $PSNativeCommandUseErrorActionPreference = $false
}

function Ensure-Directory {
    param([string]$Path)

    New-Item -ItemType Directory -Force -Path $Path | Out-Null
}

function Assert-PathExists {
    param(
        [string]$Path,
        [string]$Message
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        throw $Message
    }
}

function Wait-ForPathRemoval {
    param(
        [string]$Path,
        [int]$TimeoutSeconds = 10
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (-not (Test-Path -LiteralPath $Path)) {
            return
        }
        Start-Sleep -Milliseconds 200
    }

    throw "Timed out waiting for path removal: $Path"
}

function Get-ShortcutTarget {
    param([string]$ShortcutPath)

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($ShortcutPath)
    return $shortcut.TargetPath
}

if ((Get-Variable -Name IsWindows -ErrorAction SilentlyContinue) -and (-not $IsWindows)) {
    throw "package-smoke.ps1 is supported on Windows only."
}
if ((-not (Get-Variable -Name IsWindows -ErrorAction SilentlyContinue)) -and ($env:OS -ne "Windows_NT")) {
    throw "package-smoke.ps1 is supported on Windows only."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot ("artifacts\package-validation\" + (Get-Date -Format "yyyyMMdd-HHmmss"))
}
Ensure-Directory -Path $OutputRoot

Push-Location $repoRoot
try {
    & cargo build --release -p boundless-daemon -p boundless-cli -p boundless-tray
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    $packageZip = Join-Path $OutputRoot "boundless-package-smoke.zip"
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repoRoot "scripts\release\package-windows.ps1") `
        -Version $Version `
        -DaemonPath (Join-Path $repoRoot "target\release\boundlessd.exe") `
        -CliPath (Join-Path $repoRoot "target\release\boundlessctl.exe") `
        -TrayPath (Join-Path $repoRoot "target\release\boundlesstray.exe") `
        -OutputPath $packageZip
    if ($LASTEXITCODE -ne 0) {
        throw "package-windows.ps1 failed with exit code $LASTEXITCODE"
    }

    $extractRoot = Join-Path $OutputRoot "extract"
    Ensure-Directory -Path $extractRoot
    Expand-Archive -LiteralPath $packageZip -DestinationPath $extractRoot -Force

    $packageRoot = Get-ChildItem -LiteralPath $extractRoot -Directory | Select-Object -First 1
    if ($null -eq $packageRoot) {
        throw "Packaged archive did not contain a root directory."
    }

    $installRoot = Join-Path $OutputRoot "install-root"
    $startupRoot = Join-Path $OutputRoot "startup"
    $startMenuRoot = Join-Path $OutputRoot "start-menu"
    $desktopRoot = Join-Path $OutputRoot "desktop"
    $stateRoot = Join-Path $OutputRoot "state"
    $configPath = Join-Path $stateRoot "config.json"
    $dataRoot = Join-Path $stateRoot "Boundless"
    $securityRoot = Join-Path $dataRoot "security"
    $uninstallKey = "Registry::HKEY_CURRENT_USER\Software\Boundless\PackageSmoke"
    Ensure-Directory -Path $startupRoot
    Ensure-Directory -Path $startMenuRoot
    Ensure-Directory -Path $desktopRoot
    Ensure-Directory -Path $dataRoot
    Ensure-Directory -Path $securityRoot

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $packageRoot.FullName "Boundless-Install.ps1") `
        -InstallRoot $installRoot `
        -StartupFolderPath $startupRoot `
        -StartMenuProgramsPath $startMenuRoot `
        -DesktopFolderPath $desktopRoot `
        -UninstallRegistryKeyPath $uninstallKey `
        -NoLaunch
    if ($LASTEXITCODE -ne 0) {
        throw "Boundless-Install.ps1 failed with exit code $LASTEXITCODE"
    }

    Assert-PathExists -Path (Join-Path $installRoot "boundlessd.exe") -Message "Installed daemon binary is missing."
    Assert-PathExists -Path (Join-Path $installRoot "boundlessctl.exe") -Message "Installed CLI binary is missing."
    Assert-PathExists -Path (Join-Path $installRoot "boundlesstray.exe") -Message "Installed tray binary is missing."
    $shortcutPath = Join-Path $startupRoot "Boundless.lnk"
    $startMenuShortcutPath = Join-Path $startMenuRoot "Boundless.lnk"
    $desktopShortcutPath = Join-Path $desktopRoot "Boundless.lnk"
    Assert-PathExists -Path $shortcutPath -Message "Startup shortcut is missing."
    Assert-PathExists -Path $startMenuShortcutPath -Message "Start menu shortcut is missing."
    Assert-PathExists -Path $desktopShortcutPath -Message "Desktop shortcut is missing."

    $shortcutTarget = Get-ShortcutTarget -ShortcutPath $shortcutPath
    if ($shortcutTarget -ne (Join-Path $installRoot "boundlesstray.exe")) {
        throw "Startup shortcut target was unexpected: $shortcutTarget"
    }
    $startMenuShortcutTarget = Get-ShortcutTarget -ShortcutPath $startMenuShortcutPath
    if ($startMenuShortcutTarget -ne (Join-Path $installRoot "boundlesstray.exe")) {
        throw "Start menu shortcut target was unexpected: $startMenuShortcutTarget"
    }
    $desktopShortcutTarget = Get-ShortcutTarget -ShortcutPath $desktopShortcutPath
    if ($desktopShortcutTarget -ne (Join-Path $installRoot "boundlesstray.exe")) {
        throw "Desktop shortcut target was unexpected: $desktopShortcutTarget"
    }

    $uninstallItem = Get-ItemProperty -Path $uninstallKey
    if ($uninstallItem.DisplayName -ne "Boundless") {
        throw "Unexpected uninstall DisplayName: $($uninstallItem.DisplayName)"
    }

    $seedConfig = @{
        config_version = "2"
        machine_id = "package-smoke-machine"
        device_name = "PACKAGE-SMOKE"
        protocol_version = "1"
        bind = "0.0.0.0:15100"
        api_transport = "named_pipe"
        api_bind = "127.0.0.1:50051"
        api_pipe_name = "boundlessd-api"
        network_port = 15100
        layout_matrix = "self"
        peers = @(
            @{
                peer_id = "peer-a"
                machine_id = "machine-a"
                display_name = "Peer A"
                network_address = "10.0.0.2:15100"
                connected = $false
                last_error = ""
            }
        )
        features = @{
            share_clipboard = $true
            share_input = $true
        }
        hotkeys = @{}
        auto_start = $true
    }
    $seedConfig | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $configPath -Encoding utf8
    Set-Content -LiteralPath (Join-Path $securityRoot "device.secret") -Value "secret" -Encoding ascii

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installRoot "Boundless-Reset.ps1") `
        -NetworkOnly `
        -ForceLocalCleanup `
        -ConfigPath $configPath `
        -DataRoot $dataRoot `
        -SecurityRoot $securityRoot `
        -InstallRoot $installRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Boundless-Reset.ps1 -NetworkOnly failed with exit code $LASTEXITCODE"
    }

    $afterNetworkReset = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    if ($afterNetworkReset.peers.Count -ne 0) {
        throw "Network-only reset did not clear peers."
    }

    Ensure-Directory -Path $dataRoot
    Ensure-Directory -Path $securityRoot
    $seedConfig | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $configPath -Encoding utf8
    Set-Content -LiteralPath (Join-Path $securityRoot "device.secret") -Value "secret" -Encoding ascii
    Set-Content -LiteralPath (Join-Path $dataRoot "marker.txt") -Value "marker" -Encoding ascii

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installRoot "Boundless-Reset.ps1") `
        -All `
        -ForceLocalCleanup `
        -ConfigPath $configPath `
        -DataRoot $dataRoot `
        -SecurityRoot $securityRoot `
        -InstallRoot $installRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Boundless-Reset.ps1 -All failed with exit code $LASTEXITCODE"
    }

    if (Test-Path -LiteralPath $configPath) {
        throw "Full reset did not remove config path."
    }
    if (Test-Path -LiteralPath $dataRoot) {
        throw "Full reset did not remove data root."
    }

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $packageRoot.FullName "Boundless-Install.ps1") `
        -InstallRoot $installRoot `
        -StartupFolderPath $startupRoot `
        -StartMenuProgramsPath $startMenuRoot `
        -DesktopFolderPath $desktopRoot `
        -UninstallRegistryKeyPath $uninstallKey `
        -NoLaunch
    if ($LASTEXITCODE -ne 0) {
        throw "Reinstall failed with exit code $LASTEXITCODE"
    }

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installRoot "Boundless-Uninstall.ps1") `
        -InstallRoot $installRoot `
        -StartupFolderPath $startupRoot `
        -StartMenuProgramsPath $startMenuRoot `
        -DesktopFolderPath $desktopRoot `
        -UninstallRegistryKeyPath $uninstallKey `
        -RemoveState `
        -ConfigPath $configPath `
        -DataRoot $dataRoot `
        -SecurityRoot $securityRoot
    if ($LASTEXITCODE -ne 0) {
        throw "Boundless-Uninstall.ps1 failed with exit code $LASTEXITCODE"
    }

    Wait-ForPathRemoval -Path $installRoot
    if (Test-Path -LiteralPath $shortcutPath) {
        throw "Uninstall did not remove startup shortcut."
    }
    if (Test-Path -LiteralPath $startMenuShortcutPath) {
        throw "Uninstall did not remove start menu shortcut."
    }
    if (Test-Path -LiteralPath $desktopShortcutPath) {
        throw "Uninstall did not remove desktop shortcut."
    }
    if (Test-Path -LiteralPath $uninstallKey) {
        throw "Uninstall did not remove uninstall registry key."
    }

    $summary = [ordered]@{
        package_zip = $packageZip
        package_root = $packageRoot.FullName
        install_root = $installRoot
        startup_root = $startupRoot
        start_menu_root = $startMenuRoot
        desktop_root = $desktopRoot
        uninstall_key = $uninstallKey
        status = "passed"
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputRoot "package-smoke.json") -Encoding utf8
    Write-Host "package_smoke=passed"
    Write-Host "artifacts=$OutputRoot"
}
finally {
    Pop-Location
    if (-not $KeepArtifacts -and (Test-Path -LiteralPath $OutputRoot)) {
        Remove-Item -LiteralPath $OutputRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
