param(
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [int]$DurationSeconds = 30,
    [int]$PollMilliseconds = 150,
    [int]$EventsLimit = 200,
    [string]$OutputPath = "",
    [int]$CaptureToApplyP95BudgetMs = 25,
    [int]$CaptureToReceiveP95BudgetMs = 10,
    [int]$ReceiveToApplyP95BudgetMs = 8,
    [int]$CaptureToApplyJitterP95BudgetMs = 10,
    [int]$ClockSkewThresholdMs = 500,
    [bool]$AdjustForClockSkew = $true,
    [switch]$EnforceBudgets
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"

if (-not (Test-Path $cliExe)) {
    throw "boundlessctl binary not found at $cliExe; run cargo build -p boundless-cli first"
}
if ($DurationSeconds -le 0) {
    throw "DurationSeconds must be > 0"
}
if ($PollMilliseconds -lt 50) {
    throw "PollMilliseconds must be >= 50"
}
if ($EventsLimit -le 0) {
    throw "EventsLimit must be > 0"
}
if ($ClockSkewThresholdMs -lt 0) {
    throw "ClockSkewThresholdMs must be >= 0"
}

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $outDir = Join-Path $repoRoot "artifacts/input-trace"
    $null = New-Item -ItemType Directory -Force -Path $outDir
    $OutputPath = Join-Path $outDir ("edge-handoff-trace-" + (Get-Date -Format "yyyyMMdd-HHmmss") + ".log")
}

$outputDir = Split-Path -Parent $OutputPath
if (-not [string]::IsNullOrWhiteSpace($outputDir)) {
    $null = New-Item -ItemType Directory -Force -Path $outputDir
}

function Write-TraceLine {
    param([string]$Text)
    $stamp = (Get-Date).ToString("o")
    $line = "$stamp $Text"
    Add-Content -Path $OutputPath -Value $line
}

function Invoke-CliUnchecked {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $allArgs = @("--endpoint", $Endpoint) + $CommandArgs
    $output = & $cliExe @allArgs 2>&1
    return @{
        ExitCode = $LASTEXITCODE
        Output = [string]$output
    }
}

function Parse-CaptureTarget {
    param([string]$Output)
    $match = [regex]::Match($Output, "target=([^\s]+)")
    if ($match.Success) {
        return $match.Groups[1].Value
    }
    return "unknown"
}

function Get-MetricValue {
    param(
        [string]$Line,
        [string]$MetricName
    )
    $match = [regex]::Match($Line, "$MetricName=([0-9]+)")
    if (-not $match.Success) {
        return $null
    }
    return [int]$match.Groups[1].Value
}

function Get-Percentile {
    param(
        [int[]]$Values,
        [double]$Percentile
    )
    if ($null -eq $Values -or $Values.Count -eq 0) {
        return $null
    }
    $sorted = $Values | Sort-Object
    $rank = [Math]::Ceiling(($Percentile / 100.0) * $sorted.Count)
    $index = [Math]::Max(0, [Math]::Min($sorted.Count - 1, $rank - 1))
    return [int]$sorted[$index]
}

function Get-JitterP95 {
    param([int[]]$Series)
    if ($null -eq $Series -or $Series.Count -lt 2) {
        return $null
    }

    $deltas = New-Object System.Collections.Generic.List[int]
    for ($i = 1; $i -lt $Series.Count; $i++) {
        $deltas.Add([Math]::Abs($Series[$i] - $Series[$i - 1]))
    }
    return Get-Percentile -Values $deltas.ToArray() -Percentile 95
}

function Summarize-Metrics {
    param(
        [string]$Label,
        [System.Collections.Generic.List[int]]$CaptureToApply,
        [System.Collections.Generic.List[int]]$CaptureToReceive,
        [System.Collections.Generic.List[int]]$ReceiveToApply,
        [int]$ClockSkewThresholdMs
    )

    $applyValues = $CaptureToApply.ToArray()
    $receiveValues = $CaptureToReceive.ToArray()
    $receiveToApplyValues = $ReceiveToApply.ToArray()
    $applyP50 = Get-Percentile -Values $applyValues -Percentile 50
    $applyP95 = Get-Percentile -Values $applyValues -Percentile 95
    $applyP99 = Get-Percentile -Values $applyValues -Percentile 99
    $receiveP50 = Get-Percentile -Values $receiveValues -Percentile 50
    $receiveP95 = Get-Percentile -Values $receiveValues -Percentile 95
    $receiveP99 = Get-Percentile -Values $receiveValues -Percentile 99
    $receiveToApplyP50 = Get-Percentile -Values $receiveToApplyValues -Percentile 50
    $receiveToApplyP95 = Get-Percentile -Values $receiveToApplyValues -Percentile 95
    $receiveToApplyP99 = Get-Percentile -Values $receiveToApplyValues -Percentile 99
    $receiveToApplyJitterP95 = Get-JitterP95 -Series $receiveToApplyValues
    $applyJitterP95 = Get-JitterP95 -Series $applyValues
    $applyMax = if ($applyValues.Count -gt 0) { ($applyValues | Measure-Object -Maximum).Maximum } else { $null }
    $receiveMax = if ($receiveValues.Count -gt 0) { ($receiveValues | Measure-Object -Maximum).Maximum } else { $null }
    $receiveToApplyMax = if ($receiveToApplyValues.Count -gt 0) { ($receiveToApplyValues | Measure-Object -Maximum).Maximum } else { $null }
    $estimatedClockSkewMs = $null
    if ($null -ne $applyP50 -and $null -ne $receiveToApplyP50) {
        $delta = $applyP50 - $receiveToApplyP50
        if ($delta -ge $ClockSkewThresholdMs) {
            $estimatedClockSkewMs = $delta
        }
    }

    return @{
        Label = $Label
        CaptureToApplyCount = $applyValues.Count
        CaptureToReceiveCount = $receiveValues.Count
        ReceiveToApplyCount = $receiveToApplyValues.Count
        CaptureToApplyP50 = $applyP50
        CaptureToApplyP95 = $applyP95
        CaptureToApplyP99 = $applyP99
        CaptureToApplyMax = $applyMax
        CaptureToApplyJitterP95 = $applyJitterP95
        CaptureToReceiveP50 = $receiveP50
        CaptureToReceiveP95 = $receiveP95
        CaptureToReceiveP99 = $receiveP99
        CaptureToReceiveMax = $receiveMax
        ReceiveToApplyP50 = $receiveToApplyP50
        ReceiveToApplyP95 = $receiveToApplyP95
        ReceiveToApplyP99 = $receiveToApplyP99
        ReceiveToApplyMax = $receiveToApplyMax
        ReceiveToApplyJitterP95 = $receiveToApplyJitterP95
        EstimatedClockSkewMs = $estimatedClockSkewMs
    }
}

$targets = @(
    @{ Endpoint = $EndpointA; Label = $LabelA }
)
if (-not [string]::IsNullOrWhiteSpace($EndpointB)) {
    $targets += @{ Endpoint = $EndpointB; Label = $LabelB }
}

$states = @{}
$seenEvents = @{}
$metricsByLabel = @{}

$commit = (& git rev-parse --short HEAD 2>$null)
if ($LASTEXITCODE -ne 0) {
    $commit = "unknown"
}

Set-Content -Path $OutputPath -Value ""
Write-TraceLine "trace_start duration_s=$DurationSeconds poll_ms=$PollMilliseconds events_limit=$EventsLimit commit=$commit"
foreach ($target in $targets) {
    $states[$target.Label] = @{
        LastCaptureTarget = $null
        LastStatus = $null
        NextStatusAt = Get-Date
    }
    $metricsByLabel[$target.Label] = @{
        CaptureToApply = New-Object System.Collections.Generic.List[int]
        CaptureToReceive = New-Object System.Collections.Generic.List[int]
        ReceiveToApply = New-Object System.Collections.Generic.List[int]
    }
    Write-TraceLine "target label=$($target.Label) endpoint=$($target.Endpoint)"

    $feature = Invoke-CliUnchecked -Endpoint $target.Endpoint -CommandArgs @("feature", "list")
    if ($feature.ExitCode -eq 0) {
        Write-TraceLine "snapshot label=$($target.Label) feature_list=$(($feature.Output -replace '\r?\n',' | ').Trim())"
    }

    $inputStatus = Invoke-CliUnchecked -Endpoint $target.Endpoint -CommandArgs @("input", "status")
    if ($inputStatus.ExitCode -eq 0) {
        Write-TraceLine "snapshot label=$($target.Label) input_status=$(($inputStatus.Output -replace '\r?\n',' | ').Trim())"
    }

    $layout = Invoke-CliUnchecked -Endpoint $target.Endpoint -CommandArgs @("layout", "show")
    if ($layout.ExitCode -eq 0) {
        Write-TraceLine "snapshot label=$($target.Label) layout_show=$(($layout.Output -replace '\r?\n',' | ').Trim())"
    }
}

$interestingKinds = @(
    "input_handoff",
    "input_escape_triggered",
    "input_lock_engaged",
    "input_lock_released",
    "input_capture_backend_mode",
    "input_frame",
    "input_inject_applied",
    "input_inject_failed",
    "input_inject_skipped"
)
$interestingPattern = "kind=({0})" -f ($interestingKinds -join "|")

$deadline = (Get-Date).AddSeconds($DurationSeconds)
while ((Get-Date) -lt $deadline) {
    foreach ($target in $targets) {
        $label = $target.Label
        $endpoint = $target.Endpoint
        $state = $states[$label]

        $capture = Invoke-CliUnchecked -Endpoint $endpoint -CommandArgs @("input", "capture-target")
        if ($capture.ExitCode -eq 0) {
            $captureTarget = Parse-CaptureTarget -Output $capture.Output
            if ($captureTarget -ne $state.LastCaptureTarget) {
                Write-TraceLine "capture_target_changed label=$label endpoint=$endpoint target=$captureTarget raw=$(($capture.Output -replace '\r?\n',' | ').Trim())"
                $state.LastCaptureTarget = $captureTarget
            }
        }
        else {
            Write-TraceLine "capture_target_error label=$label endpoint=$endpoint exit=$($capture.ExitCode) output=$(($capture.Output -replace '\r?\n',' | ').Trim())"
        }

        if ((Get-Date) -ge $state.NextStatusAt) {
            $status = Invoke-CliUnchecked -Endpoint $endpoint -CommandArgs @("daemon", "status")
            if ($status.ExitCode -eq 0) {
                $normalizedStatus = ($status.Output -replace '\r?\n', ' | ').Trim()
                if ($normalizedStatus -ne $state.LastStatus) {
                    Write-TraceLine "daemon_status label=$label endpoint=$endpoint $normalizedStatus"
                    $state.LastStatus = $normalizedStatus
                }
            }
            else {
                Write-TraceLine "daemon_status_error label=$label endpoint=$endpoint exit=$($status.ExitCode) output=$(($status.Output -replace '\r?\n',' | ').Trim())"
            }
            $state.NextStatusAt = (Get-Date).AddMilliseconds(1000)
        }

        $events = Invoke-CliUnchecked -Endpoint $endpoint -CommandArgs @("transport", "events", "--limit", "$EventsLimit")
        if ($events.ExitCode -eq 0) {
            $lines = $events.Output -split "`r?`n"
            foreach ($line in $lines) {
                if ([string]::IsNullOrWhiteSpace($line)) {
                    continue
                }
                if ($line -notmatch $interestingPattern) {
                    continue
                }
                $dedupeKey = "$label|$line"
                if ($seenEvents.ContainsKey($dedupeKey)) {
                    continue
                }
                $seenEvents[$dedupeKey] = $true
                Write-TraceLine "event label=$label endpoint=$endpoint $line"

                $metrics = $metricsByLabel[$label]
                if ($line -match "kind=input_inject_applied") {
                    $captureToApply = Get-MetricValue -Line $line -MetricName "capture_to_apply_ms"
                    if ($null -ne $captureToApply) {
                        $metrics.CaptureToApply.Add($captureToApply)
                    }
                    $receiveToApply = Get-MetricValue -Line $line -MetricName "receive_to_apply_ms"
                    if ($null -ne $receiveToApply) {
                        $metrics.ReceiveToApply.Add($receiveToApply)
                    }
                }
                if ($line -match "kind=input_frame") {
                    $captureToReceive = Get-MetricValue -Line $line -MetricName "capture_to_receive_ms"
                    if ($null -ne $captureToReceive) {
                        $metrics.CaptureToReceive.Add($captureToReceive)
                    }
                }
                $metricsByLabel[$label] = $metrics
            }
        }
        else {
            Write-TraceLine "transport_events_error label=$label endpoint=$endpoint exit=$($events.ExitCode) output=$(($events.Output -replace '\r?\n',' | ').Trim())"
        }

        $states[$label] = $state
    }

    Start-Sleep -Milliseconds $PollMilliseconds
}

foreach ($target in $targets) {
    $label = $target.Label
    $metric = $metricsByLabel[$label]
    $summary = Summarize-Metrics `
        -Label $label `
        -CaptureToApply $metric.CaptureToApply `
        -CaptureToReceive $metric.CaptureToReceive `
        -ReceiveToApply $metric.ReceiveToApply `
        -ClockSkewThresholdMs $ClockSkewThresholdMs

    Write-TraceLine (
        "latency_summary label={0} capture_to_apply_count={1} capture_to_apply_p50={2} capture_to_apply_p95={3} capture_to_apply_p99={4} capture_to_apply_max={5} capture_to_apply_jitter_p95={6} capture_to_receive_count={7} capture_to_receive_p50={8} capture_to_receive_p95={9} capture_to_receive_p99={10} capture_to_receive_max={11} receive_to_apply_count={12} receive_to_apply_p50={13} receive_to_apply_p95={14} receive_to_apply_p99={15} receive_to_apply_max={16} receive_to_apply_jitter_p95={17} clock_skew_estimated_ms={18}" -f
        $summary.Label,
        $summary.CaptureToApplyCount,
        $summary.CaptureToApplyP50,
        $summary.CaptureToApplyP95,
        $summary.CaptureToApplyP99,
        $summary.CaptureToApplyMax,
        $summary.CaptureToApplyJitterP95,
        $summary.CaptureToReceiveCount,
        $summary.CaptureToReceiveP50,
        $summary.CaptureToReceiveP95,
        $summary.CaptureToReceiveP99,
        $summary.CaptureToReceiveMax,
        $summary.ReceiveToApplyCount,
        $summary.ReceiveToApplyP50,
        $summary.ReceiveToApplyP95,
        $summary.ReceiveToApplyP99,
        $summary.ReceiveToApplyMax,
        $summary.ReceiveToApplyJitterP95,
        $summary.EstimatedClockSkewMs
    )

    if ($EnforceBudgets) {
        $applyMetricName = "capture_to_apply_p95"
        $effectiveApplyP95 = $summary.CaptureToApplyP95
        $receiveMetricName = "capture_to_receive_p95"
        $effectiveReceiveP95 = $summary.CaptureToReceiveP95
        $jitterMetricName = "capture_to_apply_jitter_p95"
        $effectiveJitterP95 = $summary.CaptureToApplyJitterP95
        if ($AdjustForClockSkew -and $null -ne $summary.EstimatedClockSkewMs) {
            Write-TraceLine "clock_skew_detected label=$label estimated_ms=$($summary.EstimatedClockSkewMs) apply_p95_raw=$($summary.CaptureToApplyP95) receive_to_apply_p95=$($summary.ReceiveToApplyP95) capture_to_receive_p95_raw=$($summary.CaptureToReceiveP95) jitter_raw=$($summary.CaptureToApplyJitterP95) receive_to_apply_jitter_p95=$($summary.ReceiveToApplyJitterP95)"
            if ($null -ne $summary.ReceiveToApplyP95) {
                $applyMetricName = "receive_to_apply_p95 (clock-skew-adjusted)"
                $effectiveApplyP95 = $summary.ReceiveToApplyP95
            }
            $receiveMetricName = "capture_to_receive_p95 (skipped: clock-skew-suspected)"
            $effectiveReceiveP95 = $null
            if ($null -ne $summary.ReceiveToApplyJitterP95) {
                $jitterMetricName = "receive_to_apply_jitter_p95 (clock-skew-adjusted)"
                $effectiveJitterP95 = $summary.ReceiveToApplyJitterP95
            }
        }

        if ($null -ne $effectiveApplyP95 -and $effectiveApplyP95 -gt $CaptureToApplyP95BudgetMs) {
            throw "$applyMetricName budget exceeded for ${label}: actual=${effectiveApplyP95}ms budget=${CaptureToApplyP95BudgetMs}ms"
        }
        if ($null -ne $effectiveReceiveP95 -and $effectiveReceiveP95 -gt $CaptureToReceiveP95BudgetMs) {
            throw "$receiveMetricName budget exceeded for ${label}: actual=${effectiveReceiveP95}ms budget=${CaptureToReceiveP95BudgetMs}ms"
        }
        if ($null -eq $summary.ReceiveToApplyP95) {
            throw "receive_to_apply_p95 missing for ${label}"
        }
        if ($summary.ReceiveToApplyP95 -gt $ReceiveToApplyP95BudgetMs) {
            throw "receive_to_apply_p95 budget exceeded for ${label}: actual=$($summary.ReceiveToApplyP95)ms budget=${ReceiveToApplyP95BudgetMs}ms"
        }
        if ($null -ne $effectiveJitterP95 -and $effectiveJitterP95 -gt $CaptureToApplyJitterP95BudgetMs) {
            throw "$jitterMetricName budget exceeded for ${label}: actual=${effectiveJitterP95}ms budget=${CaptureToApplyJitterP95BudgetMs}ms"
        }
    }
}

Write-TraceLine "trace_end"
Write-Host "[trace] wrote $OutputPath"
