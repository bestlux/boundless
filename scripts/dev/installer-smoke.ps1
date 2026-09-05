[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallerPath = "",
    [string]$PreviousInstallerPath = "",
    [string]$OutputRoot = "",
    [string]$AllowedUserSid = "",
    [switch]$RequireSignature,
    [switch]$KeepArtifacts,
    [switch]$SelfTest
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

function Assert-PathMissing {
    param(
        [string]$Path,
        [string]$Message
    )

    if (Test-Path -LiteralPath $Path) {
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

function Resolve-PowerShellExecutable {
    foreach ($name in @("pwsh", "powershell")) {
        $command = Get-Command $name -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $command) {
            return $command.Source
        }
    }
    throw "Could not find pwsh or powershell for the packaged install helper."
}

function Get-BoundlessInstallHelperEvidenceValue {
    param(
        [string]$Output,
        [string]$Name
    )

    $prefix = "$Name="
    $values = @()
    $reader = [System.IO.StringReader]::new($Output)
    try {
        while ($null -ne ($line = $reader.ReadLine())) {
            if ($line.StartsWith($prefix, [StringComparison]::Ordinal)) {
                $values += $line.Substring($prefix.Length)
            }
        }
    }
    finally {
        $reader.Dispose()
    }

    if ($values.Count -eq 0) {
        throw "Packaged install helper did not emit required evidence '$Name'."
    }
    if ($values.Count -ne 1) {
        throw "Packaged install helper emitted required evidence '$Name' more than once."
    }

    $value = $values[0].Trim()
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw "Packaged install helper emitted empty required evidence '$Name'."
    }
    return $value
}

function Assert-BoundlessInstallHelperEvidenceParserFixtures {
    $evidenceName = "boundless_install_tray_shutdown_count"
    $expectedValue = "7"
    $validFixtures = @(
        [pscustomobject]@{
            Name = "lf"
            Output = "before=ignored`n$evidenceName=$expectedValue`nafter=ignored"
        },
        [pscustomobject]@{
            Name = "crlf"
            Output = "before=ignored`r`n$evidenceName=$expectedValue`r`nafter=ignored"
        },
        [pscustomobject]@{
            Name = "cr"
            Output = "before=ignored`r$evidenceName=$expectedValue`rafter=ignored"
        },
        [pscustomobject]@{
            Name = "final-line"
            Output = "before=ignored`r`n$evidenceName=$expectedValue"
        }
    )

    foreach ($fixture in $validFixtures) {
        $actualValue = Get-BoundlessInstallHelperEvidenceValue `
            -Output $fixture.Output `
            -Name $evidenceName
        if ($actualValue -ne $expectedValue) {
            throw "Install helper evidence parser fixture '$($fixture.Name)' returned '$actualValue'."
        }
    }

    foreach ($invalidFixture in @(
            [pscustomobject]@{
                Name = "missing"
                Output = "before=ignored`r`nafter=ignored"
                ExpectedMessage = "did not emit required evidence"
            },
            [pscustomobject]@{
                Name = "empty"
                Output = "$evidenceName=   "
                ExpectedMessage = "emitted empty required evidence"
            },
            [pscustomobject]@{
                Name = "duplicate"
                Output = "$evidenceName=1`r`n$evidenceName=2"
                ExpectedMessage = "more than once"
            },
            [pscustomobject]@{
                Name = "embedded"
                Output = "noise_$evidenceName=$expectedValue"
                ExpectedMessage = "did not emit required evidence"
            }
        )) {
        $caughtMessage = $null
        try {
            Get-BoundlessInstallHelperEvidenceValue `
                -Output $invalidFixture.Output `
                -Name $evidenceName | Out-Null
        }
        catch {
            $caughtMessage = $_.Exception.Message
        }
        if ($null -eq $caughtMessage -or $caughtMessage -notmatch [regex]::Escape($invalidFixture.ExpectedMessage)) {
            throw "Install helper evidence parser fixture '$($invalidFixture.Name)' did not fail as expected."
        }
    }

    Write-Host "installer_smoke_helper_evidence_parser_fixtures=passed"
}

function Invoke-BoundlessInstallHelper {
    param(
        [string]$HelperPath,
        [string]$MsiPath,
        [string]$Sid,
        [string]$LogPath,
        [bool]$ExpectRunningTray
    )

    $arguments = @(
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        $HelperPath,
        "-InstallerPath",
        $MsiPath,
        "-AllowedUserSid",
        $Sid,
        "-Quiet",
        "-NoRestart",
        "-LogPath",
        $LogPath
    )
    $global:LASTEXITCODE = 0
    $outputLines = @(
        & $script:PowerShellExe @arguments 2>&1 |
            ForEach-Object { $_.ToString() }
    )
    $exitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
    $outputLines | ForEach-Object { Write-Host $_ }
    if ($exitCode -ne 0) {
        throw "Packaged install helper exited with $exitCode."
    }

    $output = $outputLines -join [Environment]::NewLine
    $trayShutdownCount = [int](Get-BoundlessInstallHelperEvidenceValue -Output $output -Name "boundless_install_tray_shutdown_count")
    $quiescenceAcquired = Get-BoundlessInstallHelperEvidenceValue -Output $output -Name "boundless_install_tray_quiescence_acquired"
    $serviceStopFinal = Get-BoundlessInstallHelperEvidenceValue -Output $output -Name "boundless_install_service_stop_final"
    $msiExitCode = [int](Get-BoundlessInstallHelperEvidenceValue -Output $output -Name "boundless_install_exit_code")
    $coreVerified = Get-BoundlessInstallHelperEvidenceValue -Output $output -Name "boundless_install_core_verified"

    if ($ExpectRunningTray -and $trayShutdownCount -lt 1) {
        throw "Packaged helper did not report closing the running N-1 tray."
    }
    if ($quiescenceAcquired -ne "True") {
        throw "Packaged helper did not report acquiring the tray quiescence lease."
    }
    if ($serviceStopFinal -ne "StoppedOrNotInstalledBeforeMsi") {
        throw "Packaged helper did not report a completed pre-MSI service stop."
    }
    if ($msiExitCode -notin @(0, 3010)) {
        throw "Packaged helper reported unexpected MSI exit code $msiExitCode."
    }
    if ($coreVerified -ne "true") {
        throw "Packaged helper did not report verified post-install core health."
    }

    return [ordered]@{
        helper_path = $HelperPath
        tray_shutdown_count = $trayShutdownCount
        tray_quiescence_acquired = $true
        service_stop_final = $serviceStopFinal
        msi_exit_code = $msiExitCode
        core_verified = $true
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

    return $resolvedLocation -imatch '[\\/](?:Microsoft|Windows)[\\/]Installer[\\/]\{[^\\/]+\}[\\/]BoundlessIcon\.ico$'
}

function ConvertTo-AuthenticodeStatusName {
    param([object]$Status)

    if ($null -eq $Status) {
        throw "Authenticode signature status was missing."
    }

    $name = $Status.ToString()
    if ([string]::IsNullOrWhiteSpace($name)) {
        throw "Authenticode signature status did not have a stable name."
    }
    return $name
}

function Assert-AuthenticodeStatusSerializationFixtures {
    foreach ($status in [Enum]::GetValues([System.Management.Automation.SignatureStatus])) {
        $expectedName = $status.ToString()
        $summaryJson = [ordered]@{
            signature = ConvertTo-AuthenticodeStatusName -Status $status
        } | ConvertTo-Json -Compress
        $roundTrip = $summaryJson | ConvertFrom-Json
        if (
            $roundTrip.signature -isnot [string] -or
            $roundTrip.signature -cne $expectedName
        ) {
            throw "Authenticode status '$expectedName' did not serialize as its stable string name."
        }
    }

    $rawStatusName = (Get-AuthenticodeSignature -LiteralPath $PSCommandPath).Status.ToString()
    $assertedStatusName = Assert-Authenticode -Path $PSCommandPath -Required $false
    $assertedRoundTrip = ([ordered]@{
        input_injector_signature = $assertedStatusName
    } | ConvertTo-Json -Compress) | ConvertFrom-Json
    if (
        $assertedStatusName -isnot [string] -or
        $assertedStatusName -cne $rawStatusName -or
        $assertedRoundTrip.input_injector_signature -isnot [string] -or
        $assertedRoundTrip.input_injector_signature -cne $rawStatusName
    ) {
        throw "Assert-Authenticode did not preserve the live signature status as a JSON string."
    }

    Write-Host "installer_smoke_authenticode_status_serialization_fixtures=passed"
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

    return ConvertTo-AuthenticodeStatusName -Status $signature.Status
}

function Get-WindowsManifestToolPath {
    $command = Get-Command "mt.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }

    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    if (Test-Path -LiteralPath $kitsRoot) {
        $candidate = Get-ChildItem -LiteralPath $kitsRoot -Filter "mt.exe" -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\mt\.exe$' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($null -ne $candidate) {
            return $candidate.FullName
        }
    }

    throw "mt.exe was not found. Install the Windows SDK before validating the input injector execution manifest."
}

function Assert-InputInjectorExecutionManifest {
    param([string]$Path)

    $manifestTool = Get-WindowsManifestToolPath
    $manifestPath = Join-Path ([IO.Path]::GetTempPath()) ("boundless-input-injector-manifest-" + [guid]::NewGuid().ToString("N") + ".xml")
    try {
        $global:LASTEXITCODE = 0
        & $manifestTool `
            "-nologo" `
            "-inputresource:$Path;#1" `
            "-out:$manifestPath"
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $manifestPath)) {
            throw "mt.exe could not extract the input injector execution manifest. Exit code: $LASTEXITCODE."
        }

        [xml]$manifest = Get-Content -LiteralPath $manifestPath -Raw
        $namespaceManager = New-Object System.Xml.XmlNamespaceManager($manifest.NameTable)
        $namespaceManager.AddNamespace("asmv3", "urn:schemas-microsoft-com:asm.v3")
        $executionLevels = @($manifest.SelectNodes("//asmv3:requestedExecutionLevel", $namespaceManager))
        if ($executionLevels.Count -ne 1) {
            throw "Input injector manifest must contain exactly one requestedExecutionLevel element; found $($executionLevels.Count)."
        }

        $executionLevel = $executionLevels[0].GetAttribute("level")
        $uiAccess = $executionLevels[0].GetAttribute("uiAccess")
        if ($executionLevel -cne "requireAdministrator" -or $uiAccess -cne "false") {
            throw "Input injector manifest must declare requireAdministrator with uiAccess=false; level=$executionLevel uiAccess=$uiAccess."
        }

        return [ordered]@{
            execution_level = $executionLevel
            ui_access = $uiAccess
        }
    }
    finally {
        Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
    }
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
        [pscustomobject]@{
            Root = "HKLM"
            Path = "Registry::HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Uninstall\*"
        },
        [pscustomobject]@{
            Root = "HKCU"
            Path = "Registry::HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall\*"
        }
    )

    foreach ($key in $keys) {
        $entry = Get-ItemProperty -Path $key.Path -ErrorAction SilentlyContinue |
            Where-Object {
                $_.PSObject.Properties.Match("DisplayName").Count -gt 0 -and
                $_.DisplayName -eq "Boundless"
            } |
            Select-Object -First 1
        if ($null -ne $entry) {
            $entry | Add-Member -NotePropertyName RegistryRoot -NotePropertyValue $key.Root -Force
            return $entry
        }
    }

    return $null
}

function Get-BoundlessInstallRoot {
    param([object]$UninstallEntry)

    if (
        $null -ne $UninstallEntry -and
        $UninstallEntry.PSObject.Properties.Match("InstallLocation").Count -gt 0 -and
        -not [string]::IsNullOrWhiteSpace($UninstallEntry.InstallLocation)
    ) {
        return $UninstallEntry.InstallLocation
    }

    $programFilesRoot = Join-Path $env:ProgramFiles "Boundless"
    if (Test-Path -LiteralPath $programFilesRoot) {
        return $programFilesRoot
    }

    $legacyRoot = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)) "Programs\Boundless"
    if (Test-Path -LiteralPath $legacyRoot) {
        return $legacyRoot
    }

    return $programFilesRoot
}

function Get-FileEvidence {
    param(
        [string]$Path,
        [string]$Label
    )

    Assert-PathExists -Path $Path -Message "$Label is missing: $Path"
    $item = Get-Item -LiteralPath $Path
    return [ordered]@{
        path = $item.FullName
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $item.FullName).Hash
        length = $item.Length
        last_write_time_utc = $item.LastWriteTimeUtc.ToString("o")
    }
}

function Test-IsUnderPath {
    param(
        [string]$Path,
        [string]$Root
    )

    $resolvedPath = [System.IO.Path]::GetFullPath($Path).TrimEnd('\')
    $resolvedRoot = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
    return $resolvedPath.StartsWith($resolvedRoot, [System.StringComparison]::OrdinalIgnoreCase)
}

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-CurrentUserSid {
    return [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
}

function Assert-AllowedUserSid {
    param([string]$Sid)

    if ([string]::IsNullOrWhiteSpace($Sid)) {
        throw "Allowed user SID was empty. Provide -AllowedUserSid with the intended desktop user SID."
    }
    if ($Sid -notmatch '^S-1-\d+(?:-\d+)+$') {
        throw "Allowed user SID was not a strict SID string: $Sid"
    }
}

function Get-InstallerEvidence {
    $keyPath = "Registry::HKEY_LOCAL_MACHINE\Software\Boundless\Installer"
    if (-not (Test-Path -LiteralPath $keyPath)) {
        throw "Machine-wide installer evidence key was not found: HKLM\Software\Boundless\Installer"
    }

    $evidence = Get-ItemProperty -LiteralPath $keyPath
    foreach ($name in @("PayloadInstalled", "ServicePayloadInstalled", "Installed")) {
        if ($evidence.PSObject.Properties.Match($name).Count -eq 0) {
            throw "Machine-wide installer evidence value was not found: HKLM\Software\Boundless\Installer\$name"
        }
        if ([int]$evidence.$name -ne 1) {
            throw "Machine-wide installer evidence value was unexpected: $name=$($evidence.$name)"
        }
    }

    return [ordered]@{
        root = "HKLM"
        key = "Software\Boundless\Installer"
        payload_installed = [int]$evidence.PayloadInstalled
        service_payload_installed = [int]$evidence.ServicePayloadInstalled
        shortcuts_installed = [int]$evidence.Installed
    }
}

function Test-InstallerEvidencePresent {
    $keyPath = "Registry::HKEY_LOCAL_MACHINE\Software\Boundless\Installer"
    if (-not (Test-Path -LiteralPath $keyPath)) {
        return $false
    }

    $evidence = Get-ItemProperty -LiteralPath $keyPath
    foreach ($name in @("PayloadInstalled", "ServicePayloadInstalled", "Installed")) {
        if ($evidence.PSObject.Properties.Match($name).Count -gt 0) {
            return $true
        }
    }

    return $false
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
    Get-Process -Name "boundlesstray", "boundlessd", "boundless-service", "boundless-input-injector" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
}

function Assert-NoBoundlessProcesses {
    $remaining = Get-Process -Name "boundlesstray", "boundlessd", "boundless-service", "boundless-input-injector" -ErrorAction SilentlyContinue
    if ($null -ne $remaining) {
        $names = @($remaining | ForEach-Object { "$($_.ProcessName):$($_.Id)" }) -join ", "
        throw "Boundless processes still running after uninstall: $names"
    }
}

function Wait-ForNoBoundlessProcesses {
    param([int]$TimeoutSeconds = 10)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $remaining = Get-Process -Name "boundlesstray", "boundlessd", "boundless-service", "boundless-input-injector" -ErrorAction SilentlyContinue
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

function Get-BoundlessServiceConfig {
    Get-CimInstance -ClassName Win32_Service -Filter "Name='BoundlessService'" -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Test-WindowsPathEqual {
    param(
        [string]$Left,
        [string]$Right
    )

    if ([string]::IsNullOrWhiteSpace($Left) -or [string]::IsNullOrWhiteSpace($Right)) {
        return $false
    }
    $leftFull = [IO.Path]::GetFullPath($Left).TrimEnd('\')
    $rightFull = [IO.Path]::GetFullPath($Right).TrimEnd('\')
    return $leftFull.Equals($rightFull, [StringComparison]::OrdinalIgnoreCase)
}

function Get-WindowsCommandExecutablePath {
    param([string]$CommandLine)

    if ([string]::IsNullOrWhiteSpace($CommandLine)) {
        throw "Windows command line was empty while parsing its executable."
    }
    $trimmed = $CommandLine.Trim()
    if ($trimmed.StartsWith('"')) {
        $match = [regex]::Match($trimmed, '^"(?<path>[^\"]+)"(?=\s|$)')
    }
    else {
        $match = [regex]::Match(
            $trimmed,
            '^(?<path>.+?\.exe)(?=\s|$)',
            [Text.RegularExpressions.RegexOptions]::IgnoreCase
        )
    }
    if (-not $match.Success) {
        throw "Could not parse an executable token from Windows command line: $CommandLine"
    }
    try {
        return [IO.Path]::GetFullPath($match.Groups['path'].Value).TrimEnd('\')
    }
    catch {
        throw "Windows command line executable path was invalid: $($match.Groups['path'].Value)"
    }
}

function Assert-WindowsServiceExecutablePathFixtures {
    $expected = 'C:\Program Files\Boundless\boundless-service.exe'
    foreach ($commandLine in @(
        '"C:\Program Files\Boundless\boundless-service.exe" --allowed-user-sid=S-1-5-21-1',
        'C:\Program Files\Boundless\boundless-service.exe --allowed-user-sid=S-1-5-21-1'
    )) {
        $actual = Get-WindowsCommandExecutablePath -CommandLine $commandLine
        if (-not (Test-WindowsPathEqual -Left $actual -Right $expected)) {
            throw "Service executable parser fixture did not accept the exact executable token: $commandLine"
        }
    }
    foreach ($commandLine in @(
        '"C:\Program Files\Boundless\boundless-service.exe.evil" --allowed-user-sid=S-1-5-21-1',
        'C:\Program Files\Boundless\boundless-service.exe.evil --allowed-user-sid=S-1-5-21-1'
    )) {
        $accepted = $false
        try {
            $actual = Get-WindowsCommandExecutablePath -CommandLine $commandLine
            $accepted = Test-WindowsPathEqual -Left $actual -Right $expected
        }
        catch {
            $accepted = $false
        }
        if ($accepted) {
            throw "Service executable parser fixture accepted a suffix-confused executable: $commandLine"
        }
    }
}

function Wait-BoundlessServiceStatus {
    param(
        [string]$ExpectedStatus,
        [int]$TimeoutSeconds = 30
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $service = Get-BoundlessService
        if ($null -ne $service -and $service.Status.ToString() -eq $ExpectedStatus) {
            return $service
        }

        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    if ($null -eq $service) {
        throw "BoundlessService was not found while waiting for $ExpectedStatus."
    }
    throw "BoundlessService did not reach $ExpectedStatus within $($TimeoutSeconds)s; current=$($service.Status)."
}

function Wait-BoundlessServiceRemoved {
    param([int]$TimeoutSeconds = 20)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if ($null -eq (Get-BoundlessService)) {
            return
        }

        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    throw "BoundlessService registration was still present after $($TimeoutSeconds)s."
}

function Remove-BoundlessServiceRegistrationForRepair {
    $service = Get-BoundlessService
    if ($null -eq $service) {
        throw "BoundlessService was not present before repair registration recovery test."
    }

    if ($service.Status.ToString() -ne "Stopped") {
        Stop-Service -Name "BoundlessService" -Force -ErrorAction Stop
        Wait-BoundlessServiceStatus -ExpectedStatus "Stopped" | Out-Null
    }

    $deleteOutput = sc.exe delete BoundlessService 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to delete BoundlessService before repair test. Exit code: $LASTEXITCODE Output: $deleteOutput"
    }

    Wait-BoundlessServiceRemoved
    return $deleteOutput.Trim()
}

function Assert-BoundlessServiceConfig {
    param(
        [string]$ExpectedServicePath,
        [string]$ExpectedAllowedUserSid
    )

    $service = Get-BoundlessServiceConfig
    if ($null -eq $service) {
        throw "BoundlessService was not registered by the installer."
    }

    $actualServicePath = Get-WindowsCommandExecutablePath -CommandLine $service.PathName
    if (-not (Test-WindowsPathEqual -Left $actualServicePath -Right $ExpectedServicePath)) {
        throw "BoundlessService PathName did not point at the Program Files service binary. PathName=$($service.PathName)"
    }
    if ($service.PathName -notmatch "(^|\s)--allowed-user-sid=([^\s]+)") {
        throw "BoundlessService PathName did not include --allowed-user-sid. PathName=$($service.PathName)"
    }

    $sidMatches = [regex]::Matches($service.PathName, "--allowed-user-sid=([^\s]+)")
    if ($sidMatches.Count -ne 1) {
        throw "BoundlessService PathName must include exactly one --allowed-user-sid argument. PathName=$($service.PathName)"
    }
    $actualSid = $sidMatches[0].Groups[1].Value.Trim('"')
    if ($actualSid -ne $ExpectedAllowedUserSid) {
        throw "BoundlessService allowed user SID mismatch. Expected $ExpectedAllowedUserSid, got $actualSid."
    }

    if ($service.StartMode -ne "Auto") {
        throw "BoundlessService StartMode was expected to be Auto, got $($service.StartMode)."
    }
    if ($service.StartName -ne "LocalSystem") {
        throw "BoundlessService StartName was expected to be LocalSystem, got $($service.StartName)."
    }

    return [ordered]@{
        name = $service.Name
        path_name = $service.PathName
        start_mode = $service.StartMode
        start_name = $service.StartName
        state = $service.State
        allowed_user_sid = $actualSid
    }
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

function Get-BoundlessProcessCountForSession {
    param(
        [string]$Name,
        [int]$SessionId
    )

    $procs = Get-Process -Name $Name -ErrorAction SilentlyContinue |
        Where-Object { $_.SessionId -eq $SessionId }
    if ($null -eq $procs) {
        return 0
    }

    return @($procs).Count
}

function Wait-BoundlessProcessCountForSession {
    param(
        [string]$Name,
        [int]$SessionId,
        [int]$ExpectedCount,
        [int]$TimeoutSeconds = 10
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $count = Get-BoundlessProcessCountForSession -Name $Name -SessionId $SessionId
        if ($count -eq $ExpectedCount) {
            return $count
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)

    throw "Expected $ExpectedCount $Name process(es) in session $SessionId within $($TimeoutSeconds)s; found $count."
}

function Get-BoundlessDaemonRuntimeCount {
    return (Get-BoundlessProcessCount -Name "boundlessd") +
        (Get-BoundlessProcessCount -Name "boundless-service")
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
        $daemonCount = Get-BoundlessDaemonRuntimeCount
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

if ($SelfTest) {
    Assert-BoundlessInstallHelperEvidenceParserFixtures
    Assert-AuthenticodeStatusSerializationFixtures
    Assert-WindowsServiceExecutablePathFixtures
    Write-Host "installer_smoke_self_test=passed"
    return
}

if ((Get-Variable -Name IsWindows -ErrorAction SilentlyContinue) -and (-not $IsWindows)) {
    throw "installer-smoke.ps1 is supported on Windows only."
}
if ((-not (Get-Variable -Name IsWindows -ErrorAction SilentlyContinue)) -and ($env:OS -ne "Windows_NT")) {
    throw "installer-smoke.ps1 is supported on Windows only."
}
Assert-WindowsServiceExecutablePathFixtures
if (-not (Test-IsAdministrator)) {
    throw "installer-smoke.ps1 must run from an elevated PowerShell session for machine-wide Program Files MSI validation."
}
$script:PowerShellExe = Resolve-PowerShellExecutable

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot ("artifacts\installer-validation\" + (Get-Date -Format "yyyyMMdd-HHmmss"))
}
Ensure-Directory -Path $OutputRoot

$allowedUserSidSource = "explicit"
if ([string]::IsNullOrWhiteSpace($AllowedUserSid)) {
    $AllowedUserSid = Get-CurrentUserSid
    $allowedUserSidSource = "current_process"
}
Assert-AllowedUserSid -Sid $AllowedUserSid
$msiInstallProperties = @("BOUNDLESS_ALLOWED_USER_SID=$AllowedUserSid")

if ([string]::IsNullOrWhiteSpace($InstallerPath)) {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        throw "Provide either -InstallerPath or -Version."
    }

    Push-Location $repoRoot
    try {
        & cargo build --locked --release -p boundless-daemon -p boundless-cli -p boundless-tray -p boundless-input-injector
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --locked --release failed with exit code $LASTEXITCODE"
        }

        $InstallerPath = Join-Path $OutputRoot ("Boundless-$Version-windows-x64.msi")
        & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repoRoot "scripts\release\package-windows.ps1") `
            -Version $Version `
            -DaemonPath (Join-Path $repoRoot "target\release\boundlessd.exe") `
            -CliPath (Join-Path $repoRoot "target\release\boundlessctl.exe") `
            -TrayPath (Join-Path $repoRoot "target\release\boundlesstray.exe") `
            -InputInjectorPath (Join-Path $repoRoot "target\release\boundless-input-injector.exe") `
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
$installHelperPath = Join-Path (Split-Path -Parent $InstallerPath) (
    [IO.Path]::GetFileNameWithoutExtension($InstallerPath) + "-install.ps1"
)
if (
    -not [string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and
    -not (Test-Path -LiteralPath $installHelperPath)
) {
    throw "N-1 upgrade smoke requires the packaged install helper: $installHelperPath"
}
$installerSignature = Assert-Authenticode -Path $InstallerPath -Required:$RequireSignature.IsPresent
$expectedDisplayVersion = Get-ExpectedDisplayVersion -Path $InstallerPath

$installLog = Join-Path $OutputRoot "install.log"
$upgradeLog = Join-Path $OutputRoot "upgrade.log"
$repairLog = Join-Path $OutputRoot "repair.log"
$uninstallLog = Join-Path $OutputRoot "uninstall.log"

$currentUserStartupShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::Startup)) "Boundless.lnk"
$commonStartupShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::CommonStartup)) "Boundless.lnk"
$startMenuShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::CommonPrograms)) "Boundless.lnk"
$desktopShortcutPath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::CommonDesktopDirectory)) "Boundless.lnk"
$installRoot = Join-Path $env:ProgramFiles "Boundless"
$legacyInstallRoot = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)) "Programs\Boundless"
$resetScriptPath = Join-Path $installRoot "Boundless-Reset.ps1"
$iconPath = Join-Path $installRoot "Boundless.ico"
$legacyInstallScriptPath = Join-Path $legacyInstallRoot "Boundless-Install.ps1"
$interactiveDesktopSession = Test-InteractiveDesktopSession

if ([string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and (Test-Path -LiteralPath $legacyInstallScriptPath)) {
    throw "Legacy script-installed Boundless files were detected at $legacyInstallRoot. Remove that installation before running installer-smoke.ps1."
}

try {
    Stop-BoundlessProcesses

    $upgradeWhileRunningTested = $false
    $upgradeWhileRunningSkippedReason = $null
    $postUpgradeTrayRelaunched = $false
    $upgradeDaemonStatus = $null
    $postUpgradeTrayCount = $null
    $postUpgradeDaemonCount = $null
    $inputInjectorCountAfterTrayLaunch = $null
    $inputInjectorCountAfterRepair = $null
    $traySingleInstanceTested = $false
    $traySecondLaunchExitCode = $null
    $trayCurrentSessionCount = $null
    $trayGracefulQuitTested = $false
    $trayQuitControlExitCode = $null
    $trayQuitElapsedMilliseconds = $null
    $upgradeInstallExitCode = $null
    $installExitCode = $null
    $installHelperUpgradeEvidence = $null
    $repairExitCode = $null
    $uninstallExitCode = $null
    $previousInstallRoot = $null
    $previousUninstallRegistryRoot = $null
    $previousAppPayloadEvidence = $null
    $previousServicePayloadEvidence = $null
    $previousServiceInstallConfig = $null
    $currentAppPayloadEvidence = $null
    $currentServicePayloadEvidence = $null
    $upgradePayloadReplacement = $null
    $repairServiceDeleteOutput = $null
    $repairServiceConfig = $null
    $repairDaemonStatusOutput = $null
    $serviceRunningAfterRepair = $false

    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
        $PreviousInstallerPath = (Resolve-Path -LiteralPath $PreviousInstallerPath).Path
        $upgradeInstallExitCode = Invoke-MsiExec -ArgumentList (@("/i", $PreviousInstallerPath, "/qn", "/norestart") + $msiInstallProperties) -LogPath $upgradeLog

        $previousUninstallEntry = Get-UninstallEntry
        if ($null -eq $previousUninstallEntry) {
            throw "Previous installer did not create a Boundless uninstall entry."
        }
        $previousUninstallRegistryRoot = $previousUninstallEntry.RegistryRoot
        $previousInstallRoot = Get-BoundlessInstallRoot -UninstallEntry $previousUninstallEntry
        $previousTrayPath = Join-Path $previousInstallRoot "boundlesstray.exe"
        $previousServicePath = Join-Path $previousInstallRoot "boundless-service.exe"
        $previousCliPath = Join-Path $previousInstallRoot "boundlessctl.exe"
        $previousAppPayloadEvidence = Get-FileEvidence -Path $previousTrayPath -Label "Previous installer tray executable"
        $previousServicePayloadEvidence = Get-FileEvidence -Path $previousServicePath -Label "Previous installer service executable"
        $previousServiceConfig = Get-BoundlessServiceConfig
        if ($null -ne $previousServiceConfig) {
            $previousServiceInstallConfig = [ordered]@{
                name = $previousServiceConfig.Name
                path_name = $previousServiceConfig.PathName
                start_mode = $previousServiceConfig.StartMode
                start_name = $previousServiceConfig.StartName
                state = $previousServiceConfig.State
            }
        }

        if ($interactiveDesktopSession) {
            Assert-PathExists -Path $previousTrayPath -Message "Previous installer did not lay down tray executable."
            Assert-PathExists -Path $previousCliPath -Message "Previous installer did not lay down CLI executable."

            $previousTrayProcess = Start-Process -FilePath $previousTrayPath -WorkingDirectory $previousInstallRoot -PassThru
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

    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
        $helperArgs = @{
            HelperPath = $installHelperPath
            MsiPath = $InstallerPath
            Sid = $AllowedUserSid
            LogPath = $installLog
            ExpectRunningTray = $interactiveDesktopSession
        }
        $installHelperUpgradeEvidence = Invoke-BoundlessInstallHelper @helperArgs
        $installExitCode = $installHelperUpgradeEvidence.msi_exit_code
    }
    else {
        $installExitCode = Invoke-MsiExec -ArgumentList (@("/i", $InstallerPath, "/qn", "/norestart") + $msiInstallProperties) -LogPath $installLog
    }

    Assert-PathExists -Path $installLog -Message "Installer did not preserve the requested MSI log."
    $installerStageResidue = @(
        Get-ChildItem `
            -LiteralPath ([Environment]::GetFolderPath([Environment+SpecialFolder]::CommonApplicationData)) `
            -Directory `
            -Filter "BoundlessInstaller-*" `
            -ErrorAction SilentlyContinue
    )
    if ($installerStageResidue.Count -ne 0) {
        throw "Installer left a ProgramData staging directory after log handoff: $(@($installerStageResidue.FullName) -join ', ')"
    }

    $daemonPath = Join-Path $installRoot "boundlessd.exe"
    $servicePath = Join-Path $installRoot "boundless-service.exe"
    $cliPath = Join-Path $installRoot "boundlessctl.exe"
    $trayPath = Join-Path $installRoot "boundlesstray.exe"
    $inputInjectorPath = Join-Path $installRoot "boundless-input-injector.exe"

    Assert-PathExists -Path $daemonPath -Message "Installed daemon binary is missing."
    Assert-PathExists -Path $servicePath -Message "Installed service binary is missing."
    Assert-PathExists -Path $cliPath -Message "Installed CLI binary is missing."
    Assert-PathExists -Path $trayPath -Message "Installed tray binary is missing."
    Assert-PathExists -Path $inputInjectorPath -Message "Installed elevated input injector binary is missing."
    Assert-PathExists -Path $resetScriptPath -Message "Installed reset helper is missing."
    Assert-PathExists -Path $iconPath -Message "Installed icon asset is missing."
    Assert-PathExists -Path $startMenuShortcutPath -Message "Start menu shortcut is missing."
    Assert-PathExists -Path $desktopShortcutPath -Message "Desktop shortcut is missing."
    Assert-PathMissing -Path $currentUserStartupShortcutPath -Message "Installer created a current-user Startup shortcut, but tray startup is deferred in the 9B-2 machine-wide skeleton."
    Assert-PathMissing -Path $commonStartupShortcutPath -Message "Installer created a common Startup shortcut, but tray startup is deferred in the 9B-2 machine-wide skeleton."
    $serviceInstallConfig = Assert-BoundlessServiceConfig -ExpectedServicePath $servicePath -ExpectedAllowedUserSid $AllowedUserSid
    Wait-BoundlessServiceStatus -ExpectedStatus "Running" | Out-Null
    $serviceDaemonStatusOutput = Wait-ForDaemonReady -CliPath $cliPath
    $currentAppPayloadEvidence = Get-FileEvidence -Path $trayPath -Label "Current installer tray executable"
    $currentServicePayloadEvidence = Get-FileEvidence -Path $servicePath -Label "Current installer service executable"
    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
        $upgradePayloadReplacement = [ordered]@{
            previous_app_payload = $previousAppPayloadEvidence
            current_app_payload = $currentAppPayloadEvidence
            previous_service_payload = $previousServicePayloadEvidence
            current_service_payload = $currentServicePayloadEvidence
            app_payload_replaced = ($previousAppPayloadEvidence.sha256 -ne $currentAppPayloadEvidence.sha256)
            service_payload_replaced = ($previousServicePayloadEvidence.sha256 -ne $currentServicePayloadEvidence.sha256)
            current_payload_owned_by_program_files = (Test-IsUnderPath -Path $currentAppPayloadEvidence.path -Root $env:ProgramFiles)
            current_service_payload_owned_by_program_files = (Test-IsUnderPath -Path $currentServicePayloadEvidence.path -Root $env:ProgramFiles)
            current_active_service_uses_program_files_payload = ($serviceInstallConfig.path_name -match [regex]::Escape($servicePath))
        }
        if (-not $upgradePayloadReplacement.app_payload_replaced) {
            throw "N-1 upgrade did not replace the tray payload. Previous and current SHA-256 both $($currentAppPayloadEvidence.sha256)."
        }
        if (-not $upgradePayloadReplacement.service_payload_replaced) {
            throw "N-1 upgrade did not replace the service payload. Previous and current SHA-256 both $($currentServicePayloadEvidence.sha256)."
        }
        if (-not $upgradePayloadReplacement.current_payload_owned_by_program_files) {
            throw "Current app payload is not under Program Files after N-1 upgrade: $($currentAppPayloadEvidence.path)"
        }
        if (-not $upgradePayloadReplacement.current_service_payload_owned_by_program_files) {
            throw "Current service payload is not under Program Files after N-1 upgrade: $($currentServicePayloadEvidence.path)"
        }
        if (-not $upgradePayloadReplacement.current_active_service_uses_program_files_payload) {
            throw "Active BoundlessService does not use the current Program Files service payload after N-1 upgrade."
        }
    }

    foreach ($shortcutPath in @($startMenuShortcutPath, $desktopShortcutPath)) {
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
    if ($uninstallEntry.RegistryRoot -ne "HKLM") {
        throw "Boundless uninstall entry was expected under HKLM but was found under $($uninstallEntry.RegistryRoot)."
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
    $installerEvidence = Get-InstallerEvidence

    $traySignature = Assert-Authenticode -Path $trayPath -Required:$RequireSignature.IsPresent
    $daemonSignature = Assert-Authenticode -Path $daemonPath -Required:$RequireSignature.IsPresent
    $serviceSignature = Assert-Authenticode -Path $servicePath -Required:$RequireSignature.IsPresent
    $cliSignature = Assert-Authenticode -Path $cliPath -Required:$RequireSignature.IsPresent
    $inputInjectorSignature = Assert-Authenticode -Path $inputInjectorPath -Required:$RequireSignature.IsPresent

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

    $serviceVersionOutput = (& $servicePath --version 2>&1 | Out-String).Trim()
    $serviceVersionExitCode = $LASTEXITCODE
    if ($serviceVersionExitCode -ne 0) {
        throw "Installed service executable failed to report its version. Exit code: $serviceVersionExitCode."
    }
    if (
        -not [string]::IsNullOrWhiteSpace($expectedDisplayVersion) -and
        -not [string]::IsNullOrWhiteSpace($serviceVersionOutput) -and
        $serviceVersionOutput -notmatch [regex]::Escape($expectedDisplayVersion)
    ) {
        throw "Installed service executable reported an unexpected version string: $serviceVersionOutput"
    }

    $inputInjectorVersionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($inputInjectorPath)
    $inputInjectorProductVersion = $inputInjectorVersionInfo.ProductVersion
    if ([string]::IsNullOrWhiteSpace($inputInjectorProductVersion)) {
        throw "Installed input injector executable did not carry ProductVersion metadata."
    }
    if (
        -not [string]::IsNullOrWhiteSpace($expectedDisplayVersion) -and
        $inputInjectorProductVersion -ne $expectedDisplayVersion
    ) {
        throw "Installed input injector ProductVersion '$inputInjectorProductVersion' did not match MSI version '$expectedDisplayVersion'."
    }
    $inputInjectorExecutionManifest = Assert-InputInjectorExecutionManifest -Path $inputInjectorPath

    $trayLaunchMode = if ($interactiveDesktopSession) { "interactive_desktop" } else { "headless_session" }
    $trayExitedEarly = $false
    $trayExitCode = $null
    $daemonReadyOutput = $null
    if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath) -and $interactiveDesktopSession) {
        $upgradeWhileRunningTested = $true
        $currentSessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
        $postUpgradeTrayCount = Get-BoundlessProcessCountForSession -Name "boundlesstray" -SessionId $currentSessionId
        if ($postUpgradeTrayCount -eq 0) {
            $postUpgradeTrayProcess = Start-Process -FilePath $trayPath -WorkingDirectory $installRoot -PassThru
            Start-Sleep -Seconds 3
            if ($postUpgradeTrayProcess.HasExited) {
                throw "Current tray exited before post-upgrade smoke could begin. Exit code: $($postUpgradeTrayProcess.ExitCode)"
            }
            $postUpgradeTrayRelaunched = $true
        }
        elseif ($postUpgradeTrayCount -gt 1) {
            throw "Expected at most one boundlesstray.exe immediately after upgrade, found $postUpgradeTrayCount."
        }
        $runtimePresence = Wait-ForRuntimePresence
        $upgradeDaemonStatus = "tray_count=$($runtimePresence.TrayCount) daemon_count=$($runtimePresence.DaemonCount) pipe_present=$($runtimePresence.PipePresent)"
        Start-Sleep -Seconds 3
        $postUpgradeTrayCount = Get-BoundlessProcessCount -Name "boundlesstray"
        $postUpgradeDaemonCount = Get-BoundlessDaemonRuntimeCount
        if ($postUpgradeTrayCount -ne 1) {
            throw "Expected exactly one boundlesstray.exe after upgrade-while-running smoke, found $postUpgradeTrayCount."
        }
        if ($postUpgradeDaemonCount -ne 1) {
            throw "Expected exactly one Boundless daemon runtime after upgrade-while-running smoke, found $postUpgradeDaemonCount."
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

    $inputInjectorCountAfterTrayLaunch = Get-BoundlessProcessCount -Name "boundless-input-injector"
    if ($inputInjectorCountAfterTrayLaunch -ne 0) {
        throw "Tray startup launched $inputInjectorCountAfterTrayLaunch elevated input injector process(es) without an explicit user action."
    }

    if ($interactiveDesktopSession) {
        $currentSessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
        $trayCurrentSessionCount = Get-BoundlessProcessCountForSession -Name "boundlesstray" -SessionId $currentSessionId
        if ($trayCurrentSessionCount -ne 1) {
            throw "Expected one boundlesstray.exe before single-instance smoke in session $currentSessionId, found $trayCurrentSessionCount."
        }

        $secondTrayProcess = Start-Process -FilePath $trayPath -WorkingDirectory $installRoot -PassThru
        if (-not $secondTrayProcess.WaitForExit(10000)) {
            throw "Second tray launch did not exit within 10 seconds. PID: $($secondTrayProcess.Id)"
        }
        $traySecondLaunchExitCode = $secondTrayProcess.ExitCode
        if ($traySecondLaunchExitCode -ne 0) {
            throw "Second tray launch failed instead of activating the existing tray. Exit code: $traySecondLaunchExitCode"
        }

        Start-Sleep -Milliseconds 500
        $trayCurrentSessionCount = Get-BoundlessProcessCountForSession -Name "boundlesstray" -SessionId $currentSessionId
        if ($trayCurrentSessionCount -ne 1) {
            throw "Expected exactly one boundlesstray.exe after repeated launch in session $currentSessionId, found $trayCurrentSessionCount."
        }
        $null = Wait-ForDaemonReady -CliPath $cliPath
        $traySingleInstanceTested = $true

        $quitStopwatch = [Diagnostics.Stopwatch]::StartNew()
        $quitControlArgs = @{
            FilePath = $trayPath
            ArgumentList = "--quit"
            WorkingDirectory = $installRoot
            PassThru = $true
        }
        $quitControlProcess = Start-Process @quitControlArgs
        if (-not $quitControlProcess.WaitForExit(5000)) {
            throw "Tray --quit control process did not exit within 5 seconds. PID: $($quitControlProcess.Id)"
        }
        $trayQuitControlExitCode = $quitControlProcess.ExitCode
        if ($trayQuitControlExitCode -ne 0) {
            throw "Tray --quit control process failed. Exit code: $trayQuitControlExitCode"
        }
        $waitForQuitArgs = @{
            Name = "boundlesstray"
            SessionId = $currentSessionId
            ExpectedCount = 0
            TimeoutSeconds = 8
        }
        $null = Wait-BoundlessProcessCountForSession @waitForQuitArgs
        $quitStopwatch.Stop()
        $trayQuitElapsedMilliseconds = $quitStopwatch.ElapsedMilliseconds
        $trayGracefulQuitTested = $true

        $postQuitTrayArgs = @{
            FilePath = $trayPath
            WorkingDirectory = $installRoot
            PassThru = $true
        }
        $postQuitTrayProcess = Start-Process @postQuitTrayArgs
        Start-Sleep -Milliseconds 500
        if ($postQuitTrayProcess.HasExited) {
            throw "Tray exited immediately after graceful Quit/relaunch smoke. Exit code: $($postQuitTrayProcess.ExitCode)"
        }
        $null = Wait-ForDaemonReady -CliPath $cliPath
        $waitForRelaunchArgs = @{
            Name = "boundlesstray"
            SessionId = $currentSessionId
            ExpectedCount = 1
        }
        $trayCurrentSessionCount = Wait-BoundlessProcessCountForSession @waitForRelaunchArgs
    }

    $repairServiceDeleteOutput = Remove-BoundlessServiceRegistrationForRepair
    $repairExitCode = Invoke-MsiExec -ArgumentList (@("/i", $InstallerPath) + $msiInstallProperties + @("REINSTALL=ALL", "REINSTALLMODE=amus", "/qn", "/norestart")) -LogPath $repairLog
    $repairServiceConfig = Assert-BoundlessServiceConfig -ExpectedServicePath $servicePath -ExpectedAllowedUserSid $AllowedUserSid
    Wait-BoundlessServiceStatus -ExpectedStatus "Running" | Out-Null
    $repairDaemonStatusOutput = Wait-ForDaemonReady -CliPath $cliPath
    $serviceRunningAfterRepair = ((Get-BoundlessService).Status.ToString() -eq "Running")
    $inputInjectorCountAfterRepair = Get-BoundlessProcessCount -Name "boundless-input-injector"
    if ($inputInjectorCountAfterRepair -ne 0) {
        throw "MSI repair left or launched $inputInjectorCountAfterRepair elevated input injector process(es)."
    }

    $serviceRunningBeforeUninstall = (Get-BoundlessService).Status.ToString() -eq "Running"

    $uninstallExitCode = Invoke-MsiExec -ArgumentList @("/x", $InstallerPath, "/qn", "/norestart") -LogPath $uninstallLog

    Wait-ForNoBoundlessProcesses
    $inputInjectorCountAfterUninstall = Get-BoundlessProcessCount -Name "boundless-input-injector"
    Wait-ForPathRemoval -Path $installRoot
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
    if (Test-Path -LiteralPath $servicePath) {
        throw "Uninstall left the Program Files service binary: $servicePath"
    }
    if (Test-Path -LiteralPath $inputInjectorPath) {
        throw "Uninstall left the Program Files elevated input injector binary: $inputInjectorPath"
    }
    if (Test-InstallerEvidencePresent) {
        throw "Uninstall left machine-wide installer evidence under HKLM\Software\Boundless\Installer."
    }

    $summary = [ordered]@{
        installer_path = $InstallerPath
        install_root = $installRoot
        installer_registry_evidence = $installerEvidence
        uninstall_registry_root = $uninstallEntry.RegistryRoot
        tray_startup_policy = "deferred_no_startup_shortcut"
        allowed_user_sid = $AllowedUserSid
        allowed_user_sid_source = $allowedUserSidSource
        service_install_config = $serviceInstallConfig
        service_daemon_status_output = $serviceDaemonStatusOutput
        service_running_before_uninstall = $serviceRunningBeforeUninstall
        repair_tested = $true
        repair_exit_code = $repairExitCode
        repair_service_delete_output = $repairServiceDeleteOutput
        repair_service_config = $repairServiceConfig
        repair_daemon_status_output = $repairDaemonStatusOutput
        service_running_after_repair = $serviceRunningAfterRepair
        installer_signature = $installerSignature
        tray_signature = $traySignature
        daemon_signature = $daemonSignature
        service_signature = $serviceSignature
        cli_signature = $cliSignature
        input_injector_path = $inputInjectorPath
        input_injector_signature = $inputInjectorSignature
        tray_version_output = $trayVersionOutput
        tray_version_exit_code = $trayVersionExitCode
        service_version_output = $serviceVersionOutput
        service_version_exit_code = $serviceVersionExitCode
        input_injector_product_version = $inputInjectorProductVersion
        input_injector_execution_level = $inputInjectorExecutionManifest.execution_level
        input_injector_ui_access = $inputInjectorExecutionManifest.ui_access
        tray_launch_mode = $trayLaunchMode
        tray_exited_early = $trayExitedEarly
        tray_exit_code = $trayExitCode
        tray_single_instance_tested = $traySingleInstanceTested
        tray_second_launch_exit_code = $traySecondLaunchExitCode
        tray_current_session_count = $trayCurrentSessionCount
        tray_graceful_quit_tested = $trayGracefulQuitTested
        tray_quit_control_exit_code = $trayQuitControlExitCode
        tray_quit_elapsed_milliseconds = $trayQuitElapsedMilliseconds
        daemon_ready_output = $daemonReadyOutput
        upgraded_from = $PreviousInstallerPath
        previous_install_root = $previousInstallRoot
        previous_uninstall_registry_root = $previousUninstallRegistryRoot
        previous_app_payload = $previousAppPayloadEvidence
        previous_service_payload = $previousServicePayloadEvidence
        previous_service_install_config = $previousServiceInstallConfig
        current_app_payload = $currentAppPayloadEvidence
        current_service_payload = $currentServicePayloadEvidence
        upgrade_payload_replacement = $upgradePayloadReplacement
        previous_install_exit_code = $upgradeInstallExitCode
        install_exit_code = $installExitCode
        install_helper_upgrade_evidence = $installHelperUpgradeEvidence
        uninstall_exit_code = $uninstallExitCode
        upgrade_while_running_tested = $upgradeWhileRunningTested
        upgrade_while_running_skipped_reason = $upgradeWhileRunningSkippedReason
        post_upgrade_tray_relaunched = $postUpgradeTrayRelaunched
        upgrade_daemon_status = $upgradeDaemonStatus
        post_upgrade_tray_count = $postUpgradeTrayCount
        post_upgrade_daemon_count = $postUpgradeDaemonCount
        input_injector_count_after_tray_launch = $inputInjectorCountAfterTrayLaunch
        input_injector_count_after_repair = $inputInjectorCountAfterRepair
        input_injector_count_after_uninstall = $inputInjectorCountAfterUninstall
        post_uninstall_processes_cleared = $true
        post_uninstall_service_removed = $true
        post_uninstall_program_files_root_removed = $true
        post_uninstall_service_binary_removed = $true
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
