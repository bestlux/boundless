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

        $remainingEntry = Get-ChildItem -LiteralPath $Path -Force -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -eq $remainingEntry) {
            return
        }

        Start-Sleep -Milliseconds 250
    }

    $remainingEntries = @(
        Get-ChildItem -LiteralPath $Path -Force -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty Name
    ) -join ", "
    if ([string]::IsNullOrWhiteSpace($remainingEntries)) {
        $remainingEntries = "<empty directory>"
    }

    throw "Timed out waiting for path removal or empty state: $Path (remaining: $remainingEntries)"
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
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "msiexec.exe failed with exit code $($process.ExitCode). Log: $LogPath"
    }

    return $process.ExitCode
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
            Where-Object {
                $_.PSObject.Properties.Match("DisplayName").Count -gt 0 -and
                $_.DisplayName -eq "Boundless"
            } |
            Select-Object -First 1
        if ($null -ne $entry) {
            return $entry
        }
    }

    return $null
}

function Test-InteractiveDesktopSession {
    if ($env:GITHUB_ACTIONS -eq "true") {
        return $false
    }

    if (-not [Environment]::UserInteractive) {
        return $false
    }

    $currentSessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
    $explorerProcess = Get-Process -Name "explorer" -ErrorAction SilentlyContinue |
        Where-Object { $_.SessionId -eq $currentSessionId } |
        Select-Object -First 1

    return $null -ne $explorerProcess
}

function Stop-BoundlessProcesses {
    Get-Process -Name "boundlesstray", "boundlessd", "boundless-service" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
}

function Assert-NoBoundlessProcesses {
    $remaining = Get-Process -Name "boundlesstray", "boundlessd", "boundless-service" -ErrorAction SilentlyContinue
    if ($null -ne $remaining) {
        $names = @($remaining | ForEach-Object { "$($_.ProcessName):$($_.Id)" }) -join ", "
        throw "Boundless processes still running after uninstall: $names"
    }
}

function Wait-ForNoBoundlessProcesses {
    param([int]$TimeoutSeconds = 10)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $remaining = Get-Process -Name "boundlesstray", "boundlessd", "boundless-service" -ErrorAction SilentlyContinue
        if ($null -eq $remaining) {
            return
        }

        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    Assert-NoBoundlessProcesses
}

function Get-BoundlessService {
    Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Wait-ForDaemonReady {
    param(
        [string]$CliPath,
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $output = (& $CliPath daemon status 2>&1 | Out-String).Trim()
        $exitCode = $LASTEXITCODE
        if ($exitCode -eq 0) {
            return $output
        }

        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for daemon readiness via $CliPath"
}

function Get-BoundlessProcessCount {
    param([string]$Name)

    $procs = Get-Process -Name $Name -ErrorAction SilentlyContinue
    if ($null -eq $procs) {
        return 0
    }

    return @($procs).Count
}

function Test-BoundlessPipePresent {
    return $null -ne (Get-ChildItem \\.\pipe\ -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq "boundlessd-api" } |
        Select-Object -First 1)
}

function Wait-ForRuntimePresence {
    param([int]$TimeoutSeconds = 20)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $trayCount = Get-BoundlessProcessCount -Name "boundlesstray"
        $daemonCount = Get-BoundlessProcessCount -Name "boundlessd"
        $pipePresent = Test-BoundlessPipePresent
        if ($trayCount -ge 1 -and $daemonCount -ge 1 -and $pipePresent) {
            return [pscustomobject]@{
                TrayCount = $trayCount
                DaemonCount = $daemonCount
                PipePresent = $pipePresent
            }
        }

        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for Boundless runtime to become present."
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
$interactiveDesktopSession = Test-InteractiveDesktopSession

if ([string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and (Test-Path -LiteralPath $legacyInstallScriptPath)) {
    throw "Legacy script-installed Boundless files were detected at $installRoot. Remove that installation before running installer-smoke.ps1."
}

try {
    Stop-BoundlessProcesses

    $upgradeWhileRunningTested = $false
    $upgradeWhileRunningSkippedReason = $null
    $upgradeDaemonStatus = $null
    $postUpgradeTrayCount = $null
    $postUpgradeDaemonCount = $null
    $upgradeInstallExitCode = $null
    $installExitCode = $null
    $uninstallExitCode = $null

    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
        $PreviousInstallerPath = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
        $upgradeInstallExitCode = Invoke-MsiExec -ArgumentList @("/i", $PreviousInstallerPath, "/qn", "/norestart") -LogPath $upgradeLog

        if ($interactiveDesktopSession) {
            $previousTrayPath = Join-Path $installRoot "boundlesstray.exe"
            $previousCliPath = Join-Path $installRoot "boundlessctl.exe"
            Assert-PathExists -Path $previousTrayPath -Message "Previous installer did not lay down tray executable."
            Assert-PathExists -Path $previousCliPath -Message "Previous installer did not lay down CLI executable."

            $previousTrayProcess = Start-Process -FilePath $previousTrayPath -WorkingDirectory $installRoot -PassThru
            Start-Sleep -Seconds 3
            if ($previousTrayProcess.HasExited) {
                throw "Previous installer tray exited before upgrade-running smoke could begin. Exit code: $($previousTrayProcess.ExitCode)"
            }

            $null = Wait-ForDaemonReady -CliPath $previousCliPath
        }
        else {
            $upgradeWhileRunningSkippedReason = "interactive desktop session not available"
        }
    }

    $installExitCode = Invoke-MsiExec -ArgumentList @("/i", $InstallerPath, "/qn", "/norestart") -LogPath $installLog

    $daemonPath = Join-Path $installRoot "boundlessd.exe"
    $servicePath = Join-Path $installRoot "boundless-service.exe"
    $cliPath = Join-Path $installRoot "boundlessctl.exe"
    $trayPath = Join-Path $installRoot "boundlesstray.exe"

    Assert-PathExists -Path $daemonPath -Message "Installed daemon binary is missing."
    Assert-PathExists -Path $servicePath -Message "Installed service binary is missing."
    Assert-PathExists -Path $cliPath -Message "Installed CLI binary is missing."
    Assert-PathExists -Path $trayPath -Message "Installed tray binary is missing."
    Assert-PathExists -Path $resetScriptPath -Message "Installed reset helper is missing."
    Assert-PathExists -Path $iconPath -Message "Installed icon asset is missing."
    Assert-PathExists -Path $startupShortcutPath -Message "Startup shortcut is missing."
    Assert-PathExists -Path $startMenuShortcutPath -Message "Start menu shortcut is missing."
    Assert-PathExists -Path $desktopShortcutPath -Message "Desktop shortcut is missing."

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
    if (
        -not [string]::IsNullOrWhiteSpace($uninstallEntry.InstallLocation) -and
        $uninstallEntry.InstallLocation -ne $installRoot
    ) {
        throw "Unexpected uninstall InstallLocation: $($uninstallEntry.InstallLocation)"
    }

    $traySignature = Assert-Authenticode -Path $trayPath -Required:$RequireSignature.IsPresent
    $daemonSignature = Assert-Authenticode -Path $daemonPath -Required:$RequireSignature.IsPresent
    $serviceSignature = Assert-Authenticode -Path $servicePath -Required:$RequireSignature.IsPresent
    $cliSignature = Assert-Authenticode -Path $cliPath -Required:$RequireSignature.IsPresent

    $trayVersionOutput = (& $trayPath --version 2>&1 | Out-String).Trim()
    $trayVersionExitCode = $LASTEXITCODE
    if ($trayVersionExitCode -ne 0) {
        throw "Installed tray executable failed to report its version. Exit code: $trayVersionExitCode."
    }
    if (
        -not [string]::IsNullOrWhiteSpace($expectedDisplayVersion) -and
        -not [string]::IsNullOrWhiteSpace($trayVersionOutput) -and
        $trayVersionOutput -notmatch [regex]::Escape($expectedDisplayVersion)
    ) {
        throw "Installed tray executable reported an unexpected version string: $trayVersionOutput"
    }

    $trayLaunchMode = if ($interactiveDesktopSession) { "interactive_desktop" } else { "headless_session" }
    $trayExitedEarly = $false
    $trayExitCode = $null
    $daemonReadyOutput = $null
    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and $interactiveDesktopSession) {
        $upgradeWhileRunningTested = $true
        $runtimePresence = Wait-ForRuntimePresence
        $upgradeDaemonStatus = "tray_count=$($runtimePresence.TrayCount) daemon_count=$($runtimePresence.DaemonCount) pipe_present=$($runtimePresence.PipePresent)"
        Start-Sleep -Seconds 3
        $postUpgradeTrayCount = Get-BoundlessProcessCount -Name "boundlesstray"
        $postUpgradeDaemonCount = Get-BoundlessProcessCount -Name "boundlessd"
        if ($postUpgradeTrayCount -ne 1) {
            throw "Expected exactly one boundlesstray.exe after upgrade-while-running smoke, found $postUpgradeTrayCount."
        }
        if ($postUpgradeDaemonCount -ne 1) {
            throw "Expected exactly one boundlessd.exe after upgrade-while-running smoke, found $postUpgradeDaemonCount."
        }
    }
    else {
        $trayProcess = Start-Process -FilePath $trayPath -WorkingDirectory $installRoot -PassThru
        Start-Sleep -Seconds 3
        if ($trayProcess.HasExited) {
            $trayExitedEarly = $true
            $trayExitCode = $trayProcess.ExitCode
        }
        else {
            $daemonReadyOutput = Wait-ForDaemonReady -CliPath $cliPath
        }
    }

    $uninstallExitCode = Invoke-MsiExec -ArgumentList @("/x", $InstallerPath, "/qn", "/norestart") -LogPath $uninstallLog

    Wait-ForNoBoundlessProcesses
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
    if ($null -ne (Get-BoundlessService)) {
        throw "Uninstall left a registered Boundless service."
    }

    $summary = [ordered]@{
        installer_path = $InstallerPath
        install_root = $installRoot
        installer_signature = $installerSignature
        tray_signature = $traySignature
        daemon_signature = $daemonSignature
        service_signature = $serviceSignature
        cli_signature = $cliSignature
        tray_version_output = $trayVersionOutput
        tray_version_exit_code = $trayVersionExitCode
        tray_launch_mode = $trayLaunchMode
        tray_exited_early = $trayExitedEarly
        tray_exit_code = $trayExitCode
        daemon_ready_output = $daemonReadyOutput
        upgraded_from = $PreviousInstallerPath
        previous_install_exit_code = $upgradeInstallExitCode
        install_exit_code = $installExitCode
        uninstall_exit_code = $uninstallExitCode
        upgrade_while_running_tested = $upgradeWhileRunningTested
        upgrade_while_running_skipped_reason = $upgradeWhileRunningSkippedReason
        upgrade_daemon_status = $upgradeDaemonStatus
        post_upgrade_tray_count = $postUpgradeTrayCount
        post_upgrade_daemon_count = $postUpgradeDaemonCount
        post_uninstall_processes_cleared = $true
        post_uninstall_service_removed = $true
        status = "passed"
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputRoot "installer-smoke.json") -Encoding utf8
    Write-Host "installer_smoke=passed"
    Write-Host "artifacts=$OutputRoot"
}
finally {
    Stop-BoundlessProcesses
    if (-not $KeepArtifacts -and (Test-Path -LiteralPath $OutputRoot)) {
        Remove-Item -LiteralPath $OutputRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
