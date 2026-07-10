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
    [switch]$SelfTest,
    [Parameter(DontShow = $true)]
    [switch]$ElevatedInstall,
    [Parameter(DontShow = $true)]
    [string]$ExpectedInstallerSha256 = ""
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

function New-BoundlessMsiArguments {
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

    return $arguments
}

function Invoke-BoundlessMsiElevated {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid
    )

    if (-not (Test-IsAdministrator)) {
        throw "The MSI phase must run elevated."
    }

    $arguments = New-BoundlessMsiArguments -ResolvedInstallerPath $ResolvedInstallerPath -Sid $Sid
    $startArgs = @{
        FilePath = "msiexec.exe"
        ArgumentList = (@($arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " ")
        Wait = $true
        PassThru = $true
    }

    $process = Start-Process @startArgs
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "msiexec.exe failed with exit code $($process.ExitCode)."
    }

    return $process.ExitCode
}

function Resolve-CurrentPowerShellExecutable {
    $currentProcess = Get-Process -Id $PID -ErrorAction Stop
    if (-not [string]::IsNullOrWhiteSpace($currentProcess.Path)) {
        return $currentProcess.Path
    }

    foreach ($candidate in @("pwsh.exe", "powershell.exe")) {
        $command = Get-Command $candidate -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $command) {
            return $command.Source
        }
    }
    throw "Could not resolve the current PowerShell executable for elevation."
}

function New-BoundlessTrayOwnerMutexSecurity {
    param([string]$UserSid)

    Assert-AllowedUserSid -Sid $UserSid
    $security = [Security.AccessControl.MutexSecurity]::new()
    $security.SetSecurityDescriptorSddlForm(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$UserSid)"
    )
    return $security
}

function New-BoundlessNamedMutex {
    param(
        [string]$Name,
        [string]$UserSid,
        [bool]$InitiallyOwned
    )

    $security = New-BoundlessTrayOwnerMutexSecurity -UserSid $UserSid
    $arguments = [object[]]@($InitiallyOwned, $Name, $false, $security)
    $mutexAclType = "System.Threading.MutexAcl" -as [type]
    if ($null -ne $mutexAclType) {
        $createMethod = $mutexAclType.GetMethods() |
            Where-Object { $_.Name -eq "Create" -and $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $createMethod) {
            throw "Could not resolve MutexAcl.Create for the tray quiescence lease."
        }
        $mutex = $createMethod.Invoke($null, $arguments)
    }
    else {
        $constructor = [Threading.Mutex].GetConstructors() |
            Where-Object { $_.GetParameters().Count -eq 4 } |
            Select-Object -First 1
        if ($null -eq $constructor) {
            throw "Could not resolve the secured Mutex constructor for the tray quiescence lease."
        }
        $mutex = $constructor.Invoke($arguments)
    }

    return [pscustomobject]@{
        mutex = $mutex
        created_new = [bool]$arguments[2]
        name = $Name
    }
}

function Get-BoundlessTrayOwnerMutexName {
    param(
        [string]$UserSid,
        [int]$SessionId
    )

    return "Local\Boundless.Tray.SingleInstance.v1.$UserSid.$SessionId.Owner"
}

function Enter-BoundlessTrayQuiescence {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId,
        [int]$TimeoutSeconds = 12
    )

    $mutexNameArgs = @{
        UserSid = $ExpectedOwnerSid
        SessionId = $ExpectedSessionId
    }
    $mutexName = Get-BoundlessTrayOwnerMutexName @mutexNameArgs
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $attempts = 0
    do {
        $attempts += 1
        $shutdownArgs = @{
            ExpectedOwnerSid = $ExpectedOwnerSid
            ExpectedSessionId = $ExpectedSessionId
            TimeoutSeconds = [Math]::Max(1, [int](($deadline - (Get-Date)).TotalSeconds))
        }
        $shutdown = Stop-BoundlessTrayForUpgrade @shutdownArgs
        $leaseArgs = @{
            Name = $mutexName
            UserSid = $ExpectedOwnerSid
            InitiallyOwned = $true
        }
        $leaseAttempt = New-BoundlessNamedMutex @leaseArgs
        if ($leaseAttempt.created_new) {
            return [pscustomobject]@{
                mutex = $leaseAttempt.mutex
                evidence = [pscustomobject]@{
                    name = $mutexName
                    acquired = $true
                    attempts = $attempts
                    shutdown = $shutdown
                    integrity = "creator_default"
                    spans_elevation_and_msi = $true
                }
            }
        }

        $leaseAttempt.mutex.Dispose()
        Start-Sleep -Milliseconds 50
    } while ((Get-Date) -lt $deadline)

    throw "Could not acquire the Boundless tray quiescence lease within $($TimeoutSeconds)s. The UAC/MSI phase was not started."
}

function Exit-BoundlessTrayQuiescence {
    param([object]$Lease)

    if ($null -eq $Lease -or $null -eq $Lease.mutex) {
        return
    }
    try {
        $Lease.mutex.ReleaseMutex()
    }
    finally {
        $Lease.mutex.Dispose()
    }
}

function Get-BoundlessAdminOnlyStageSddl {
    # A protected DACL is sufficient because the stage is created atomically
    # below a machine-owned known folder. Avoid a mandatory-label SACL here:
    # setting one during directory creation requires a privilege that a normal
    # split-token administrator does not receive merely by accepting UAC.
    return "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
}

function New-BoundlessSecuredDirectoryAtomic {
    param(
        [string]$Path,
        [Security.AccessControl.DirectorySecurity]$Security
    )

    $create = [IO.Directory].GetMethods() |
        Where-Object {
            $parameters = $_.GetParameters()
            $_.Name -eq "CreateDirectory" -and $parameters.Count -eq 2 -and
                $parameters[0].ParameterType -eq [string] -and
                $parameters[1].ParameterType -eq [Security.AccessControl.DirectorySecurity]
        } | Select-Object -First 1
    $invokeArguments = [Array]::CreateInstance([object], 2)
    if ($null -ne $create) {
        # MethodInfo.Invoke does not unwrap PowerShell's PSObject wrappers from
        # an object[] literal under either Windows PowerShell 5.1 or pwsh.
        $invokeArguments.SetValue($Path.PSObject.BaseObject, 0)
        $invokeArguments.SetValue($Security.PSObject.BaseObject, 1)
    }
    else {
        $aclType = "System.IO.FileSystemAclExtensions" -as [type]
        if ($null -eq $aclType) {
            throw "No secured directory creation API is available."
        }
        $create = $aclType.GetMethods() |
            Where-Object {
                $parameters = $_.GetParameters()
                $_.Name -eq "CreateDirectory" -and $parameters.Count -eq 2 -and
                    $parameters[0].ParameterType -eq [Security.AccessControl.DirectorySecurity] -and
                    $parameters[1].ParameterType -eq [string]
            } | Select-Object -First 1
        if ($null -eq $create) {
            throw "No FileSystemAclExtensions.CreateDirectory API is available."
        }
        $invokeArguments.SetValue($Security.PSObject.BaseObject, 0)
        $invokeArguments.SetValue($Path.PSObject.BaseObject, 1)
    }

    $null = $create.Invoke($null, $invokeArguments)
    return Get-Item -LiteralPath $Path -Force -ErrorAction Stop
}

function Get-BoundlessProgramDataRoot {
    $path = [Environment]::GetFolderPath(
        [Environment+SpecialFolder]::CommonApplicationData
    )
    if ([string]::IsNullOrWhiteSpace($path)) {
        throw "Could not resolve the Windows CommonApplicationData known folder."
    }
    return [IO.Path]::GetFullPath($path).TrimEnd('\')
}

function Test-BoundlessInstallerStagePath {
    param(
        [string]$Path,
        [string]$ProgramDataRoot = ""
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }
    if ([string]::IsNullOrWhiteSpace($ProgramDataRoot)) {
        $ProgramDataRoot = Get-BoundlessProgramDataRoot
    }
    $fullPath = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $fullProgramData = [IO.Path]::GetFullPath($ProgramDataRoot).TrimEnd('\')
    $parent = [IO.Directory]::GetParent($fullPath)
    if ($null -eq $parent -or -not $parent.FullName.Equals(
        $fullProgramData,
        [StringComparison]::OrdinalIgnoreCase
    )) {
        return $false
    }
    return [IO.Path]::GetFileName($fullPath) -match '^BoundlessInstaller-[0-9a-f]{32}$'
}

function Assert-BoundlessAdminOnlyAcl {
    param(
        [string]$Path,
        [bool]$RequireProtected = $false
    )

    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if ($RequireProtected -and -not $acl.AreAccessRulesProtected) {
        throw "Installer staging ACL inherited permissions: $Path"
    }

    $allowedSids = @("S-1-5-18", "S-1-5-32-544")
    $observedAllowedSids = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate(
            [Security.Principal.SecurityIdentifier]
        ).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $allowedSids) {
                throw "Installer staging ACL granted access to unexpected SID $sid at $Path"
            }
            $observedAllowedSids += $sid
        }
    }
    foreach ($requiredSid in $allowedSids) {
        if ($requiredSid -notin $observedAllowedSids) {
            throw "Installer staging ACL omitted required SID $requiredSid at $Path"
        }
    }
    return $acl
}

function New-BoundlessStagingProbeCommand {
    param(
        [string]$ProbeParent,
        [string]$SourcePath,
        [string]$UserSid
    )

    $stageLeaf = "BoundlessInstaller-$([guid]::NewGuid().ToString('N'))"
    $payload = [ordered]@{
        stage_parent = $ProbeParent
        stage_leaf = $stageLeaf
        source_path = $SourcePath
        source_sha256 = (Get-FileHash -LiteralPath $SourcePath -Algorithm SHA256).Hash
        user_sid = $UserSid
        stage_sddl = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;$UserSid)"
    }
    $payloadJson = $payload | ConvertTo-Json -Compress
    $payloadBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($payloadJson))
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
function New-BoundlessSecuredDirectoryAtomic {
__SECURED_DIRECTORY_FUNCTION__
}
function Assert-ProbeAcl {
    param([string]$Path, [string[]]$ExpectedSids)
    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if (-not $acl.AreAccessRulesProtected) {
        throw "Staging probe inherited an ACL."
    }
    $observed = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $ExpectedSids) {
                throw "Staging probe granted an unexpected principal."
            }
            $observed += $sid
        }
    }
    foreach ($sid in $ExpectedSids) {
        if ($sid -notin $observed) {
            throw "Staging probe omitted a required principal."
        }
    }
}
$payloadJson = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD_BASE64__")
)
$payload = $payloadJson | ConvertFrom-Json
$parent = (Resolve-Path -LiteralPath $payload.stage_parent -ErrorAction Stop).Path.TrimEnd('\')
$stageRoot = Join-Path $parent $payload.stage_leaf
$trustedStage = $false
try {
    if (
        [IO.Directory]::GetParent([IO.Path]::GetFullPath($stageRoot)).FullName -ne $parent -or
        [IO.Path]::GetFileName($stageRoot) -notmatch '^BoundlessInstaller-[0-9a-f]{32}$'
    ) {
        throw "Staging probe received an unsafe boundary."
    }
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetSecurityDescriptorSddlForm([string]$payload.stage_sddl)
    $item = New-BoundlessSecuredDirectoryAtomic -Path $stageRoot -Security $security
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Staging probe created a reparse point."
    }
    $probeSids = @("S-1-5-18", "S-1-5-32-544", [string]$payload.user_sid)
    Assert-ProbeAcl -Path $stageRoot -ExpectedSids $probeSids
    $trustedStage = $true

    $stagedCopy = Join-Path $stageRoot "probe.bin"
    Copy-Item -LiteralPath $payload.source_path -Destination $stagedCopy -ErrorAction Stop
    if ((Get-FileHash -LiteralPath $stagedCopy -Algorithm SHA256).Hash -ne $payload.source_sha256) {
        throw "Staging probe copy hash did not match."
    }
}
finally {
    if ($trustedStage -and (Test-Path -LiteralPath $stageRoot)) {
        $resolved = (Resolve-Path -LiteralPath $stageRoot).Path
        $resolvedParent = [IO.Directory]::GetParent($resolved)
        $leaf = [IO.Path]::GetFileName($resolved)
        $item = Get-Item -LiteralPath $resolved -Force
        if (
            $null -eq $resolvedParent -or
            -not $resolvedParent.FullName.Equals($parent, [StringComparison]::OrdinalIgnoreCase) -or
            $leaf -notmatch '^BoundlessInstaller-[0-9a-f]{32}$' -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        ) {
            throw "Staging probe refused an unsafe cleanup boundary."
        }
        Assert-ProbeAcl -Path $resolved -ExpectedSids $probeSids
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
    }
}
if (Test-Path -LiteralPath $stageRoot) {
    throw "Staging probe did not clean its stage."
}
Write-Output "boundless_staging_child_probe=passed"
'@
    $securedDirectoryFunction = (
        Get-Command New-BoundlessSecuredDirectoryAtomic -CommandType Function -ErrorAction Stop
    ).Definition
    $source = $source.Replace("__SECURED_DIRECTORY_FUNCTION__", $securedDirectoryFunction)
    $source = $source.Replace("__PAYLOAD_BASE64__", $payloadBase64)
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
    if ($encodedCommand.Length -gt 30000) {
        throw "The staging child-process probe exceeded the safe Windows command-line budget."
    }
    return [pscustomobject]@{
        encoded_command = $encodedCommand
        stage_path = Join-Path $ProbeParent $stageLeaf
    }
}

function Invoke-BoundlessStagingChildProbes {
    param([string]$SourcePath)

    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    $probeParent = Join-Path $tempRoot (
        "BoundlessStagingProbe-$([guid]::NewGuid().ToString('N'))"
    )
    $userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $testedHosts = @()
    try {
        New-Item -ItemType Directory -Path $probeParent -ErrorAction Stop | Out-Null
        foreach ($hostName in @("powershell.exe", "pwsh.exe")) {
            $hostCommand = Get-Command $hostName -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($null -eq $hostCommand) {
                continue
            }
            $probe = New-BoundlessStagingProbeCommand `
                -ProbeParent $probeParent `
                -SourcePath $SourcePath `
                -UserSid $userSid
            $probeProcessArgs = @{
                FilePath = $hostCommand.Source
                ArgumentList = @("-NoProfile", "-EncodedCommand", $probe.encoded_command)
                TimeoutSeconds = 20
            }
            if ($hostName -eq "powershell.exe") {
                # pwsh prepends its own modules to the inherited PSModulePath.
                # Windows PowerShell can then find the pwsh Security manifest
                # first and fail to load Get-Acl. Restore Desktop-edition paths
                # for this cross-host executable probe.
                $userWindowsModules = Join-Path (
                    [Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments)
                ) "WindowsPowerShell\Modules"
                $machineWindowsModules = @(
                    [Environment]::GetEnvironmentVariable("PSModulePath", "Machine") -split ';' |
                        Where-Object { $_ -match '(?i)\\WindowsPowerShell\\' }
                )
                $probeProcessArgs.EnvironmentVariables = @{
                    PSModulePath = (@($userWindowsModules) + $machineWindowsModules) -join ';'
                }
            }
            try {
                $result = Invoke-BoundedProcess @probeProcessArgs
            }
            catch {
                throw "Could not launch staging child-process probe under $hostName at '$($hostCommand.Source)'. $($_.Exception.Message)"
            }
            if (
                $result.exit_code -ne 0 -or
                $result.stdout -notmatch 'boundless_staging_child_probe=passed' -or
                (Test-Path -LiteralPath $probe.stage_path)
            ) {
                throw "Staging child-process probe failed under $hostName. exit=$($result.exit_code) stdout='$($result.stdout)' stderr='$($result.stderr)'"
            }
            $testedHosts += $hostName
        }
        if ($testedHosts.Count -eq 0) {
            throw "No PowerShell host was available for the staging child-process probe."
        }
        return @($testedHosts)
    }
    finally {
        if (Test-Path -LiteralPath $probeParent) {
            $resolved = (Resolve-Path -LiteralPath $probeParent).Path
            $parent = [IO.Directory]::GetParent($resolved)
            $leaf = [IO.Path]::GetFileName($resolved)
            if (
                $null -eq $parent -or
                -not $parent.FullName.TrimEnd('\').Equals($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
                $leaf -notmatch '^BoundlessStagingProbe-[0-9a-f]{32}$'
            ) {
                throw "Refusing unsafe staging probe cleanup: $resolved"
            }
            Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
        }
    }
}

function Assert-ElevatedInstallResult {
    param([object]$Result)

    if ($null -eq $Result -or $Result.status -ne "passed") {
        $detail = if ($null -ne $Result -and $Result.PSObject.Properties.Match("error").Count -gt 0) {
            $Result.error
        }
        else {
            "elevated install result was missing or malformed"
        }
        throw "Elevated Boundless install failed: $detail"
    }
    if ($Result.msi_exit_code -notin @(0, 3010)) {
        throw "Elevated Boundless install returned unexpected MSI exit code $($Result.msi_exit_code)."
    }
    if ($Result.service_shutdown.force_kill_used) {
        throw "Elevated Boundless install reported a forbidden service force-kill."
    }
    if (
        $null -eq $Result.installer_stage -or
        -not $Result.installer_stage.admin_only -or
        -not $Result.installer_stage.hash_verified
    ) {
        throw "Elevated Boundless install did not prove an admin-only hash-verified MSI stage."
    }
    return $Result
}

function Invoke-ElevatedInstallPhase {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid,
        [string]$ExpectedInstallerSha256
    )

    if (-not (Test-IsAdministrator)) {
        throw "Internal elevated install phase was not elevated."
    }

    $stageRoot = Split-Path -Parent $ResolvedInstallerPath
    if (
        -not (Test-BoundlessInstallerStagePath -Path $stageRoot) -or
        [IO.Path]::GetFileName($ResolvedInstallerPath) -ne "Boundless.msi"
    ) {
        throw "Internal elevated install phase did not receive the expected immutable MSI stage."
    }
    Assert-BoundlessAdminOnlyAcl -Path $stageRoot -RequireProtected $true | Out-Null
    Assert-BoundlessAdminOnlyAcl -Path $ResolvedInstallerPath | Out-Null
    $stagedHash = (Get-FileHash -LiteralPath $ResolvedInstallerPath -Algorithm SHA256).Hash
    if (-not $stagedHash.Equals($ExpectedInstallerSha256, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Immutable staged MSI hash verification failed."
    }

    # This stop is sequential and bounded. MSI ServiceControl remains in the
    # package as an idempotent verification/repair contract; it does not race a
    # concurrent helper stop and the helper never force-kills the service.
    $serviceShutdown = Stop-BoundlessServiceForUpgrade
    $msiArgs = @{
        ResolvedInstallerPath = $ResolvedInstallerPath
        Sid = $Sid
    }
    $exitCode = Invoke-BoundlessMsiElevated @msiArgs

    return [pscustomobject]@{
        status = "passed"
        msi_exit_code = $exitCode
        service_shutdown = $serviceShutdown
        installer_stage = [pscustomobject]@{
            admin_only = $true
            hash_verified = $true
            staged_copy_used = $true
            cleaned = $false
        }
    }
}

function New-BoundlessElevatedInstallCommand {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid
    )

    if ([string]::IsNullOrWhiteSpace($PSCommandPath)) {
        throw "Could not resolve the install helper path for immutable elevation staging."
    }
    $resolvedHelperPath = (Resolve-Path -LiteralPath $PSCommandPath).Path
    $payload = [ordered]@{
        installer_path = $ResolvedInstallerPath
        installer_sha256 = (Get-FileHash -LiteralPath $ResolvedInstallerPath -Algorithm SHA256).Hash
        helper_path = $resolvedHelperPath
        helper_sha256 = (Get-FileHash -LiteralPath $resolvedHelperPath -Algorithm SHA256).Hash
        sid = $Sid
        quiet = [bool]$Quiet
        no_restart = [bool]$NoRestart
        log_path = $LogPath
        stage_sddl = Get-BoundlessAdminOnlyStageSddl
    }
    $payloadJson = $payload | ConvertTo-Json -Compress
    $payloadBase64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($payloadJson))
    $source = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
function New-BoundlessSecuredDirectoryAtomic {
__SECURED_DIRECTORY_FUNCTION__
}
function Assert-AdminAcl {
    param([string]$Path, [bool]$RequireProtected = $false)
    $acl = Get-Acl -LiteralPath $Path -ErrorAction Stop
    if ($RequireProtected -and -not $acl.AreAccessRulesProtected) {
        throw "Installer stage inherited permissions."
    }
    $required = @("S-1-5-18", "S-1-5-32-544")
    $observed = @()
    foreach ($rule in @($acl.Access)) {
        $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow) {
            if ($sid -notin $required) { throw "Installer stage granted unexpected access." }
            $observed += $sid
        }
    }
    foreach ($sid in $required) {
        if ($sid -notin $observed) { throw "Installer stage omitted a required principal." }
    }
}
function Quote-Argument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') { return $Value }
    return '"' + ($Value -replace '"', '\"') + '"'
}
$payloadJson = [Text.Encoding]::UTF8.GetString(
    [Convert]::FromBase64String("__PAYLOAD_BASE64__")
)
$payload = $payloadJson | ConvertFrom-Json
$programDataKnownFolder = [Environment]::GetFolderPath(
    [Environment+SpecialFolder]::CommonApplicationData
)
if ([string]::IsNullOrWhiteSpace($programDataKnownFolder)) {
    throw "Could not resolve the Windows CommonApplicationData known folder."
}
$programData = [IO.Path]::GetFullPath($programDataKnownFolder).TrimEnd('\')
$stageRoot = Join-Path $programData ("BoundlessInstaller-" + [guid]::NewGuid().ToString("N"))
$trustedStage = $false
$exitCode = 1
try {
    $security = [Security.AccessControl.DirectorySecurity]::new()
    $security.SetSecurityDescriptorSddlForm([string]$payload.stage_sddl)
    $item = New-BoundlessSecuredDirectoryAtomic -Path $stageRoot -Security $security
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Installer stage was a reparse point."
    }
    Assert-AdminAcl -Path $stageRoot -RequireProtected $true
    $trustedStage = $true

    $stagedMsi = Join-Path $stageRoot "Boundless.msi"
    $stagedHelper = Join-Path $stageRoot "Boundless-Install.ps1"
    Copy-Item -LiteralPath $payload.installer_path -Destination $stagedMsi -ErrorAction Stop
    Copy-Item -LiteralPath $payload.helper_path -Destination $stagedHelper -ErrorAction Stop
    Assert-AdminAcl -Path $stagedMsi
    Assert-AdminAcl -Path $stagedHelper
    if ((Get-FileHash -LiteralPath $stagedMsi -Algorithm SHA256).Hash -ne $payload.installer_sha256) {
        throw "Staged MSI hash mismatch."
    }
    if ((Get-FileHash -LiteralPath $stagedHelper -Algorithm SHA256).Hash -ne $payload.helper_sha256) {
        throw "Staged helper hash mismatch."
    }

    $arguments = @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $stagedHelper,
        "-ElevatedInstall", "-InstallerPath", $stagedMsi,
        "-ExpectedInstallerSha256", $payload.installer_sha256,
        "-AllowedUserSid", $payload.sid
    )
    if ([bool]$payload.quiet) { $arguments += "-Quiet" }
    if ([bool]$payload.no_restart) { $arguments += "-NoRestart" }
    if (-not [string]::IsNullOrWhiteSpace([string]$payload.log_path)) {
        $arguments += @("-LogPath", [string]$payload.log_path)
    }
    $argumentLine = @($arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    $hostPath = (Get-Process -Id $PID -ErrorAction Stop).Path
    $child = Start-Process -FilePath $hostPath -ArgumentList $argumentLine -WindowStyle Hidden -Wait -PassThru
    if ($child.ExitCode -notin @(0, 3010)) {
        throw "Immutable staged helper failed with exit code $($child.ExitCode)."
    }
    $exitCode = $child.ExitCode
}
catch {
    Write-Host "boundless_install_elevated_error=$($_.Exception.Message)"
    $exitCode = 1
}
finally {
    if ($trustedStage -and (Test-Path -LiteralPath $stageRoot)) {
        $resolved = (Resolve-Path -LiteralPath $stageRoot).Path
        $parent = [IO.Directory]::GetParent($resolved)
        $leaf = [IO.Path]::GetFileName($resolved)
        $item = Get-Item -LiteralPath $resolved -Force
        if (
            $null -eq $parent -or
            -not $parent.FullName.Equals($programData, [StringComparison]::OrdinalIgnoreCase) -or
            $leaf -notmatch '^BoundlessInstaller-[0-9a-f]{32}$' -or
            ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        ) {
            throw "Refusing unsafe installer stage cleanup."
        }
        Assert-AdminAcl -Path $resolved -RequireProtected $true
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction Stop
    }
}
exit $exitCode
'@
    $securedDirectoryFunction = (
        Get-Command New-BoundlessSecuredDirectoryAtomic -CommandType Function -ErrorAction Stop
    ).Definition
    $source = $source.Replace("__SECURED_DIRECTORY_FUNCTION__", $securedDirectoryFunction)
    $source = $source.Replace("__PAYLOAD_BASE64__", $payloadBase64)
    $encodedCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($source))
    if ($encodedCommand.Length -gt 30000) {
        throw "The bounded elevated install command exceeded the safe Windows command-line budget."
    }
    return [pscustomobject]@{
        source = $source
        encoded_command = $encodedCommand
        installer_sha256 = $payload.installer_sha256
    }
}

function Invoke-BoundlessMsi {
    param(
        [string]$ResolvedInstallerPath,
        [string]$Sid
    )

    $elevatedCommandArgs = @{
        ResolvedInstallerPath = $ResolvedInstallerPath
        Sid = $Sid
    }
    $elevatedCommand = New-BoundlessElevatedInstallCommand @elevatedCommandArgs
    $arguments = @(
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
        $elevatedCommand.encoded_command
    )

    $startArgs = @{
        FilePath = (Resolve-CurrentPowerShellExecutable)
        ArgumentList = (@($arguments | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " ")
        WindowStyle = "Hidden"
        Wait = $true
        PassThru = $true
    }
    if (-not (Test-IsAdministrator)) {
        $startArgs.Verb = "RunAs"
    }
    $process = Start-Process @startArgs
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "Elevated Boundless install phase exited with $($process.ExitCode)."
    }

    # The elevated phase can launch MSI only after the bounded non-forced
    # service stop completed. Exact stop timing is printed in that phase; this
    # parent records only the cross-elevation contract.
    return Assert-ElevatedInstallResult -Result ([pscustomobject]@{
        status = "passed"
        msi_exit_code = $process.ExitCode
        service_shutdown = [pscustomobject]@{
            initial_status = "captured_in_elevated_phase"
            final_status = "StoppedOrNotInstalledBeforeMsi"
            stop_requested = $null
            elapsed_milliseconds = $null
            force_kill_used = $false
            msi_service_control = "idempotent_verification_after_helper_stop"
        }
        installer_stage = [pscustomobject]@{
            admin_only = $true
            hash_verified = $true
            staged_copy_used = $true
            cleaned = $true
        }
    })
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
        [int]$TimeoutSeconds = 10,
        [hashtable]$EnvironmentVariables = @{}
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.Arguments = @($ArgumentList | ForEach-Object { ConvertTo-ProcessArgument -Value $_ }) -join " "
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($entry in $EnvironmentVariables.GetEnumerator()) {
        $startInfo.EnvironmentVariables[$entry.Key] = [string]$entry.Value
    }

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

function Get-BoundlessServiceStopDecision {
    param(
        [string]$Status,
        [bool]$StopRequested
    )

    if ($Status -eq "Stopped") {
        return "complete"
    }
    if ($Status -eq "StopPending" -or $StopRequested) {
        return "wait"
    }
    return "request_stop"
}

function Stop-BoundlessServiceForUpgrade {
    param([int]$TimeoutSeconds = 15)

    if (-not (Test-IsAdministrator)) {
        throw "Stopping BoundlessService for upgrade requires elevation."
    }

    $service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        return [pscustomobject]@{
            initial_status = "NotInstalled"
            final_status = "NotInstalled"
            stop_requested = $false
            elapsed_milliseconds = 0
            force_kill_used = $false
            msi_service_control = "idempotent_install_contract"
        }
    }

    $initialStatus = $service.Status.ToString()
    $stopRequested = $false
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
        if ($null -eq $service) {
            throw "BoundlessService disappeared while the helper was stopping it."
        }

        $status = $service.Status.ToString()
        $decision = Get-BoundlessServiceStopDecision -Status $status -StopRequested $stopRequested
        if ($decision -eq "complete") {
            $stopwatch.Stop()
            return [pscustomobject]@{
                initial_status = $initialStatus
                final_status = $status
                stop_requested = $stopRequested
                elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
                force_kill_used = $false
                msi_service_control = "idempotent_verification_after_helper_stop"
            }
        }
        if ($decision -eq "request_stop" -and $service.CanStop) {
            # ServiceController.Stop sends one normal SCM stop control and
            # returns. The bounded poll below owns completion; there is no
            # Stop-Process/TerminateProcess fallback.
            $service.Stop()
            $stopRequested = $true
        }

        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)

    $finalService = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
    $finalStatus = if ($null -eq $finalService) { "Missing" } else { $finalService.Status.ToString() }
    throw "BoundlessService did not stop within $($TimeoutSeconds)s; initial=$initialStatus current=$finalStatus. The MSI was not started."
}

function Get-ProcessOwnerSid {
    param([int]$ProcessId)

    $process = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction Stop |
        Select-Object -First 1
    if ($null -eq $process) {
        throw "Process $ProcessId exited before its owner could be verified."
    }
    $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid -ErrorAction Stop
    if ($owner.ReturnValue -ne 0 -or [string]::IsNullOrWhiteSpace($owner.Sid)) {
        throw "Could not prove the owner SID for Boundless tray process $ProcessId; return=$($owner.ReturnValue)."
    }
    return $owner.Sid
}

function Assert-BoundlessTrayShutdownTargets {
    param(
        [object[]]$Processes,
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId
    )

    foreach ($process in @($Processes)) {
        if ($process.session_id -ne $ExpectedSessionId) {
            throw "Refusing to stop Boundless tray PID $($process.id) from session $($process.session_id); expected session $ExpectedSessionId."
        }
        if ([string]::IsNullOrWhiteSpace($process.owner_sid) -or $process.owner_sid -ne $ExpectedOwnerSid) {
            throw "Refusing to stop Boundless tray PID $($process.id) because its owner SID could not be proven as $ExpectedOwnerSid."
        }
    }
    return @($Processes)
}

function Initialize-BoundlessInstallNativeMethods {
    if ($null -ne ("BoundlessInstallNativeMethods" -as [type])) {
        return
    }

    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class BoundlessInstallNativeMethods
{
    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool PostThreadMessage(
        uint threadId,
        uint message,
        UIntPtr wParam,
        IntPtr lParam);
}
"@
}

function Request-LegacyBoundlessTrayQuit {
    param([int[]]$ProcessIds)

    Initialize-BoundlessInstallNativeMethods
    $postCount = 0
    foreach ($processId in $ProcessIds) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if ($null -eq $process) {
            continue
        }
        try {
            foreach ($thread in @($process.Threads)) {
                # v5.0.13 has no external Quit command. Posting WM_QUIT to its
                # same-user GUI/hook message queues causes eframe to unwind and
                # DashboardApp to drop; InputBrokerSupervisor::Drop then runs
                # the existing local fail-open and bounded detach path.
                if ([BoundlessInstallNativeMethods]::PostThreadMessage(
                    [uint32]$thread.Id,
                    [uint32]0x0012,
                    [UIntPtr]::Zero,
                    [IntPtr]::Zero
                )) {
                    $postCount += 1
                }
            }
        }
        catch {
            if ($null -ne (Get-Process -Id $processId -ErrorAction SilentlyContinue)) {
                throw "Could not request graceful legacy shutdown for Boundless tray PID $processId. $($_.Exception.Message)"
            }
        }
    }
    return $postCount
}

function Wait-BoundlessTrayProcessIdsExited {
    param(
        [int[]]$ProcessIds,
        [int]$TimeoutMilliseconds
    )

    $deadline = (Get-Date).AddMilliseconds($TimeoutMilliseconds)
    do {
        $remaining = @(
            $ProcessIds |
                Where-Object { $null -ne (Get-Process -Id $_ -ErrorAction SilentlyContinue) }
        )
        if ($remaining.Count -eq 0) {
            return $true
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Stop-BoundlessTrayForUpgrade {
    param(
        [string]$ExpectedOwnerSid,
        [int]$ExpectedSessionId = -1,
        [int]$TimeoutSeconds = 8
    )

    $currentSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    if ($ExpectedSessionId -lt 0) {
        $ExpectedSessionId = $currentSessionId
    }
    if ($ExpectedSessionId -ne $currentSessionId) {
        throw "Refusing tray shutdown outside helper session $currentSessionId; requested $ExpectedSessionId."
    }
    $targets = @(
        Assert-BoundlessTrayShutdownTargets -Processes @(
            Get-BoundlessTrayProcessesForCurrentSession
        ) -ExpectedOwnerSid $ExpectedOwnerSid -ExpectedSessionId $ExpectedSessionId
    )
    if ($targets.Count -eq 0) {
        return [pscustomobject]@{
            initial_count = 0
            control_requests = 0
            legacy_thread_quit_posts = 0
            elapsed_milliseconds = 0
            force_kill_used = $false
        }
    }

    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $processIds = @($targets | Select-Object -ExpandProperty id)
    $controlRequests = 0
    foreach ($path in @($targets.path | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)) {
        try {
            $result = Invoke-BoundedProcess -FilePath $path -ArgumentList @("--quit") -TimeoutSeconds 3
            if ($result.exit_code -eq 0) {
                $controlRequests += 1
            }
        }
        catch {
            # The v5.0.13 tray does not recognize --quit. Its bounded WM_QUIT
            # bridge below is the only supported compatibility path.
        }
    }

    if (Wait-BoundlessTrayProcessIdsExited -ProcessIds $processIds -TimeoutMilliseconds 2000) {
        $stopwatch.Stop()
        return [pscustomobject]@{
            initial_count = $targets.Count
            control_requests = $controlRequests
            legacy_thread_quit_posts = 0
            elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
            force_kill_used = $false
        }
    }

    $legacyPosts = Request-LegacyBoundlessTrayQuit -ProcessIds $processIds
    $remainingMilliseconds = [Math]::Max(100, ($TimeoutSeconds * 1000) - [int]$stopwatch.ElapsedMilliseconds)
    if (-not (Wait-BoundlessTrayProcessIdsExited -ProcessIds $processIds -TimeoutMilliseconds $remainingMilliseconds)) {
        $remaining = @(
            $processIds |
                Where-Object { $null -ne (Get-Process -Id $_ -ErrorAction SilentlyContinue) }
        ) -join ","
        throw "Boundless tray did not exit gracefully within $($TimeoutSeconds)s (remaining PIDs: $remaining). Quit Boundless manually and rerun the helper. The UAC/MSI phase was not started."
    }

    $stopwatch.Stop()
    return [pscustomobject]@{
        initial_count = $targets.Count
        control_requests = $controlRequests
        legacy_thread_quit_posts = $legacyPosts
        elapsed_milliseconds = $stopwatch.ElapsedMilliseconds
        force_kill_used = $false
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
                    session_id = $_.SessionId
                    owner_sid = Get-ProcessOwnerSid -ProcessId $_.Id
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

    $shutdownTarget = [pscustomobject]@{
        id = 789
        session_id = 7
        owner_sid = $validSid
        path = $expectedTrayPath
        responding = $true
    }
    $shutdownTargetArgs = @{
        Processes = @($shutdownTarget)
        ExpectedOwnerSid = $validSid
        ExpectedSessionId = 7
    }
    $acceptedShutdownTargets = @(
        Assert-BoundlessTrayShutdownTargets @shutdownTargetArgs
    )
    if ($acceptedShutdownTargets.Count -ne 1) {
        throw "Tray shutdown target fixture did not retain the proven same-user target."
    }
    $wrongOwnerRejected = $false
    try {
        $wrongOwnerTarget = $shutdownTarget.PSObject.Copy()
        $wrongOwnerTarget.owner_sid = "S-1-5-21-9-9-9-1002"
        $wrongOwnerArgs = @{
            Processes = @($wrongOwnerTarget)
            ExpectedOwnerSid = $validSid
            ExpectedSessionId = 7
        }
        Assert-BoundlessTrayShutdownTargets @wrongOwnerArgs | Out-Null
    }
    catch {
        $wrongOwnerRejected = $true
    }
    if (-not $wrongOwnerRejected) {
        throw "Tray shutdown target fixture accepted another Windows user."
    }
    $currentProcessOwnerSid = Get-ProcessOwnerSid -ProcessId $PID
    $currentIdentitySid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    if ($currentProcessOwnerSid -ne $currentIdentitySid) {
        throw "Live process-owner fixture returned $currentProcessOwnerSid; expected $currentIdentitySid."
    }
    Initialize-BoundlessInstallNativeMethods
    if ($null -eq [BoundlessInstallNativeMethods].GetMethod("PostThreadMessage")) {
        throw "Legacy WM_QUIT bridge native fixture did not expose PostThreadMessage."
    }

    $mutexSecurity = New-BoundlessTrayOwnerMutexSecurity -UserSid $currentIdentitySid
    $mutexSddl = $mutexSecurity.GetSecurityDescriptorSddlForm(
        [Security.AccessControl.AccessControlSections]::All
    )
    if (
        $mutexSddl -notmatch '\(A;;GA;;;SY\)' -or
        $mutexSddl -notmatch '\(A;;GA;;;BA\)' -or
        $mutexSddl -notmatch [regex]::Escape("(A;;GA;;;$currentIdentitySid)")
    ) {
        throw "Tray quiescence mutex fixture did not preserve its ownership DACL."
    }
    $selectedSidMutexName = Get-BoundlessTrayOwnerMutexName -UserSid $validSid -SessionId 7
    if ($selectedSidMutexName -ne "Local\Boundless.Tray.SingleInstance.v1.$validSid.7.Owner") {
        throw "Tray quiescence identity fixture did not retain the selected desktop SID and current session."
    }
    $quiescenceFixtureName = "Local\Boundless.Test.UpgradeLease.$PID.$([guid]::NewGuid().ToString('N'))"
    $firstLeaseArgs = @{
        Name = $quiescenceFixtureName
        UserSid = $currentIdentitySid
        InitiallyOwned = $true
    }
    $firstLease = New-BoundlessNamedMutex @firstLeaseArgs
    try {
        if (-not $firstLease.created_new) {
            throw "First tray quiescence fixture did not create the owner mutex."
        }
        $secondLeaseArgs = @{
            Name = $quiescenceFixtureName
            UserSid = $currentIdentitySid
            InitiallyOwned = $false
        }
        $secondLease = New-BoundlessNamedMutex @secondLeaseArgs
        try {
            if ($secondLease.created_new) {
                throw "Second tray quiescence fixture bypassed the held owner mutex."
            }
        }
        finally {
            $secondLease.mutex.Dispose()
        }
    }
    finally {
        if ($firstLease.created_new) {
            $firstLease.mutex.ReleaseMutex()
        }
        $firstLease.mutex.Dispose()
    }

    $stageSddl = Get-BoundlessAdminOnlyStageSddl
    if (
        $stageSddl -notmatch '\(A;OICI;FA;;;SY\)' -or
        $stageSddl -notmatch '\(A;OICI;FA;;;BA\)' -or
        $stageSddl -match ';;;BU\)' -or
        $stageSddl -match 'S:'
    ) {
        throw "Installer staging security fixture was not an admin-only protected DACL."
    }
    $knownProgramData = Get-BoundlessProgramDataRoot
    $originalProgramDataEnvironment = $env:ProgramData
    try {
        $env:ProgramData = "C:\Users\Public\BoundlessProgramDataPoison"
        $knownProgramDataWithPoisonedEnvironment = Get-BoundlessProgramDataRoot
    }
    finally {
        $env:ProgramData = $originalProgramDataEnvironment
    }
    if (-not (Test-WindowsPathEqual -Left $knownProgramData -Right $knownProgramDataWithPoisonedEnvironment)) {
        throw "Installer staging known-folder fixture trusted the inherited ProgramData environment variable."
    }
    $safeStageFixture = Join-Path $knownProgramData (
        "BoundlessInstaller-" + ("a" * 32)
    )
    $nestedStageFixture = Join-Path $knownProgramData (
        "Boundless\BoundlessInstaller-" + ("a" * 32)
    )
    if (
        -not (Test-BoundlessInstallerStagePath -Path $safeStageFixture) -or
        (Test-BoundlessInstallerStagePath -Path $nestedStageFixture)
    ) {
        throw "Installer staging path fixture accepted an unsafe cleanup boundary."
    }
    $stagingProbeHosts = @(
        Invoke-BoundlessStagingChildProbes -SourcePath $PSCommandPath
    )

    if (
        (Get-BoundlessServiceStopDecision -Status "Stopped" -StopRequested $false) -ne "complete" -or
        (Get-BoundlessServiceStopDecision -Status "StopPending" -StopRequested $false) -ne "wait" -or
        (Get-BoundlessServiceStopDecision -Status "Running" -StopRequested $false) -ne "request_stop" -or
        (Get-BoundlessServiceStopDecision -Status "Running" -StopRequested $true) -ne "wait"
    ) {
        throw "Bounded service-stop state fixture returned an unexpected action."
    }

    $validElevatedResult = [pscustomobject]@{
        status = "passed"
        msi_exit_code = 0
        service_shutdown = [pscustomobject]@{
            force_kill_used = $false
        }
        installer_stage = [pscustomobject]@{
            admin_only = $true
            hash_verified = $true
        }
    }
    Assert-ElevatedInstallResult -Result $validElevatedResult | Out-Null
    $rebootElevatedResult = $validElevatedResult.PSObject.Copy()
    $rebootElevatedResult.msi_exit_code = 3010
    Assert-ElevatedInstallResult -Result $rebootElevatedResult | Out-Null
    $serviceForceKillRejected = $false
    try {
        $invalidElevatedResult = $validElevatedResult.PSObject.Copy()
        $invalidElevatedResult.service_shutdown = [pscustomobject]@{
            force_kill_used = $true
        }
        Assert-ElevatedInstallResult -Result $invalidElevatedResult | Out-Null
    }
    catch {
        $serviceForceKillRejected = $true
    }
    if (-not $serviceForceKillRejected) {
        throw "Elevated install fixture accepted a service force-kill."
    }
    $elevatedCommandArgs = @{
        ResolvedInstallerPath = $PSCommandPath
        Sid = $validSid
    }
    $elevatedCommand = New-BoundlessElevatedInstallCommand @elevatedCommandArgs
    $decodedElevatedCommand = [Text.Encoding]::Unicode.GetString(
        [Convert]::FromBase64String($elevatedCommand.encoded_command)
    )
    $commandTokens = $null
    $commandErrors = $null
    [void][System.Management.Automation.Language.Parser]::ParseInput(
        $decodedElevatedCommand,
        [ref]$commandTokens,
        [ref]$commandErrors
    )
    if ($commandErrors.Count -ne 0) {
        throw "Elevated in-memory command fixture did not parse: $($commandErrors[0].Message)"
    }
    if (
        $decodedElevatedCommand -match '\$PSCommandPath' -or
        $decodedElevatedCommand -match '\$env:ProgramData' -or
        $decodedElevatedCommand -match 'S:\(ML;' -or
        $decodedElevatedCommand -notmatch 'BoundlessInstaller-' -or
        $decodedElevatedCommand -notmatch 'PSObject\.BaseObject' -or
        $decodedElevatedCommand -notmatch 'Staged helper hash mismatch'
    ) {
        throw "Elevated command fixture did not enforce immutable helper/MSI staging."
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
        tray_shutdown_identity_fixture = "passed"
        legacy_quit_bridge_fixture = "passed"
        tray_quiescence_lease_fixture = "passed"
        admin_only_stage_fixture = "passed"
        program_data_known_folder_fixture = "passed"
        staging_child_process_probe_hosts = $stagingProbeHosts
        bounded_service_stop_fixture = "passed"
        elevated_install_result_fixture = "passed"
        elevated_in_memory_command_fixture = "passed"
        msi_property_fixture = $msiPropertyFixture
    } | ConvertTo-Json -Depth 3
}

if ($SelfTest) {
    Invoke-InstallHelperSelfTest
    return
}

if ($ElevatedInstall) {
    try {
        if (-not (Test-IsAdministrator)) {
            throw "Internal immutable install phase did not receive an elevated token."
        }
        Assert-AllowedUserSid -Sid $AllowedUserSid
        if ($ExpectedInstallerSha256 -notmatch '^[0-9A-Fa-f]{64}$') {
            throw "Internal immutable install phase received an invalid MSI hash."
        }
        $resolvedElevatedInstallerPath = Resolve-InstallerPath
        $stageRoot = Split-Path -Parent $resolvedElevatedInstallerPath
        if (
            [string]::IsNullOrWhiteSpace($PSCommandPath) -or
            -not (Test-BoundlessInstallerStagePath -Path $stageRoot) -or
            -not (Test-WindowsPathEqual -Left (Split-Path -Parent $PSCommandPath) -Right $stageRoot)
        ) {
            throw "Internal immutable install phase was not running from its verified stage."
        }
        Assert-BoundlessAdminOnlyAcl -Path $PSCommandPath | Out-Null
        $elevatedPhaseArgs = @{
            ResolvedInstallerPath = $resolvedElevatedInstallerPath
            Sid = $AllowedUserSid
            ExpectedInstallerSha256 = $ExpectedInstallerSha256
        }
        $elevatedResult = Invoke-ElevatedInstallPhase @elevatedPhaseArgs
        Write-Host "boundless_install_service_stop_initial=$($elevatedResult.service_shutdown.initial_status)"
        Write-Host "boundless_install_service_stop_final=$($elevatedResult.service_shutdown.final_status)"
        Write-Host "boundless_install_service_stop_elapsed_ms=$($elevatedResult.service_shutdown.elapsed_milliseconds)"
        exit $elevatedResult.msi_exit_code
    }
    catch {
        Write-Error $_
        exit 1
    }
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

$currentSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$quiescenceArgs = @{
    # The selected SID is the intended desktop identity captured before UAC.
    # Using the helper process token here breaks over-the-shoulder elevation by
    # leasing an administrator-owned mutex while the real desktop tray remains
    # free to relaunch in this same session.
    ExpectedOwnerSid = $selection.sid
    ExpectedSessionId = $currentSessionId
}
$trayQuiescence = Enter-BoundlessTrayQuiescence @quiescenceArgs
$trayShutdown = $trayQuiescence.evidence.shutdown
Write-Host "boundless_install_tray_shutdown_count=$($trayShutdown.initial_count)"
Write-Host "boundless_install_tray_shutdown_elapsed_ms=$($trayShutdown.elapsed_milliseconds)"
Write-Host "boundless_install_tray_quiescence_acquired=$($trayQuiescence.evidence.acquired)"
try {
    $installResult = Invoke-BoundlessMsi -ResolvedInstallerPath $resolvedInstallerPath -Sid $selection.sid
}
finally {
    Exit-BoundlessTrayQuiescence -Lease $trayQuiescence
}
$exitCode = $installResult.msi_exit_code
Write-Host "boundless_install_exit_code=$exitCode"
Write-Host "boundless_install_service_stop_initial=$($installResult.service_shutdown.initial_status)"
Write-Host "boundless_install_service_stop_final=$($installResult.service_shutdown.final_status)"
if ($null -ne $installResult.service_shutdown.elapsed_milliseconds) {
    Write-Host "boundless_install_service_stop_elapsed_ms=$($installResult.service_shutdown.elapsed_milliseconds)"
}
$verification = Invoke-PostInstallVerification `
    -ResolvedInstallerPath $resolvedInstallerPath `
    -ExpectedAllowedUserSid $selection.sid `
    -LaunchTray:(-not $Quiet -and -not (Test-IsAdministrator))
$summary.pre_install_tray_shutdown = $trayShutdown
$summary.pre_install_tray_quiescence = $trayQuiescence.evidence
$summary.elevated_install = $installResult
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
