param(
    [ValidateSet("quick", "smoke", "full", "trace", "recovery", "clipboard")]
    [string]$Profile = "quick",
    [int]$TimeoutSeconds = 60,
    [switch]$KeepArtifacts,
    [int]$TraceDurationSeconds = 45,
    [int]$TraceCaptureToApplyP95BudgetMs = 45,
    [int]$TraceCaptureToReceiveP95BudgetMs = 20,
    [int]$TraceCaptureToApplyJitterP95BudgetMs = 18,
    [switch]$TraceEnforceBudgets,
    [string]$TraceOutputPath = "",
    [string]$TraceMatrixCsvPath = "",
    [string]$TraceMatrixJsonPath = "",
    [string]$TraceScenario = "trace",
    [string]$TraceTopology = "",
    [switch]$TraceSkipMatrixExport,
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [string]$RecoveryResponderHost = "",
    [int]$RecoveryResponderPairingPort = 15200,
    [string]$RecoveryScenarioPrefix = "s4_recovery",
    [int]$RecoveryPendingWaitSeconds = 20,
    [int]$RecoveryPostExpiryGraceSeconds = 2,
    [int]$RecoveryEventsLimit = 300,
    [string]$RecoverySuccessCode = "",
    [ValidateSet("full", "success-only", "lockout-only", "success-and-lockout")]
    [string]$RecoveryMode = "full"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

function Invoke-CheckedCommand {
    param(
        [string]$Label,
        [scriptblock]$Action,
        [switch]$CheckLastExitCode
    )

    Write-Host "[test-suite] $Label"
    $global:LASTEXITCODE = 0
    & $Action
    $exitCode = $global:LASTEXITCODE
    if ($CheckLastExitCode -and $exitCode -ne 0) {
        throw "$Label failed with exit code $exitCode"
    }
}

function Run-WorkspaceQualityChecks {
    Invoke-CheckedCommand -Label "cargo fmt --all -- --check" -CheckLastExitCode -Action {
        cargo fmt --all -- --check | Out-Host
    }
    Invoke-CheckedCommand -Label "cargo test --workspace" -CheckLastExitCode -Action {
        cargo test --workspace | Out-Host
    }
    Invoke-CheckedCommand -Label "cargo clippy --workspace --all-targets -- -D warnings" -CheckLastExitCode -Action {
        cargo clippy --workspace --all-targets -- -D warnings | Out-Host
    }
}

function Run-TwoNodeSmoke {
    $commandParams = @{
        TimeoutSeconds = $TimeoutSeconds
    }
    if ($KeepArtifacts) {
        $commandParams.KeepArtifacts = $true
    }

    Invoke-CheckedCommand -Label "scripts/dev/two-node-smoke.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/two-node-smoke.ps1") @commandParams | Out-Host
    }
}

function Run-ThreeNodeSmoke {
    $threeNodeTimeout = [Math]::Max($TimeoutSeconds, 90)
    $commandParams = @{
        TimeoutSeconds = $threeNodeTimeout
    }
    if ($KeepArtifacts) {
        $commandParams.KeepArtifacts = $true
    }

    Invoke-CheckedCommand -Label "scripts/dev/three-node-smoke.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/three-node-smoke.ps1") @commandParams | Out-Host
    }
}

function Run-TraceCapture {
    $resolvedTraceOutputPath = $TraceOutputPath
    if ([string]::IsNullOrWhiteSpace($resolvedTraceOutputPath)) {
        $traceOutDir = Join-Path $repoRoot "artifacts/input-trace"
        $null = New-Item -ItemType Directory -Force -Path $traceOutDir
        $resolvedTraceOutputPath = Join-Path $traceOutDir ("edge-handoff-trace-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".log")
    }

    $commandParams = @{
        EndpointA = $EndpointA
        LabelA = $LabelA
        DurationSeconds = $TraceDurationSeconds
        OutputPath = $resolvedTraceOutputPath
        CaptureToApplyP95BudgetMs = $TraceCaptureToApplyP95BudgetMs
        CaptureToReceiveP95BudgetMs = $TraceCaptureToReceiveP95BudgetMs
        CaptureToApplyJitterP95BudgetMs = $TraceCaptureToApplyJitterP95BudgetMs
    }
    if ($TraceEnforceBudgets) {
        $commandParams.EnforceBudgets = $true
    }
    if (-not [string]::IsNullOrWhiteSpace($EndpointB)) {
        $commandParams.EndpointB = $EndpointB
        $commandParams.LabelB = $LabelB
    }

    Invoke-CheckedCommand -Label "scripts/dev/edge-handoff-trace.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/edge-handoff-trace.ps1") @commandParams | Out-Host
    }

    if (-not $TraceSkipMatrixExport) {
        $resolvedMatrixCsvPath = $TraceMatrixCsvPath
        if ([string]::IsNullOrWhiteSpace($resolvedMatrixCsvPath)) {
            $resolvedMatrixCsvPath = [System.IO.Path]::ChangeExtension($resolvedTraceOutputPath, ".matrix.csv")
        }
        $resolvedMatrixJsonPath = $TraceMatrixJsonPath
        if ([string]::IsNullOrWhiteSpace($resolvedMatrixJsonPath)) {
            $resolvedMatrixJsonPath = [System.IO.Path]::ChangeExtension($resolvedTraceOutputPath, ".matrix.json")
        }

        $exportParams = @{
            TracePaths = @($resolvedTraceOutputPath)
            OutputCsvPath = $resolvedMatrixCsvPath
            OutputJsonPath = $resolvedMatrixJsonPath
            Scenario = $TraceScenario
            Topology = $TraceTopology
            CaptureToApplyP95BudgetMs = $TraceCaptureToApplyP95BudgetMs
            CaptureToReceiveP95BudgetMs = $TraceCaptureToReceiveP95BudgetMs
            CaptureToApplyJitterP95BudgetMs = $TraceCaptureToApplyJitterP95BudgetMs
        }

        Invoke-CheckedCommand -Label "scripts/dev/input-trace-matrix.ps1" -Action {
            & (Join-Path $repoRoot "scripts/dev/input-trace-matrix.ps1") @exportParams | Out-Host
        }
    }
}

function Resolve-HostFromEndpoint {
    param([string]$Endpoint)

    try {
        $uri = [System.Uri]$Endpoint
        if (-not [string]::IsNullOrWhiteSpace($uri.Host)) {
            return $uri.Host
        }
    }
    catch {
    }

    return ""
}

function Run-RecoveryMatrix {
    if ([string]::IsNullOrWhiteSpace($EndpointB)) {
        throw "-Profile recovery requires -EndpointB"
    }

    $responderHost = $RecoveryResponderHost
    if ([string]::IsNullOrWhiteSpace($responderHost)) {
        $responderHost = Resolve-HostFromEndpoint -Endpoint $EndpointB
    }
    if ([string]::IsNullOrWhiteSpace($responderHost)) {
        throw "Unable to infer responder host from -EndpointB '$EndpointB'; pass -RecoveryResponderHost explicitly"
    }

    $commandParams = @{
        EndpointA = $EndpointA
        EndpointB = $EndpointB
        LabelA = $LabelA
        LabelB = $LabelB
        ResponderHost = $responderHost
        ResponderPairingPort = $RecoveryResponderPairingPort
        EventsLimit = $RecoveryEventsLimit
        ScenarioPrefix = $RecoveryScenarioPrefix
        PendingWaitSeconds = $RecoveryPendingWaitSeconds
        PostExpiryGraceSeconds = $RecoveryPostExpiryGraceSeconds
        Mode = $RecoveryMode
    }
    if (-not [string]::IsNullOrWhiteSpace($RecoverySuccessCode)) {
        $commandParams.SuccessCode = $RecoverySuccessCode
    }

    Invoke-CheckedCommand -Label "scripts/dev/s4-recovery-automation.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/s4-recovery-automation.ps1") @commandParams | Out-Host
    }
}

function Run-ClipboardMatrix {
    Invoke-CheckedCommand -Label "scripts/dev/clipboard-matrix.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/clipboard-matrix.ps1") | Out-Host
    }

    $smokeParams = @{
        TimeoutSeconds = $TimeoutSeconds
        ClipboardOnly = $true
    }
    if ($KeepArtifacts) {
        $smokeParams.KeepArtifacts = $true
    }

    Invoke-CheckedCommand -Label "scripts/dev/two-node-smoke.ps1 -ClipboardOnly" -Action {
        & (Join-Path $repoRoot "scripts/dev/two-node-smoke.ps1") @smokeParams | Out-Host
    }
}

$originalCargoIncremental = $env:CARGO_INCREMENTAL
$env:CARGO_INCREMENTAL = "0"

try {
    switch ($Profile) {
        "quick" {
            Run-WorkspaceQualityChecks
        }
        "smoke" {
            Run-WorkspaceQualityChecks
            Run-TwoNodeSmoke
        }
        "full" {
            Run-WorkspaceQualityChecks
            Run-TwoNodeSmoke
            Run-ThreeNodeSmoke
        }
        "trace" {
            Run-TraceCapture
        }
        "recovery" {
            Run-RecoveryMatrix
        }
        "clipboard" {
            Run-ClipboardMatrix
        }
    }

    Write-Host "[test-suite] complete profile=$Profile"
}
finally {
    if ($null -eq $originalCargoIncremental) {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_INCREMENTAL = $originalCargoIncremental
    }
}
