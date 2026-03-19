[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallerPath = "",
    [string]$PreviousInstallerPath = "",
    [string]$OutputRoot = "",
    [switch]$RequireSignature,
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
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if (-not (Test-Path -LiteralPath $Path)) {
            return
        }

        Start-Sleep -Milliseconds 250
    }

    throw "Timed out waiting for path removal: $Path"
}

function Invoke-MsiExec {
    param(
        [string[]]$ArgumentList,
        [string]$LogPath
    )

    $arguments = @($ArgumentList)
    if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
        $arguments += @("/l*v", $LogPath)
    }

    $process = Start-Process -FilePath "msiexec.exe" -ArgumentList $arguments -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "msiexec.exe failed with exit code $($process.ExitCode). Log: $LogPath"
    }
}

function Get-ShortcutTarget {
    param([string]$ShortcutPath)

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($ShortcutPath)
    return $shortcut.TargetPath
}

function Get-ShortcutIconLocation {
    param([string]$ShortcutPath)

    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($ShortcutPath)
    return $shortcut.IconLocation
}

function Test-ExpectedShortcutIconLocation {
    param(
        [string]$IconLocation,
        [string]$InstalledIconPath
    )

    if ([string]::IsNullOrWhiteSpace($IconLocation)) {
        return $false
    }

    $resolvedLocation = $IconLocation.Split(',')[0].Trim()
    if ([string]::IsNullOrWhiteSpace($resolvedLocation)) {
        return $false
    }

    if ($resolvedLocation -ieq $InstalledIconPath) {
        return $true
    }

    return $resolvedLocation -imatch '[\\/]Microsoft[\\/]Installer[\\/]\{[^\\/]+\}[\\/]BoundlessIcon\.ico$'
}

function Assert-Authenticode {
    param(
        [string]$Path,
        [bool]$Required
    )

    $signature = Get-AuthenticodeSignature -FilePath $Path
    if ($Required -and $signature.Status -ne "Valid") {
        throw "Authenticode signature was expected to be valid for $Path but was $($signature.Status)."
    }

    return $signature.Status
}

function Get-ExpectedDisplayVersion {
    param([string]$Path)

    $name = [System.IO.Path]::GetFileNameWithoutExtension($Path)
    if ($name -match '^(?:Boundless|boundless)-(?<version>\d+\.\d+\.\d+)') {
        return $Matches.version
    }

    return $null
}

function Get-UninstallEntry {
    $keys = @(
        "Registry::HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall\*",
        "Registry::HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Uninstall\*"
    )

    foreach ($key in $keys) {
        $entry = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue |
            Where-Object { $_.DisplayName -eq "Boundless" } |
            Select-Object -First 1
        if ($null -ne $entry) {
            return $entry
        }
    }

    return $null
}

if ((Get-Variable -Name IsWindows -ErrorAction SilentlyContinue) -and (-not $IsWindows)) {
    throw "installer-smoke.ps1 is supported on Windows only."
}
if ((-not (Get-Variable -Name IsWindows -ErrorAction SilentlyContinue)) -and ($env:OS -ne "Windows_NT")) {
    throw "installer-smoke.ps1 is supported on Windows only."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot ("artifacts\installer-validation\" + (Get-Date -Format "yyyyMMdd-HHmmss"))
}
Ensure-Directory -Path $OutputRoot

if ([string]::IsNullOrWhiteSpace($InstallerPath)) {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        throw "Provide either -InstallerPath or -Version."
    }

    Push-Location $repoRoot
    try {
        & cargo build --release -p boundless-daemon -p boundless-cli -p boundless-tray
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed with exit code $LASTEXITCODE"
        }

        $InstallerPath = Join-Path $OutputRoot ("Boundless-$Version-windows-x64.msi")
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repoRoot "scripts\release\package-windows.ps1") `
            -Version $Version `
            -DaemonPath (Join-Path $repoRoot "target\release\boundlessd.exe") `
            -CliPath (Join-Path $repoRoot "target\release\boundlessctl.exe") `
            -TrayPath (Join-Path $repoRoot "target\release\boundlesstray.exe") `
            -OutputPath $InstallerPath
        if ($LASTEXITCODE -ne 0) {
            throw "package-windows.ps1 failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$InstallerPath = (Resolve-Path -LiteralPath $InstallerPath).Path
Assert-PathExists -Path $InstallerPath -Message "Installer was not found."
$installerSignature = Assert-Authenticode -Path $InstallerPath -Required:$RequireSignature.IsPresent
$expectedDisplayVersion = Get-ExpectedDisplayVersion -Path $InstallerPath

$installLog = Join-Path $OutputRoot "install.log"
$upgradeLog = Join-Path $OutputRoot "upgrade.log"
$uninstallLog = Join-Path $OutputRoot "uninstall.log"

$startupShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::Startup)) "Boundless.lnk"
$startMenuShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::Programs)) "Boundless.lnk"
$desktopShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)) "Boundless.lnk"
$installRoot = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)) "Programs\Boundless"
$resetScriptPath = Join-Path $installRoot "Boundless-Reset.ps1"
$iconPath = Join-Path $installRoot "Boundless.ico"
$legacyInstallScriptPath = Join-Path $installRoot "Boundless-Install.ps1"

if ([string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and (Test-Path -LiteralPath $legacyInstallScriptPath)) {
    throw "Legacy script-installed Boundless files were detected at $installRoot. Remove that installation before running installer-smoke.ps1."
}

try {
    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
        $PreviousInstallerPath = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
        Invoke-MsiExec -ArgumentList @("/i", $PreviousInstallerPath, "/qn", "/norestart") -LogPath $upgradeLog
    }

    Invoke-MsiExec -ArgumentList @("/i", $InstallerPath, "/qn", "/norestart") -LogPath $installLog

    Assert-PathExists -Path (Join-Path $installRoot "boundlessd.exe") -Message "Installed daemon binary is missing."
    Assert-PathExists -Path (Join-Path $installRoot "boundlessctl.exe") -Message "Installed CLI binary is missing."
    Assert-PathExists -Path (Join-Path $installRoot "boundlesstray.exe") -Message "Installed tray binary is missing."
    Assert-PathExists -Path $resetScriptPath -Message "Installed reset helper is missing."
    Assert-PathExists -Path $iconPath -Message "Installed icon asset is missing."
    Assert-PathExists -Path $startupShortcutPath -Message "Startup shortcut is missing."
    Assert-PathExists -Path $startMenuShortcutPath -Message "Start menu shortcut is missing."
    Assert-PathExists -Path $desktopShortcutPath -Message "Desktop shortcut is missing."

    $trayPath = Join-Path $installRoot "boundlesstray.exe"
    foreach ($shortcutPath in @($startupShortcutPath, $startMenuShortcutPath, $desktopShortcutPath)) {
        if ((Get-ShortcutTarget -ShortcutPath $shortcutPath) -ne $trayPath) {
            throw "Shortcut target was unexpected: $shortcutPath"
        }

        $iconLocation = Get-ShortcutIconLocation -ShortcutPath $shortcutPath
        if (-not (Test-ExpectedShortcutIconLocation -IconLocation $iconLocation -InstalledIconPath $iconPath)) {
            throw "Shortcut icon location was unexpected for ${shortcutPath}: $iconLocation"
        }
    }

    $uninstallEntry = Get-UninstallEntry
    if ($null -eq $uninstallEntry) {
        throw "Boundless uninstall entry was not found."
    }
    if (-not [string]::IsNullOrWhiteSpace($expectedDisplayVersion) -and $uninstallEntry.DisplayVersion -ne $expectedDisplayVersion) {
        throw "Unexpected uninstall DisplayVersion: $($uninstallEntry.DisplayVersion)"
    }
    if ($uninstallEntry.InstallLocation -ne $installRoot) {
        throw "Unexpected uninstall InstallLocation: $($uninstallEntry.InstallLocation)"
    }

    $traySignature = Assert-Authenticode -Path $trayPath -Required:$RequireSignature.IsPresent
    $daemonSignature = Assert-Authenticode -Path (Join-Path $installRoot "boundlessd.exe") -Required:$RequireSignature.IsPresent
    $cliSignature = Assert-Authenticode -Path (Join-Path $installRoot "boundlessctl.exe") -Required:$RequireSignature.IsPresent

    $trayProcess = Start-Process -FilePath $trayPath -WorkingDirectory $installRoot -PassThru
    Start-Sleep -Seconds 3
    if ($trayProcess.HasExited) {
        throw "Installed tray executable exited immediately."
    }
    Stop-Process -Id $trayProcess.Id -Force -ErrorAction SilentlyContinue

    Invoke-MsiExec -ArgumentList @("/x", $InstallerPath, "/qn", "/norestart") -LogPath $uninstallLog

    Wait-ForPathRemoval -Path $installRoot
    if (Test-Path -LiteralPath $startupShortcutPath) {
        throw "Uninstall did not remove startup shortcut."
    }
    if (Test-Path -LiteralPath $startMenuShortcutPath) {
        throw "Uninstall did not remove start menu shortcut."
    }
    if (Test-Path -LiteralPath $desktopShortcutPath) {
        throw "Uninstall did not remove desktop shortcut."
    }
    if ($null -ne (Get-UninstallEntry)) {
        throw "Uninstall did not remove Boundless uninstall entry."
    }

    $summary = [ordered]@{
        installer_path = $InstallerPath
        install_root = $installRoot
        installer_signature = $installerSignature
        tray_signature = $traySignature
        daemon_signature = $daemonSignature
        cli_signature = $cliSignature
        upgraded_from = $PreviousInstallerPath
        status = "passed"
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputRoot "installer-smoke.json") -Encoding utf8
    Write-Host "installer_smoke=passed"
    Write-Host "artifacts=$OutputRoot"
}
finally {
    if (-not $KeepArtifacts -and (Test-Path -LiteralPath $OutputRoot)) {
        Remove-Item -LiteralPath $OutputRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
