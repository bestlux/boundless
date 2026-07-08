[CmdletBinding(DefaultParameterSetName = "All")]
param(
    [Parameter(ParameterSetName = "NetworkOnly")]
    [switch]$NetworkOnly,

    [Parameter(ParameterSetName = "All")]
    [switch]$All,

    [switch]$ForceLocalCleanup,
    [switch]$IncludeServiceProfile,
    [switch]$SelfTest,

    [string]$Endpoint = "npipe://./pipe/boundlessd-api",
    [string]$InstallRoot = "",
    [string]$ConfigPath = "",
    [string]$DataRoot = "",
    [string]$SecurityRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$script:PackagingScriptRoot = if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
    (Resolve-Path ".").Path
}
else {
    $PSScriptRoot
}

function Get-LocalAppDataPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
}

function Get-ServiceLocalAppDataPath {
    return Join-Path $env:WINDIR "System32\config\systemprofile\AppData\Local"
}

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-DefaultConfigPath {
    return Join-Path (Join-Path (Get-LocalAppDataPath) "Boundless") "config.json"
}

function Get-DefaultDataRoot {
    return Join-Path (Get-LocalAppDataPath) "Boundless"
}

function Get-DefaultSecurityRoot {
    return Join-Path (Join-Path (Get-LocalAppDataPath) "Boundless") "security"
}

function Remove-IfExists {
    param([string]$Path)

    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

function Invoke-LocalNetworkReset {
    param([string]$TargetConfigPath)

    if (-not (Test-Path -LiteralPath $TargetConfigPath)) {
        return
    }

    $config = Get-Content -LiteralPath $TargetConfigPath -Raw | ConvertFrom-Json
    if ($null -eq $config.peers) {
        $config | Add-Member -NotePropertyName peers -NotePropertyValue @()
    } else {
        $config.peers = @()
    }

    $config | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $TargetConfigPath -Encoding utf8
}

function Invoke-LocalFullReset {
    param(
        [string]$TargetConfigPath,
        [string]$TargetDataRoot,
        [string]$TargetSecurityRoot
    )

    if (Test-Path -LiteralPath $TargetConfigPath) {
        Remove-Item -LiteralPath $TargetConfigPath -Force
    }

    Remove-IfExists -Path $TargetSecurityRoot
    Remove-IfExists -Path $TargetDataRoot
}

function Get-MachineIdFromStatusOutput {
    param([object[]]$StatusOutput)

    foreach ($line in $StatusOutput) {
        # daemon status emits key=value pairs on a single space-separated line.
        if ($line -match '(?:^|\s)machine_id=(\S+)') {
            return $Matches[1].Trim()
        }
    }

    return $null
}

function Get-CanonicalDaemonStatusFixture {
    $fixturePath = Join-Path (Join-Path $script:PackagingScriptRoot "fixtures") "daemon-status-single-line.txt"
    if (-not (Test-Path -LiteralPath $fixturePath)) {
        throw "daemon status fixture was not found: $fixturePath"
    }

    return (Get-Content -LiteralPath $fixturePath -Raw).Trim()
}

function Get-DaemonMachineId {
    param(
        [string]$BoundlessCtl,
        [string]$TargetEndpoint
    )

    $statusArguments = @("--endpoint", $TargetEndpoint, "daemon", "status")
    $statusOutput = & $BoundlessCtl @statusArguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "boundlessctl daemon status exited with $($LASTEXITCODE): $statusOutput"
    }

    $machineId = Get-MachineIdFromStatusOutput -StatusOutput $statusOutput
    if ($null -ne $machineId) {
        return $machineId
    }

    throw "boundlessctl daemon status did not report machine_id"
}

function Invoke-ResetSelfTest {
    $singleLine = Get-CanonicalDaemonStatusFixture
    if ((Get-MachineIdFromStatusOutput -StatusOutput @($singleLine)) -ne "4f0c6bce-6c10-4df9-b8b5-3a9a3fbb5da1") {
        throw "machine_id must parse from canonical single-line daemon status output"
    }

    $multiLine = @("running=true", "machine_id=abc-123", "peers=0")
    if ((Get-MachineIdFromStatusOutput -StatusOutput $multiLine) -ne "abc-123") {
        throw "machine_id must parse from line-per-field daemon status output"
    }

    if ($null -ne (Get-MachineIdFromStatusOutput -StatusOutput @("running=true peers=0"))) {
        throw "missing machine_id must return null"
    }

    Write-Host "self_test=pass"
}

function Invoke-DaemonReset {
    param(
        [string]$BoundlessCtl,
        [string]$TargetEndpoint,
        [string]$Mode
    )

    $machineId = Get-DaemonMachineId -BoundlessCtl $BoundlessCtl -TargetEndpoint $TargetEndpoint
    if ($Mode -eq "network") {
        $confirm = "safe-reset-network:$machineId"
        $arguments = @("--endpoint", $TargetEndpoint, "safe-reset", "--network", "--confirm", $confirm)
    } else {
        $confirm = "rotate-trust:$machineId"
        $arguments = @("--endpoint", $TargetEndpoint, "pair", "rotate-trust", "--confirm", $confirm)
    }

    & $BoundlessCtl @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "boundlessctl reset exited with $LASTEXITCODE"
    }
}

function Invoke-LocalResetTarget {
    param(
        [string]$TargetConfigPath,
        [string]$TargetDataRoot,
        [string]$TargetSecurityRoot
    )

    if ($NetworkOnly) {
        Invoke-LocalNetworkReset -TargetConfigPath $TargetConfigPath
    } else {
        Invoke-LocalFullReset -TargetConfigPath $TargetConfigPath -TargetDataRoot $TargetDataRoot -TargetSecurityRoot $TargetSecurityRoot
    }
}

if ($SelfTest) {
    Invoke-ResetSelfTest
    return
}

if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
    $ConfigPath = Get-DefaultConfigPath
}
if ([string]::IsNullOrWhiteSpace($InstallRoot)) {
    $InstallRoot = $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($DataRoot)) {
    $DataRoot = Get-DefaultDataRoot
}
if ([string]::IsNullOrWhiteSpace($SecurityRoot)) {
    $SecurityRoot = Get-DefaultSecurityRoot
}

$modeName = if ($NetworkOnly) { "network" } else { "all" }
$boundlessCtl = Join-Path $InstallRoot "boundlessctl.exe"
$attemptedRemoteReset = $false

if (-not $ForceLocalCleanup -and (Test-Path -LiteralPath $boundlessCtl)) {
    try {
        $attemptedRemoteReset = $true
        Invoke-DaemonReset -BoundlessCtl $boundlessCtl -TargetEndpoint $Endpoint -Mode $modeName
        Write-Host "reset_mode=$modeName"
        if ($NetworkOnly) {
            Write-Host "reset_method=daemon_api_safe_reset"
            Write-Host "restart_required=false"
        } else {
            Write-Host "reset_method=daemon_api_rotate_trust"
            Write-Host "restart_required=true"
        }
        return
    }
    catch {
        Write-Warning "boundlessctl daemon reset failed: $($_.Exception.Message). Falling back to local cleanup."
    }
}

Invoke-LocalResetTarget -TargetConfigPath $ConfigPath -TargetDataRoot $DataRoot -TargetSecurityRoot $SecurityRoot

if ($IncludeServiceProfile) {
    if (-not (Test-Administrator)) {
        throw "-IncludeServiceProfile requires an elevated PowerShell session."
    }

    $serviceDataRoot = Join-Path (Get-ServiceLocalAppDataPath) "Boundless"
    $serviceConfigPath = Join-Path $serviceDataRoot "config.json"
    $serviceSecurityRoot = Join-Path $serviceDataRoot "security"
    Invoke-LocalResetTarget -TargetConfigPath $serviceConfigPath -TargetDataRoot $serviceDataRoot -TargetSecurityRoot $serviceSecurityRoot
}

Write-Host "reset_mode=$modeName"
Write-Host "reset_method=local_cleanup"
Write-Host "attempted_remote_reset=$attemptedRemoteReset"
Write-Host "service_profile_cleanup=$IncludeServiceProfile"
