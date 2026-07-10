[CmdletBinding()]
param(
    [string]$InstallerPath = "",
    [string]$AllowedUserSid = "",
    [string]$AllowedUserName = "",
    [switch]$UseCurrentUserWhenElevated,
    [switch]$Quiet,
    [switch]$NoRestart,
    [string]$LogPath = "",
    [switch]$ResolveOnly,
    [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Assert-AllowedUserSid {
    param([string]$Sid)

    if ([string]::IsNullOrWhiteSpace($Sid)) {
        throw "Allowed user SID was empty."
    }

    if ($Sid -notmatch '^S-1-\d+(?:-\d+)+$') {
        throw "Allowed user SID must be a strict numeric SID such as S-1-5-21-... Got: $Sid"
    }
}

function Resolve-AccountSid {
    param([string]$AccountName)

    if ([string]::IsNullOrWhiteSpace($AccountName)) {
        throw "Allowed user name was empty."
    }

    try {
        $account = [Security.Principal.NTAccount]::new($AccountName)
        return $account.Translate([Security.Principal.SecurityIdentifier]).Value
    }
    catch {
        throw "Could not resolve Windows account '$AccountName' to a SID. Use DOMAIN\user format or pass -AllowedUserSid explicitly. $($_.Exception.Message)"
    }
}

function Resolve-AccountNameFromSid {
    param([string]$Sid)

    try {
        $securityIdentifier = [Security.Principal.SecurityIdentifier]::new($Sid)
        return $securityIdentifier.Translate([Security.Principal.NTAccount]).Value
    }
    catch {
        return ""
    }
}

function Resolve-AllowedUser {
    if (-not [string]::IsNullOrWhiteSpace($AllowedUserSid) -and -not [string]::IsNullOrWhiteSpace($AllowedUserName)) {
        throw "Pass either -AllowedUserSid or -AllowedUserName, not both."
    }

    if (-not [string]::IsNullOrWhiteSpace($AllowedUserSid)) {
        Assert-AllowedUserSid -Sid $AllowedUserSid
        return [pscustomobject]@{
            sid = $AllowedUserSid
            account = Resolve-AccountNameFromSid -Sid $AllowedUserSid
            source = "explicit_sid"
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($AllowedUserName)) {
        $sid = Resolve-AccountSid -AccountName $AllowedUserName
        Assert-AllowedUserSid -Sid $sid
        return [pscustomobject]@{
            sid = $sid
            account = $AllowedUserName
            source = "explicit_account"
        }
    }

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $isElevated = Test-IsAdministrator
    if ($isElevated -and -not $UseCurrentUserWhenElevated) {
        throw "Refusing to infer the allowed user from an already-elevated shell. Run this helper from the intended desktop user's normal PowerShell so it can capture that SID before UAC, or pass -AllowedUserSid for the intended user. Use -UseCurrentUserWhenElevated only when the elevated account is intentionally the desktop user to authorize."
    }

    $source = if ($isElevated) {
        "current_elevated_user_explicitly_allowed"
    }
    else {
        "current_unelevated_user"
    }

    return [pscustomobject]@{
        sid = $identity.User.Value
        account = $identity.Name
        source = $source
    }
}

function Resolve-InstallerPath {
    if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
        if (-not (Test-Path -LiteralPath $InstallerPath)) {
            throw "InstallerPath was not found: $InstallerPath"
        }

        return (Resolve-Path -LiteralPath $InstallerPath).Path
    }

    $scriptRoot = if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
        (Resolve-Path ".").Path
    }
    else {
        $PSScriptRoot
    }

    $candidates = @(Get-ChildItem -LiteralPath $scriptRoot -Filter "Boundless-*-windows-x64.msi" -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -eq 0) {
        throw "No Boundless Windows MSI was found next to this helper. Pass -InstallerPath <path-to-msi>."
    }
    if ($candidates.Count -gt 1) {
        $names = @($candidates | Select-Object -ExpandProperty Name) -join ", "
        throw "Multiple Boundless Windows MSI files were found next to this helper. Pass -InstallerPath explicitly. Found: $names"
    }

    return $candidates[0].FullName
}

function ConvertTo-ProcessArgument {
    param([string]$Value)

    if ($Value -notmatch '[\s"]') {
        return $Value
    }

    return '"' + ($Value -replace '"', '\"') + '"'
}

function Invoke-BoundlessMsi {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid
    )

    $arguments = @(
        "/i",
        $ResolvedInstallerPath,
        "BOUNDLESS_ALLOWED_USER_SID=$Sid"
    )

    if ($Quiet) {
        $arguments += "/qn"
    }
    if ($NoRestart) {
        $arguments += "/norestart"
    }
    if (-not [string]::IsNullOrWhiteSpace($LogPath)) {
        $resolvedLogPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogPath)
        $logParent = Split-Path -Parent $resolvedLogPath
        if (-not [string]::IsNullOrWhiteSpace($logParent)) {
            New-Item -ItemType Directory -Force -Path $logParent | Out-Null
        }
        $arguments += @("/l*v", $resolvedLogPath)
    }

    $argumentLine = @($arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " "
    $startArgs = @{
        FilePath = "msiexec.exe"
        ArgumentList = $argumentLine
        Wait = $true
        PassThru = $true
    }

    if (-not (Test-IsAdministrator)) {
        $startArgs.Verb = "RunAs"
    }

    $process = Start-Process @startArgs
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "msiexec.exe failed with exit code $($process.ExitCode)."
    }

    return $process.ExitCode
}

function Get-MsiProperty {
    param(
        [string]$Path,
        [string]$Property
    )

    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $installer.GetType().InvokeMember(
        "OpenDatabase",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $installer,
        @($Path, 0)
    )
    $escapedProperty = $Property.Replace("'", "''")
    $view = $database.GetType().InvokeMember(
        "OpenView",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $database,
        @("SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$escapedProperty'")
    )
    $view.GetType().InvokeMember(
        "Execute",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $view,
        $null
    ) | Out-Null
    $record = $view.GetType().InvokeMember(
        "Fetch",
        [System.Reflection.BindingFlags]::InvokeMethod,
        $null,
        $view,
        $null
    )
    if ($null -eq $record) {
        throw "MSI property '$Property' was not found in $Path"
    }
    return $record.StringData(1)
}

function Get-BoundlessUninstallEntry {
    param([string]$ProductCode)

    foreach ($path in @(
        "Registry::HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductCode",
        "Registry::HKEY_LOCAL_MACHINE\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\$ProductCode"
    )) {
        $entry = Get-ItemProperty -LiteralPath $path -ErrorAction SilentlyContinue
        if ($null -ne $entry) {
            return $entry
        }
    }
    return $null
}

function Invoke-BoundedProcess {
    param(
        [string]$FilePath,
        [string[]]$ArgumentList,
        [int]$TimeoutSeconds = 10
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = @($ArgumentList | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " "
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Failed to start $FilePath."
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw "$FilePath did not exit within $($TimeoutSeconds)s."
        }
        $process.WaitForExit()

        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $exitCode = $process.ExitCode
        return [pscustomobject]@{
            exit_code = $exitCode
            stdout = $stdout.Trim()
            stderr = $stderr.Trim()
        }
    }
    finally {
        $process.Dispose()
    }
}

function Wait-BoundlessServiceRunning {
    param([int]$TimeoutSeconds = 30)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
        if ($null -ne $service -and $service.Status.ToString() -eq "Running") {
            return $service
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    if ($null -eq $service) {
        throw "BoundlessService was not registered after installation."
    }
    throw "BoundlessService did not reach Running within $($TimeoutSeconds)s; current=$($service.Status)."
}

function ConvertFrom-BoundlessDaemonStatusOutput {
    param(
        [string]$Output,
        [string]$ExpectedVersion
    )

    $running = $Output -match '(^|\s)running=true(\s|$)'
    $versionMatch = [regex]::Match($Output, '(^|\s)daemon_version=(?<version>[^\s]+)(\s|$)')
    $reportedVersion = if ($versionMatch.Success) {
        $versionMatch.Groups['version'].Value
    }
    else {
        ""
    }

    return [pscustomobject]@{
        running = $running
        reported_version = $reportedVersion
        expected_version = $ExpectedVersion
        healthy = $running -and $reportedVersion -eq $ExpectedVersion
    }
}

function Wait-BoundlessDaemonApi {
    param(
        [string]$CliPath,
        [string]$ExpectedVersion,
        [int]$TimeoutSeconds = 30
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $lastResult = $null
    $lastStatus = $null
    do {
        $lastResult = Invoke-BoundedProcess -FilePath $CliPath -ArgumentList @("daemon", "status") -TimeoutSeconds 5
        if ($lastResult.exit_code -eq 0) {
            $lastStatus = ConvertFrom-BoundlessDaemonStatusOutput `
                -Output $lastResult.stdout `
                -ExpectedVersion $ExpectedVersion
            if ($lastStatus.healthy) {
                return $lastStatus
            }
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    $detail = if ($null -eq $lastResult) {
        "no status attempt completed"
    }
    else {
        $reportedVersion = if ($null -eq $lastStatus -or [string]::IsNullOrWhiteSpace($lastStatus.reported_version)) {
            "missing"
        }
        else {
            $lastStatus.reported_version
        }
        "exit_code=$($lastResult.exit_code) reported_version=$reportedVersion expected_version=$ExpectedVersion stderr=$($lastResult.stderr)"
    }
    throw "Boundless daemon API did not become healthy within $($TimeoutSeconds)s; $detail"
}

function Get-BoundlessVersionFromOutput {
    param(
        [string]$Output,
        [string]$ExecutableName
    )

    $match = [regex]::Match(
        $Output,
        "(?m)^\s*$([regex]::Escape($ExecutableName))\s+(?<version>[^\s]+)\s*$"
    )
    if (-not $match.Success) {
        throw "$ExecutableName --version returned an unexpected value: '$Output'"
    }
    return $match.Groups['version'].Value
}

function Get-BoundlessExecutableVersion {
    param(
        [string]$Path,
        [string]$ExecutableName,
        [int]$TimeoutSeconds = 10
    )

    $result = Invoke-BoundedProcess -FilePath $Path -ArgumentList @("--version") -TimeoutSeconds $TimeoutSeconds
    if ($result.exit_code -ne 0) {
        throw "$ExecutableName --version failed with exit code $($result.exit_code): $($result.stderr)"
    }
    return Get-BoundlessVersionFromOutput -Output $result.stdout -ExecutableName $ExecutableName
}

function Get-BoundlessTrayProcessesForCurrentSession {
    $sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    return @(
        Get-Process -Name "boundlesstray" -ErrorAction SilentlyContinue |
            Where-Object { $_.SessionId -eq $sessionId } |
            ForEach-Object {
                $path = try { $_.Path } catch { "" }
                $responding = try { $_.Responding } catch { $false }
                [pscustomobject]@{
                    id = $_.Id
                    path = $path
                    responding = $responding
                }
            }
    )
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

function Assert-SoleBoundlessTraySnapshot {
    param(
        [object[]]$Processes,
        [string]$ExpectedTrayPath,
        [string]$Phase
    )

    $processes = @($Processes)
    if ($processes.Count -gt 1) {
        throw "Expected at most one Boundless tray $Phase, found $($processes.Count) in the current session."
    }
    if ($processes.Count -eq 0) {
        return $null
    }

    $process = $processes[0]
    if (-not (Test-WindowsPathEqual -Left $process.path -Right $ExpectedTrayPath)) {
        throw "Boundless tray $Phase was running from an unexpected path. Expected '$ExpectedTrayPath', got '$($process.path)'. Close the old or portable tray and retry."
    }
    return $process
}

function Ensure-OneBoundlessTray {
    param(
        [string]$TrayPath,
        [string]$InstallRoot,
        [int]$TimeoutSeconds = 15,
        [int]$StableMilliseconds = 2000
    )

    $expectedTrayPath = [IO.Path]::GetFullPath($TrayPath)
    $existing = Assert-SoleBoundlessTraySnapshot `
        -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
        -ExpectedTrayPath $expectedTrayPath `
        -Phase "before launch"
    $launchedProcess = $null
    if ($null -eq $existing) {
        $launchedProcess = Start-Process -FilePath $expectedTrayPath -WorkingDirectory $InstallRoot -PassThru
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $stableSince = $null
    $stableProcessId = $null
    do {
        $process = Assert-SoleBoundlessTraySnapshot `
            -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
            -ExpectedTrayPath $expectedTrayPath `
            -Phase "during readiness verification"
        if ($null -ne $process -and $process.responding) {
            if ($stableProcessId -ne $process.id) {
                $stableProcessId = $process.id
                $stableSince = Get-Date
            }
            $stableFor = [int]((Get-Date) - $stableSince).TotalMilliseconds
            if ($stableFor -ge $StableMilliseconds) {
                return [pscustomobject]@{
                    count = 1
                    process_id = $process.id
                    path = $process.path
                    path_matches = $true
                    responding = $true
                    stable_milliseconds = $stableFor
                }
            }
        }
        else {
            $stableProcessId = $null
            $stableSince = $null
            if ($null -ne $launchedProcess -and $launchedProcess.HasExited) {
                throw "Boundless tray exited before it remained ready for $($StableMilliseconds)ms; exit_code=$($launchedProcess.ExitCode)."
            }
        }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $deadline)

    throw "Boundless tray did not remain single, responsive, and path-correct for $($StableMilliseconds)ms within $($TimeoutSeconds)s."
}

function Test-ManifestVersionMatchesMsi {
    param(
        [string]$ManifestVersion,
        [string]$MsiVersion
    )

    return $ManifestVersion -eq $MsiVersion -or
        $ManifestVersion.StartsWith("$MsiVersion-") -or
        $ManifestVersion.StartsWith("$MsiVersion+")
}

function Assert-PostInstallEvidence {
    param([object]$Evidence)

    if (-not $Evidence.product_registered) {
        throw "Windows Installer did not register the MSI product after reporting success."
    }
    if ($Evidence.display_version -ne $Evidence.msi_version) {
        throw "Installed DisplayVersion '$($Evidence.display_version)' did not match MSI ProductVersion '$($Evidence.msi_version)'."
    }
    if (-not (Test-ManifestVersionMatchesMsi -ManifestVersion $Evidence.manifest_version -MsiVersion $Evidence.msi_version)) {
        throw "Installed package-manifest version '$($Evidence.manifest_version)' did not match MSI ProductVersion '$($Evidence.msi_version)'."
    }
    if ($Evidence.service_allowed_user_sid -ne $Evidence.expected_allowed_user_sid) {
        throw "BoundlessService allowed-user SID mismatch. Expected $($Evidence.expected_allowed_user_sid), got $($Evidence.service_allowed_user_sid)."
    }
    if (-not $Evidence.service_binary_path_matches) {
        throw "BoundlessService command line did not reference the installed Program Files service binary."
    }
    if ($Evidence.service_status -ne "Running") {
        throw "BoundlessService was not Running after install; current=$($Evidence.service_status)."
    }
    if (-not $Evidence.daemon_api_healthy) {
        throw "Boundless daemon API was not healthy after install."
    }
    if ($Evidence.daemon_runtime_version -ne $Evidence.expected_runtime_version) {
        throw "Boundless daemon runtime version '$($Evidence.daemon_runtime_version)' did not match installed version '$($Evidence.expected_runtime_version)'."
    }
    if (-not $Evidence.executable_versions_match) {
        throw "One or more installed Boundless executables did not report the installed package version."
    }
    if ($Evidence.tray_verification -eq "passed" -and $Evidence.tray_count -ne 1) {
        throw "Expected exactly one Boundless tray after install, found $($Evidence.tray_count)."
    }
    if ($Evidence.tray_verification -eq "passed" -and -not $Evidence.tray_path_matches) {
        throw "The sole Boundless tray did not run from the installed Program Files path."
    }
    if ($Evidence.tray_verification -eq "passed" -and -not $Evidence.tray_responding) {
        throw "The sole Boundless tray did not remain responsive during readiness verification."
    }
    if ($Evidence.tray_verification -eq "passed" -and $Evidence.tray_stable_milliseconds -lt 2000) {
        throw "The sole Boundless tray was not stable for the required 2000ms readiness interval."
    }
    if ($Evidence.tray_verification -notin @("passed", "deferred_elevated_or_quiet")) {
        throw "Unexpected tray verification status '$($Evidence.tray_verification)'."
    }
    return $Evidence
}

function Invoke-PostInstallVerification {
    param(
        [string]$ResolvedInstallerPath,
        [string]$ExpectedAllowedUserSid,
        [bool]$LaunchTray
    )

    $msiVersion = Get-MsiProperty -Path $ResolvedInstallerPath -Property "ProductVersion"
    $productCode = Get-MsiProperty -Path $ResolvedInstallerPath -Property "ProductCode"
    $uninstallEntry = Get-BoundlessUninstallEntry -ProductCode $productCode
    $productRegistered = $null -ne $uninstallEntry
    if (-not $productRegistered) {
        throw "Windows Installer product $productCode was not registered after msiexec reported success."
    }

    $installRoot = if (
        $uninstallEntry.PSObject.Properties.Match("InstallLocation").Count -gt 0 -and
        -not [string]::IsNullOrWhiteSpace($uninstallEntry.InstallLocation)
    ) {
        $uninstallEntry.InstallLocation
    }
    else {
        Join-Path $env:ProgramFiles "Boundless"
    }
    $manifestPath = Join-Path $installRoot "package-manifest.json"
    if (-not (Test-Path -LiteralPath $manifestPath)) {
        throw "Installed package manifest was missing: $manifestPath"
    }
    $manifestVersion = (Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json).version

    $service = Wait-BoundlessServiceRunning
    $serviceConfig = Get-CimInstance -ClassName Win32_Service -Filter "Name='BoundlessService'" -ErrorAction Stop |
        Select-Object -First 1
    if ($null -eq $serviceConfig) {
        throw "BoundlessService configuration was unavailable after install."
    }
    $sidMatches = [regex]::Matches($serviceConfig.PathName, "--allowed-user-sid=([^\s]+)")
    if ($sidMatches.Count -ne 1) {
        throw "BoundlessService command line did not contain exactly one --allowed-user-sid argument. PathName=$($serviceConfig.PathName)"
    }
    $serviceAllowedUserSid = $sidMatches[0].Groups[1].Value.Trim('"')
    $expectedServicePath = Join-Path $installRoot "boundless-service.exe"
    $serviceBinaryPathMatches = $serviceConfig.PathName -match [regex]::Escape($expectedServicePath)

    $cliPath = Join-Path $installRoot "boundlessctl.exe"
    $trayPath = Join-Path $installRoot "boundlesstray.exe"
    $daemonPath = Join-Path $installRoot "boundlessd.exe"
    $servicePath = Join-Path $installRoot "boundless-service.exe"
    foreach ($requiredPath in @($cliPath, $trayPath, $daemonPath, $servicePath)) {
        if (-not (Test-Path -LiteralPath $requiredPath)) {
            throw "Installed Boundless payload was missing: $requiredPath"
        }
    }

    $reportedExecutableVersions = [ordered]@{
        boundlessctl = Get-BoundlessExecutableVersion -Path $cliPath -ExecutableName "boundlessctl"
        boundlesstray = Get-BoundlessExecutableVersion -Path $trayPath -ExecutableName "boundlesstray"
        boundlessd = Get-BoundlessExecutableVersion -Path $daemonPath -ExecutableName "boundlessd"
        boundless_service = Get-BoundlessExecutableVersion -Path $servicePath -ExecutableName "boundless-service"
    }
    $executableVersionsMatch = @($reportedExecutableVersions.Values | Where-Object { $_ -ne $manifestVersion }).Count -eq 0

    $daemonApi = Wait-BoundlessDaemonApi -CliPath $cliPath -ExpectedVersion $manifestVersion
    if ($LaunchTray) {
        $trayEvidence = Ensure-OneBoundlessTray -TrayPath $trayPath -InstallRoot $installRoot
        $trayCount = $trayEvidence.count
        $trayPathMatches = $trayEvidence.path_matches
        $trayResponding = $trayEvidence.responding
        $trayStableMilliseconds = $trayEvidence.stable_milliseconds
        $trayVerification = "passed"
    }
    else {
        $existingTray = Assert-SoleBoundlessTraySnapshot `
            -Processes @(Get-BoundlessTrayProcessesForCurrentSession) `
            -ExpectedTrayPath $trayPath `
            -Phase "during deferred verification"
        $trayCount = if ($null -eq $existingTray) { 0 } else { 1 }
        $trayPathMatches = $null -ne $existingTray
        $trayResponding = $null -ne $existingTray -and $existingTray.responding
        $trayStableMilliseconds = 0
        $trayVerification = "deferred_elevated_or_quiet"
    }

    $evidence = [pscustomobject]@{
        product_registered = $productRegistered
        product_code = $productCode
        msi_version = $msiVersion
        display_version = $uninstallEntry.DisplayVersion
        manifest_version = $manifestVersion
        service_allowed_user_sid = $serviceAllowedUserSid
        expected_allowed_user_sid = $ExpectedAllowedUserSid
        service_binary_path_matches = $serviceBinaryPathMatches
        service_status = $service.Status.ToString()
        daemon_api_healthy = $daemonApi.healthy
        daemon_runtime_version = $daemonApi.reported_version
        expected_runtime_version = $manifestVersion
        executable_versions_match = $executableVersionsMatch
        executable_versions = $reportedExecutableVersions
        tray_count = $trayCount
        tray_path_matches = $trayPathMatches
        tray_responding = $trayResponding
        tray_stable_milliseconds = $trayStableMilliseconds
        tray_verification = $trayVerification
    }
    return Assert-PostInstallEvidence -Evidence $evidence
}

function Invoke-InstallHelperSelfTest {
    $validSid = "S-1-5-21-1-2-3-1001"
    $valid = [pscustomobject]@{
        product_registered = $true
        msi_version = "5.0.13"
        display_version = "5.0.13"
        manifest_version = "5.0.13-dogfood.1"
        service_allowed_user_sid = $validSid
        expected_allowed_user_sid = $validSid
        service_binary_path_matches = $true
        service_status = "Running"
        daemon_api_healthy = $true
        daemon_runtime_version = "5.0.13-dogfood.1"
        expected_runtime_version = "5.0.13-dogfood.1"
        executable_versions_match = $true
        tray_count = 1
        tray_path_matches = $true
        tray_responding = $true
        tray_stable_milliseconds = 2000
        tray_verification = "passed"
    }
    Assert-PostInstallEvidence -Evidence $valid | Out-Null

    $boundedProcess = Invoke-BoundedProcess `
        -FilePath $env:ComSpec `
        -ArgumentList @("/d", "/c", "echo running=true") `
        -TimeoutSeconds 5
    if ($boundedProcess.exit_code -ne 0 -or $boundedProcess.stdout -notmatch "running=true") {
        throw "Bounded process fixture did not capture a successful command. exit=$($boundedProcess.exit_code) stdout='$($boundedProcess.stdout)' stderr='$($boundedProcess.stderr)'"
    }

    $currentDaemon = ConvertFrom-BoundlessDaemonStatusOutput `
        -Output "running=true daemon_version=5.0.13-dogfood.1 peers=1" `
        -ExpectedVersion "5.0.13-dogfood.1"
    $staleDaemon = ConvertFrom-BoundlessDaemonStatusOutput `
        -Output "running=true daemon_version=5.0.12 peers=1" `
        -ExpectedVersion "5.0.13-dogfood.1"
    if (-not $currentDaemon.healthy -or $staleDaemon.healthy) {
        throw "Daemon status version fixture did not reject a stale running service."
    }

    $parsedTrayVersion = Get-BoundlessVersionFromOutput `
        -Output "boundlesstray 5.0.13-dogfood.1" `
        -ExecutableName "boundlesstray"
    if ($parsedTrayVersion -ne "5.0.13-dogfood.1") {
        throw "Executable version fixture parsed '$parsedTrayVersion'."
    }

    $expectedTrayPath = "C:\Program Files\Boundless\boundlesstray.exe"
    $correctTray = [pscustomobject]@{
        id = 123
        path = $expectedTrayPath
        responding = $true
    }
    $acceptedTray = Assert-SoleBoundlessTraySnapshot `
        -Processes @($correctTray) `
        -ExpectedTrayPath $expectedTrayPath `
        -Phase "in self-test"
    if ($acceptedTray.id -ne 123) {
        throw "Tray path fixture did not accept the installed path."
    }
    $wrongTrayRejected = $false
    try {
        Assert-SoleBoundlessTraySnapshot `
            -Processes @([pscustomobject]@{
                id = 456
                path = "C:\Portable\Boundless\boundlesstray.exe"
                responding = $true
            }) `
            -ExpectedTrayPath $expectedTrayPath `
            -Phase "in self-test" | Out-Null
    }
    catch {
        $wrongTrayRejected = $true
    }
    if (-not $wrongTrayRejected) {
        throw "Tray path fixture accepted an old or portable executable path."
    }

    $msiPropertyFixture = "skipped"
    if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
        $resolvedSelfTestInstaller = (Resolve-Path -LiteralPath $InstallerPath).Path
        $selfTestVersion = Get-MsiProperty -Path $resolvedSelfTestInstaller -Property "ProductVersion"
        $selfTestProductCode = Get-MsiProperty -Path $resolvedSelfTestInstaller -Property "ProductCode"
        if ($selfTestVersion -notmatch '^\d+\.\d+\.\d+$' -or $selfTestProductCode -notmatch '^\{[0-9A-Fa-f-]+\}$') {
            throw "MSI property fixture returned unexpected values. ProductVersion=$selfTestVersion ProductCode=$selfTestProductCode"
        }
        $msiPropertyFixture = "passed"
    }

    foreach ($mutation in @(
        @{ name = "registration"; property = "product_registered"; value = $false },
        @{ name = "display_version"; property = "display_version"; value = "5.0.12" },
        @{ name = "version"; property = "manifest_version"; value = "5.0.12" },
        @{ name = "sid"; property = "service_allowed_user_sid"; value = "S-1-5-21-9" },
        @{ name = "service_path"; property = "service_binary_path_matches"; value = $false },
        @{ name = "service"; property = "service_status"; value = "Stopped" },
        @{ name = "api"; property = "daemon_api_healthy"; value = $false },
        @{ name = "daemon_runtime_version"; property = "daemon_runtime_version"; value = "5.0.12" },
        @{ name = "executable_versions"; property = "executable_versions_match"; value = $false },
        @{ name = "tray_count"; property = "tray_count"; value = 2 },
        @{ name = "tray_path"; property = "tray_path_matches"; value = $false },
        @{ name = "tray_responsive"; property = "tray_responding"; value = $false },
        @{ name = "tray_stability"; property = "tray_stable_milliseconds"; value = 250 }
    )) {
        $fixture = $valid.PSObject.Copy()
        $fixture.($mutation.property) = $mutation.value
        $failed = $false
        try {
            Assert-PostInstallEvidence -Evidence $fixture | Out-Null
        }
        catch {
            $failed = $true
        }
        if (-not $failed) {
            throw "Post-install verification fixture '$($mutation.name)' was expected to fail."
        }
    }

    [pscustomobject]@{
        status = "passed"
        helper = "Boundless-Install.ps1"
        post_install_fixtures = 13
        bounded_process_fixture = "passed"
        daemon_version_fixture = "passed"
        executable_version_fixture = "passed"
        tray_path_fixture = "passed"
        msi_property_fixture = $msiPropertyFixture
    } | ConvertTo-Json -Depth 3
}

if ($SelfTest) {
    Invoke-InstallHelperSelfTest
    return
}

$selection = Resolve-AllowedUser
Assert-AllowedUserSid -Sid $selection.sid

$summary = [ordered]@{
    selected_user_sid = $selection.sid
    selected_user_account = $selection.account
    selected_user_source = $selection.source
    elevated_process = Test-IsAdministrator
}

if ($ResolveOnly) {
    $summary.status = "resolved"
    $summary | ConvertTo-Json -Depth 3
    return
}

$resolvedInstallerPath = Resolve-InstallerPath
$summary.installer_path = $resolvedInstallerPath

Write-Host "boundless_install_selected_user_sid=$($selection.sid)"
if (-not [string]::IsNullOrWhiteSpace($selection.account)) {
    Write-Host "boundless_install_selected_user_account=$($selection.account)"
}
Write-Host "boundless_install_selected_user_source=$($selection.source)"

$exitCode = Invoke-BoundlessMsi -ResolvedInstallerPath $resolvedInstallerPath -Sid $selection.sid
Write-Host "boundless_install_exit_code=$exitCode"
$verification = Invoke-PostInstallVerification `
    -ResolvedInstallerPath $resolvedInstallerPath `
    -ExpectedAllowedUserSid $selection.sid `
    -LaunchTray:(-not $Quiet -and -not (Test-IsAdministrator))
$summary.post_install_verification = $verification
$summary.status = if ($verification.tray_verification -eq "passed") {
    "installed_and_verified"
}
else {
    "installed_core_verified_tray_deferred"
}
Write-Host "boundless_install_core_verified=true"
Write-Host "boundless_install_tray_verification=$($verification.tray_verification)"
$summary | ConvertTo-Json -Depth 5
