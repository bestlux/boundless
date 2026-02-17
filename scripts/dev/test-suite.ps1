param(
    [ValidateSet("quick", "smoke", "full", "trace")]
    [string]$Profile = "smoke",
    [int]$TimeoutSeconds = 60,
    [switch]$KeepArtifacts,
    [int]$TraceDurationSeconds = 45,
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

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
    $commandParams = @{
        EndpointA = $EndpointA
        LabelA = $LabelA
        DurationSeconds = $TraceDurationSeconds
    }
    if (-not [string]::IsNullOrWhiteSpace($EndpointB)) {
        $commandParams.EndpointB = $EndpointB
        $commandParams.LabelB = $LabelB
    }

    Invoke-CheckedCommand -Label "scripts/dev/edge-handoff-trace.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/edge-handoff-trace.ps1") @commandParams | Out-Host
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
