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
        [string]$ServiceVersionOutput,
        [string]$UpgradedFrom = "",
        [object]$PreviousInstallExitCode = $null
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
        upgraded_from = $UpgradedFrom
        previous_install_exit_code = $PreviousInstallExitCode
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
        [string]$UpgradedFrom = "",
        [object]$PreviousInstallExitCode = $null,
        [switch]$NoInstallerSummary,
        [switch]$RequireReady,
        [ValidateSet("stable", "prerelease")]
        [string]$Policy = "prerelease",
        [ValidateSet("msi-owned", "service-self-update", "tray-self-update")]
        [string]$ServiceUpdateMode = "msi-owned",
        [int]$ExpectedExitCode,
        [string]$ExpectedRisk,
        [string]$ExpectedInstallerStatus,
        [string]$ExpectedServiceVersionStatus,
        [string]$ExpectedServiceUpdateOwnershipStatus,
        [string]$ExpectedNMinusOneStatus
    )

    $caseRoot = Join-Path $OutputRoot $Name
    New-Item -ItemType Directory -Force -Path $caseRoot | Out-Null
    $summaryPath = Join-Path $caseRoot "installer-smoke.json"
    if (-not $NoInstallerSummary) {
        New-InstallerSmokeSummary -Path $summaryPath -ServiceVersionOutput $ServiceVersionOutput -UpgradedFrom $UpgradedFrom -PreviousInstallExitCode $PreviousInstallExitCode
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
        "-ServiceUpdateMode",
        $ServiceUpdateMode,
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
    Assert-GateStatus -Packet $packet -Id "service_update_ownership" -ExpectedStatus $ExpectedServiceUpdateOwnershipStatus
    Assert-GateStatus -Packet $packet -Id "n_minus_1_msi_upgrade" -ExpectedStatus $ExpectedNMinusOneStatus
    if ($packet.service_update_mode -ne $ServiceUpdateMode) {
        throw "Fixture '$Name' expected service_update_mode '$ServiceUpdateMode', found '$($packet.service_update_mode)'."
    }
    Write-Host "fixture=$Name exit_code=$exitCode risk=$($packet.risk_classification) service_version_parity=$ExpectedServiceVersionStatus n_minus_1_msi_upgrade=$ExpectedNMinusOneStatus"
}

Invoke-Fixture -Name "exact_stable_version_passes_without_n_minus_one" -ServiceVersionOutput "boundless-service 5.0.0" -ExpectedExitCode 0 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "n_minus_one_msi_upgrade_passes" -ServiceVersionOutput "boundless-service 5.0.0" -UpgradedFrom "Boundless-4.0.2-windows-x64.msi" -PreviousInstallExitCode 0 -ExpectedExitCode 0 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "passed"
Invoke-Fixture -Name "stable_policy_requires_n_minus_one_msi_evidence" -ServiceVersionOutput "boundless-service 5.0.0" -Policy stable -ExpectedExitCode 1 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "unsupported_service_self_update_fails" -ServiceVersionOutput "boundless-service 5.0.0" -ServiceUpdateMode "service-self-update" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "failed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "unsupported_tray_self_update_fails" -ServiceVersionOutput "boundless-service 5.0.0" -ServiceUpdateMode "tray-self-update" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "failed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "missing_previous_install_exit_code_fails" -ServiceVersionOutput "boundless-service 5.0.0" -UpgradedFrom "Boundless-4.0.2-windows-x64.msi" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "empty_previous_install_exit_code_fails" -ServiceVersionOutput "boundless-service 5.0.0" -UpgradedFrom "Boundless-4.0.2-windows-x64.msi" -PreviousInstallExitCode "" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "malformed_previous_install_exit_code_fails" -ServiceVersionOutput "boundless-service 5.0.0" -UpgradedFrom "Boundless-4.0.2-windows-x64.msi" -PreviousInstallExitCode "zero" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "failed_prior_msi_install_fails" -ServiceVersionOutput "boundless-service 5.0.0" -UpgradedFrom "Boundless-4.0.2-windows-x64.msi" -PreviousInstallExitCode 1603 -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "passed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "failed"
Invoke-Fixture -Name "substring_version_fails" -ServiceVersionOutput "boundless-service 15.0.0" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "prerelease_service_version_fails_for_stable_release" -ServiceVersionOutput "boundless-service 5.0.0-rc" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "empty_service_version_output_fails" -ServiceVersionOutput "" -ExpectedExitCode 1 -ExpectedRisk "blocked" -ExpectedInstallerStatus "passed" -ExpectedServiceVersionStatus "failed" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "missing_installer_summary_skips_and_require_ready_fails" -NoInstallerSummary -RequireReady -ExpectedExitCode 1 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "skipped" -ExpectedServiceVersionStatus "skipped" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"
Invoke-Fixture -Name "missing_installer_summary_stable_policy_fails" -NoInstallerSummary -Policy stable -ExpectedExitCode 1 -ExpectedRisk "at-risk" -ExpectedInstallerStatus "skipped" -ExpectedServiceVersionStatus "skipped" -ExpectedServiceUpdateOwnershipStatus "passed" -ExpectedNMinusOneStatus "skipped"

Write-Host "release_readiness_fixtures=passed artifacts=$OutputRoot"
