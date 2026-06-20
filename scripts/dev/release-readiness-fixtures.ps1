[CmdletBinding()]
param(
    [string]$RepoRoot = "",
    [string]$OutputRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
}
else {
    (Resolve-Path -LiteralPath $RepoRoot).Path
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/release-readiness-fixtures/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

function New-InstallerSmokeSummary {
    param(
        [string]$Path,
        [string]$ServiceVersionOutput
    )

    @{
        installer_path = "Boundless-5.0.0-windows-x64.msi"
        installer_signature = "unchecked"
        tray_signature = "unchecked"
        daemon_signature = "unchecked"
        service_signature = "unchecked"
        cli_signature = "unchecked"
        service_version_output = $ServiceVersionOutput
        service_version_exit_code = 0
        status = "passed"
    } | ConvertTo-Json | Set-Content -LiteralPath $Path -Encoding utf8
}

function Assert-GateStatus {
    param(
        [object]$Packet,
        [string]$Id,
        [string]$ExpectedStatus
    )

    $gate = @($Packet.results | Where-Object { $_.id -eq $Id })
    if ($gate.Count -ne 1) {
        throw "Expected exactly one gate '$Id', found $($gate.Count)."
    }
    if ($gate[0].status -ne $ExpectedStatus) {
        throw "Expected gate '$Id' status '$ExpectedStatus', found '$($gate[0].status)'."
    }
}

function Invoke-Fixture {
    param(
        [string]$Name,
        [string]$ServiceVersionOutput = "",
        [switch]$NoInstallerSummary,
        [switch]$RequireReady,
        [ValidateSet("stable", "prerelease")]
        [string]$Policy = "prerelease",
        [int]$ExpectedExitCode,
        [string]$ExpectedRisk,
        [string]$ExpectedInstallerStatus,
        [string]$ExpectedServiceVersionStatus
    )

    $caseRoot = Join-Path $OutputRoot $Name
    New-Item -ItemType Directory -Force -Path $caseRoot | Out-Null
    $summaryPath = Join-Path $caseRoot "installer-smoke.json"
    if (-not $NoInstallerSummary) {
        New-InstallerSmokeSummary -Path $summaryPath -ServiceVersionOutput $ServiceVersionOutput
    }

    $readinessRoot = Join-Path $caseRoot "packet"
    $arguments = @(
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        (Join-Path $repoRoot "scripts/dev/release-readiness.ps1"),
        "-SkipUnitGates",
        "-ReleaseVersion",
        "v5.0.0",
        "-Policy",
        $Policy,
        "-OutputRoot",
        $readinessRoot
    )
    if (-not $NoInstallerSummary) {
        $arguments += @("-InstallerSmokeSummaryPath", $summaryPath)
    }
    if ($RequireReady) {
        $arguments += "-RequireReady"
    }

    $captured = & powershell @arguments *>&1
    $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
    $captured | ForEach-Object { $_.ToString() } | Set-Content -LiteralPath (Join-Path $caseRoot "release-readiness.log") -Encoding utf8
    if ($exitCode -ne $ExpectedExitCode) {
        throw "Fixture '$Name' expected exit code $ExpectedExitCode, found $exitCode."
    }

    $jsonPath = Join-Path $readinessRoot "release-readiness.json"
    if (-not (Test-Path -LiteralPath $jsonPath)) {
        throw "Fixture '$Name' did not produce release-readiness.json."
    }
    $packet = Get-Content -LiteralPath $jsonPath -Raw | ConvertFrom-Json
    if ($packet.risk_classification -ne $ExpectedRisk) {
        throw "Fixture '$Name' expected risk '$ExpectedRisk', found '$($packet.risk_classification)'."
    }

    Assert-GateStatus -Packet $packet -Id "installer_smoke" -ExpectedStatus $ExpectedInstallerStatus
    Assert-GateStatus -Packet $packet -Id "service_version_parity" -ExpectedStatus $ExpectedServiceVersionStatus
    Write-Host "fixture=$Name exit_code=$exitCode risk=$($packet.risk_classification) service_version_parity=$ExpectedServiceVersionStatus"
}

Invoke-Fixture -Name "exact_stable_version_passes" -ServiceVersionOutput "boundless-service 5.0.0" -ExpectedExitCode 0 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed"
Invoke-Fixture -Name "substring_version_fails" -ServiceVersionOutput "boundless-service 15.0.0" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed"
Invoke-Fixture -Name "prerelease_service_version_fails_for_stable_release" -ServiceVersionOutput "boundless-service 5.0.0-rc" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed"
Invoke-Fixture -Name "empty_service_version_output_fails" -ServiceVersionOutput "" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed"
Invoke-Fixture -Name "missing_installer_summary_skips_and_require_ready_fails" -NoInstallerSummary -RequireReady -ExpectedExitCode 1 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "skipped" -ExpectedServiceVersionStatus "skipped"
Invoke-Fixture -Name "missing_installer_summary_stable_policy_fails" -NoInstallerSummary -Policy stable -ExpectedExitCode 1 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "skipped" -ExpectedServiceVersionStatus "skipped"

Write-Host "release_readiness_fixtures=passed artifacts=$OutputRoot"
