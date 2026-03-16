param(
    [string[]]$TracePaths = @(),
    [string]$TraceDir = "",
    [string]$OutputCsvPath = "",
    [string]$OutputJsonPath = "",
    [string]$Scenario = "trace",
    [string]$Topology = "",
    [int]$CaptureToApplyP95BudgetMs = 25,
    [int]$CaptureToReceiveP95BudgetMs = 10,
    [int]$ReceiveToApplyP95BudgetMs = 8,
    [int]$CaptureToApplyJitterP95BudgetMs = 10
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

function Ensure-ParentDirectory {
    param([string]$Path)
    $dir = Split-Path -Parent $Path
    if (-not [string]::IsNullOrWhiteSpace($dir)) {
        $null = New-Item -ItemType Directory -Force -Path $dir
    }
}

function Parse-KeyValues {
    param([string]$Line)

    $map = @{}
    if ([string]::IsNullOrWhiteSpace($Line)) {
        return $map
    }

    $matches = [regex]::Matches($Line, "([A-Za-z0-9_]+)=([^\s]+)")
    foreach ($match in $matches) {
        $key = $match.Groups[1].Value
        $value = $match.Groups[2].Value
        $map[$key] = $value
    }
    return $map
}

function Get-IntOrNull {
    param(
        [hashtable]$Values,
        [string]$Key
    )
    if (-not $Values.ContainsKey($Key)) {
        return $null
    }
    $parsed = 0
    if ([int]::TryParse($Values[$Key], [ref]$parsed)) {
        return $parsed
    }
    return $null
}

if ($TracePaths.Count -eq 0) {
    if ([string]::IsNullOrWhiteSpace($TraceDir)) {
        $TraceDir = Join-Path $repoRoot "artifacts/input-trace"
    }
    if (-not (Test-Path $TraceDir)) {
        throw "trace directory not found: $TraceDir"
    }

    $TracePaths = Get-ChildItem -Path $TraceDir -Filter "*.log" -File |
        Sort-Object LastWriteTimeUtc |
        ForEach-Object { $_.FullName }
}

if ($TracePaths.Count -eq 0) {
    throw "no trace files found"
}

if ([string]::IsNullOrWhiteSpace($OutputCsvPath) -or [string]::IsNullOrWhiteSpace($OutputJsonPath)) {
    $outDir = Join-Path $repoRoot "artifacts/input-trace"
    $null = New-Item -ItemType Directory -Force -Path $outDir
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    if ([string]::IsNullOrWhiteSpace($OutputCsvPath)) {
        $OutputCsvPath = Join-Path $outDir "input-latency-matrix-$stamp.csv"
    }
    if ([string]::IsNullOrWhiteSpace($OutputJsonPath)) {
        $OutputJsonPath = Join-Path $outDir "input-latency-matrix-$stamp.json"
    }
}

$rows = New-Object System.Collections.Generic.List[object]
$topologyValue = if ([string]::IsNullOrWhiteSpace($Topology)) { "unspecified" } else { $Topology }

foreach ($tracePath in $TracePaths) {
    if (-not (Test-Path $tracePath)) {
        throw "trace file not found: $tracePath"
    }

    $fullPath = (Resolve-Path $tracePath).Path
    $lines = Get-Content -Path $fullPath
    $traceStartLine = $lines | Where-Object { $_ -match "\strace_start\s" } | Select-Object -First 1
    $traceStartValues = Parse-KeyValues -Line $traceStartLine
    $commit = if ($traceStartValues.ContainsKey("commit")) { $traceStartValues["commit"] } else { "unknown" }
    $traceStartedAt = if ([string]::IsNullOrWhiteSpace($traceStartLine)) {
        ""
    }
    else {
        ($traceStartLine -split "\s+", 2)[0]
    }

    $summaryLines = @($lines | Where-Object { $_ -match "\slatency_summary\s" })
    if ($summaryLines.Count -eq 0) {
        throw "trace file has no latency_summary lines: $fullPath"
    }

    foreach ($line in $summaryLines) {
        $values = Parse-KeyValues -Line $line
        $label = if ($values.ContainsKey("label")) { $values["label"] } else { "unknown" }
        $summaryTimestamp = ($line -split "\s+", 2)[0]

        $applyCount = Get-IntOrNull -Values $values -Key "capture_to_apply_count"
        $applyP50 = Get-IntOrNull -Values $values -Key "capture_to_apply_p50"
        $applyP95 = Get-IntOrNull -Values $values -Key "capture_to_apply_p95"
        $applyP99 = Get-IntOrNull -Values $values -Key "capture_to_apply_p99"
        $applyMax = Get-IntOrNull -Values $values -Key "capture_to_apply_max"
        $applyJitterP95 = Get-IntOrNull -Values $values -Key "capture_to_apply_jitter_p95"
        $receiveCount = Get-IntOrNull -Values $values -Key "capture_to_receive_count"
        $receiveP50 = Get-IntOrNull -Values $values -Key "capture_to_receive_p50"
        $receiveP95 = Get-IntOrNull -Values $values -Key "capture_to_receive_p95"
        $receiveP99 = Get-IntOrNull -Values $values -Key "capture_to_receive_p99"
        $receiveMax = Get-IntOrNull -Values $values -Key "capture_to_receive_max"
        $receiveToApplyCount = Get-IntOrNull -Values $values -Key "receive_to_apply_count"
        $receiveToApplyP50 = Get-IntOrNull -Values $values -Key "receive_to_apply_p50"
        $receiveToApplyP95 = Get-IntOrNull -Values $values -Key "receive_to_apply_p95"
        $receiveToApplyP99 = Get-IntOrNull -Values $values -Key "receive_to_apply_p99"
        $receiveToApplyMax = Get-IntOrNull -Values $values -Key "receive_to_apply_max"
        $receiveToApplyJitterP95 = Get-IntOrNull -Values $values -Key "receive_to_apply_jitter_p95"
        $estimatedClockSkewMs = Get-IntOrNull -Values $values -Key "clock_skew_estimated_ms"

        $violations = New-Object System.Collections.Generic.List[string]
        $applyCountValue = if ($null -eq $applyCount) { 0 } else { $applyCount }
        $receiveCountValue = if ($null -eq $receiveCount) { 0 } else { $receiveCount }
        $effectiveApplyP95 = $applyP95
        $effectiveReceiveP95 = $receiveP95
        $effectiveJitterP95 = $applyJitterP95
        $applyBudgetMetric = "capture_to_apply_p95"
        $receiveBudgetMetric = "capture_to_receive_p95"
        $jitterBudgetMetric = "capture_to_apply_jitter_p95"

        if ($null -ne $estimatedClockSkewMs) {
            if ($null -ne $receiveToApplyP95) {
                $effectiveApplyP95 = $receiveToApplyP95
                $applyBudgetMetric = "receive_to_apply_p95"
            }
            $effectiveReceiveP95 = $null
            $receiveBudgetMetric = "capture_to_receive_p95_skipped_clock_skew_suspected"
            if ($null -ne $receiveToApplyJitterP95) {
                $effectiveJitterP95 = $receiveToApplyJitterP95
                $jitterBudgetMetric = "receive_to_apply_jitter_p95"
            }
        }

        if ($applyCountValue -le 0) {
            $violations.Add("no_capture_to_apply_samples")
        }
        if ($receiveCountValue -le 0) {
            $violations.Add("no_capture_to_receive_samples")
        }
        if ($null -eq $receiveToApplyCount -or $receiveToApplyCount -le 0) {
            $violations.Add("no_receive_to_apply_samples")
        }
        if ($null -ne $effectiveApplyP95 -and $effectiveApplyP95 -gt $CaptureToApplyP95BudgetMs) {
            $violations.Add($applyBudgetMetric)
        }
        if ($null -ne $effectiveReceiveP95 -and $effectiveReceiveP95 -gt $CaptureToReceiveP95BudgetMs) {
            $violations.Add($receiveBudgetMetric)
        }
        if ($null -ne $receiveToApplyP95 -and $receiveToApplyP95 -gt $ReceiveToApplyP95BudgetMs) {
            $violations.Add("receive_to_apply_p95")
        }
        if ($null -ne $effectiveJitterP95 -and $effectiveJitterP95 -gt $CaptureToApplyJitterP95BudgetMs) {
            $violations.Add($jitterBudgetMetric)
        }

        $rows.Add([pscustomobject]@{
                trace_file                              = $fullPath
                trace_started_at                        = $traceStartedAt
                summary_timestamp                       = $summaryTimestamp
                commit                                  = $commit
                scenario                                = $Scenario
                topology                                = $topologyValue
                label                                   = $label
                result                                  = if ($violations.Count -eq 0) { "pass" } else { "fail" }
                budget_violations                       = ($violations -join ";")
                capture_to_apply_count                  = $applyCount
                capture_to_apply_p50_ms                 = $applyP50
                capture_to_apply_p95_ms                 = $applyP95
                capture_to_apply_p99_ms                 = $applyP99
                capture_to_apply_max_ms                 = $applyMax
                capture_to_apply_jitter_p95_ms          = $applyJitterP95
                capture_to_receive_count                = $receiveCount
                capture_to_receive_p50_ms               = $receiveP50
                capture_to_receive_p95_ms               = $receiveP95
                capture_to_receive_p99_ms               = $receiveP99
                capture_to_receive_max_ms               = $receiveMax
                receive_to_apply_count                  = $receiveToApplyCount
                receive_to_apply_p50_ms                 = $receiveToApplyP50
                receive_to_apply_p95_ms                 = $receiveToApplyP95
                receive_to_apply_p99_ms                 = $receiveToApplyP99
                receive_to_apply_max_ms                 = $receiveToApplyMax
                receive_to_apply_jitter_p95_ms          = $receiveToApplyJitterP95
                clock_skew_estimated_ms                 = $estimatedClockSkewMs
                capture_to_apply_p95_effective_ms       = $effectiveApplyP95
                capture_to_receive_p95_effective_ms     = $effectiveReceiveP95
                capture_to_apply_jitter_p95_effective_ms= $effectiveJitterP95
                capture_to_apply_budget_metric          = $applyBudgetMetric
                capture_to_receive_budget_metric        = $receiveBudgetMetric
                capture_to_apply_jitter_budget_metric   = $jitterBudgetMetric
                budget_capture_to_apply_p95_ms          = $CaptureToApplyP95BudgetMs
                budget_capture_to_receive_p95_ms        = $CaptureToReceiveP95BudgetMs
                budget_receive_to_apply_p95_ms          = $ReceiveToApplyP95BudgetMs
                budget_capture_to_apply_jitter_p95_ms   = $CaptureToApplyJitterP95BudgetMs
            })
    }
}

if ($rows.Count -eq 0) {
    throw "no matrix rows generated"
}

$sortedRows = @($rows |
    Sort-Object trace_started_at, label, trace_file
)

Ensure-ParentDirectory -Path $OutputCsvPath
Ensure-ParentDirectory -Path $OutputJsonPath

$sortedRows | Export-Csv -Path $OutputCsvPath -NoTypeInformation
(@($sortedRows) | ConvertTo-Json -Depth 5) | Set-Content -Path $OutputJsonPath -Encoding UTF8

$passCount = @($sortedRows | Where-Object { $_.result -eq "pass" }).Count
$failCount = @($sortedRows | Where-Object { $_.result -eq "fail" }).Count

Write-Host "[matrix] rows=$($sortedRows.Count) pass=$passCount fail=$failCount"
Write-Host "[matrix] csv=$OutputCsvPath"
Write-Host "[matrix] json=$OutputJsonPath"
