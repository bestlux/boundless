param(
    [string]$OutputDir = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $repoRoot ("artifacts/clipboard-validation/" + (Get-Date -Format "yyyyMMdd-HHmmss"))
}

$null = New-Item -ItemType Directory -Force -Path $OutputDir
$results = [System.Collections.Generic.List[object]]::new()

function Write-MatrixResults {
    $csvPath = Join-Path $OutputDir "clipboard-matrix.csv"
    $jsonPath = Join-Path $OutputDir "clipboard-matrix.json"
    $results | Export-Csv -Path $csvPath -NoTypeInformation
    $results | ConvertTo-Json -Depth 4 | Set-Content -Path $jsonPath
}

function Invoke-MatrixScenario {
    param(
        [string]$Name,
        [string]$Category,
        [string]$Description,
        [scriptblock]$Action
    )

    $logPath = Join-Path $OutputDir ($Name + ".log")
    $startedAt = Get-Date
    $status = "passed"
    $failure = ""

    Write-Host "[clipboard-matrix] $Name"
    try {
        $global:LASTEXITCODE = 0
        & $Action 2>&1 | Tee-Object -FilePath $logPath | Out-Host
        if ($global:LASTEXITCODE -ne 0) {
            throw "command exited with code $global:LASTEXITCODE"
        }
    }
    catch {
        $status = "failed"
        $failure = $_.Exception.Message
    }

    $results.Add([pscustomobject]@{
        scenario = $Name
        category = $Category
        status = $status
        description = $Description
        log_path = $logPath
        started_at = $startedAt.ToString("o")
        duration_seconds = [Math]::Round(((Get-Date) - $startedAt).TotalSeconds, 2)
        failure = $failure
    })
    Write-MatrixResults

    if ($status -ne "passed") {
        throw "[clipboard-matrix] scenario failed: $Name :: $failure"
    }
}

$originalCargoIncremental = $env:CARGO_INCREMENTAL
$env:CARGO_INCREMENTAL = "0"

try {
    Invoke-MatrixScenario -Name "text_reconnect_replay" -Category "unit" -Description "Disconnected local text snapshot is replayed after reconnect" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon clipboard_sync_persists_disconnected_local_text_for_replay_without_immediate_queueing -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "image_reconnect_replay" -Category "unit" -Description "Disconnected local image snapshot is replayed after reconnect" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon clipboard_sync_persists_disconnected_local_image_for_replay -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "remote_apply_retry" -Category "unit" -Description "Remote clipboard apply failures requeue and retry on the next tick" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon clipboard_tick_requeues_remote_apply_failures_and_retries_next_tick -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "pending_replay_canceled_by_live_remote" -Category "unit" -Description "A live remote payload cancels a stale pending replay" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon remote_clipboard_apply_cancels_pending_replay -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "invalid_bmp_rejection" -Category "unit" -Description "Invalid BMP payloads are rejected by clipboard validation" -Action {
        cmd /d /s /c "cargo test -p core-clipboard rejects_truncated_bmp -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "oversized_image_rejection" -Category "unit" -Description "Clipboard image policy rejects payloads above the configured size ceiling" -Action {
        cmd /d /s /c "cargo test -p core-clipboard rejects_large_image -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "chunk_interruption_requeue" -Category "unit" -Description "Chunked clipboard image transfer requeues the logical payload on mid-transfer failure" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon flush_requeues_chunked_clipboard_image_on_mid_transfer_failure -- --nocapture 2>&1"
    }

    Invoke-MatrixScenario -Name "chunk_hash_mismatch_rejection" -Category "unit" -Description "Chunked clipboard image reassembly rejects hash mismatches" -Action {
        cmd /d /s /c "cargo test -p boundless-daemon chunked_clipboard_image_transfer_rejects_hash_mismatch -- --nocapture 2>&1"
    }
    Write-MatrixResults
    Write-Host "[clipboard-matrix] artifacts written to $OutputDir"
}
finally {
    if ($null -eq $originalCargoIncremental) {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_INCREMENTAL = $originalCargoIncremental
    }
}
