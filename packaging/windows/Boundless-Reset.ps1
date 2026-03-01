[CmdletBinding(DefaultParameterSetName = "All")]
param(
    [Parameter(ParameterSetName = "NetworkOnly")]
    [switch]$NetworkOnly,

    [Parameter(ParameterSetName = "All")]
    [switch]$All,

    [switch]$ForceLocalCleanup,

    [string]$Endpoint = "npipe://./pipe/boundlessd-api",
    [string]$InstallRoot = "",
    [string]$ConfigPath = "",
    [string]$DataRoot = "",
    [string]$SecurityRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-LocalAppDataPath {
    return [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
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
    $arguments = @("--endpoint", $Endpoint, "safe-reset")
    if ($NetworkOnly) {
        $arguments += "--network"
    } else {
        $arguments += "--all"
    }

    try {
        $attemptedRemoteReset = $true
        & $boundlessCtl @arguments
        if ($LASTEXITCODE -eq 0) {
            Write-Host "reset_mode=$modeName"
            Write-Host "reset_method=daemon_api"
            return
        }

        Write-Warning "boundlessctl safe-reset exited with $LASTEXITCODE; falling back to local cleanup."
    }
    catch {
        Write-Warning "boundlessctl safe-reset failed: $($_.Exception.Message). Falling back to local cleanup."
    }
}

if ($NetworkOnly) {
    Invoke-LocalNetworkReset -TargetConfigPath $ConfigPath
} else {
    Invoke-LocalFullReset -TargetConfigPath $ConfigPath -TargetDataRoot $DataRoot -TargetSecurityRoot $SecurityRoot
}

Write-Host "reset_mode=$modeName"
Write-Host "reset_method=local_cleanup"
Write-Host "attempted_remote_reset=$attemptedRemoteReset"
