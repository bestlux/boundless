# Shared measurement and budget contract; no runtime side effects.
function Assert-InputTraceBudgets {
    param(
        [System.Collections.IDictionary]$Summary,
        [ValidateRange(2, 1000000)][int]$MinimumSamples = 20,
        [ValidateRange(1, 60000)][int]$CaptureToApplyP95BudgetMs = 25,
        [ValidateRange(1, 60000)][int]$CaptureToReceiveP95BudgetMs = 10,
        [ValidateRange(1, 60000)][int]$ReceiveToApplyP95BudgetMs = 8,
        [ValidateRange(1, 60000)][int]$CaptureToApplyJitterP95BudgetMs = 10
    )

    foreach ($metric in @('CaptureToApply', 'CaptureToReceive', 'ReceiveToApply')) {
        $count = $Summary["${metric}Count"]
        if ($null -eq $count -or $count -lt $MinimumSamples) {
            throw "$metric needs at least $MinimumSamples fresh samples for $($Summary.Label); observed=$count"
        }
    }
    if ($null -ne $Summary.EstimatedClockSkewMs) {
        # A receive-only metric cannot substitute for an end-to-end latency claim.
        throw "Clock skew suspected for $($Summary.Label); synchronize clocks or use same-clock RTT measurement before asserting end-to-end budgets"
    }
    $budgets = @{
        CaptureToApplyP95 = $CaptureToApplyP95BudgetMs
        CaptureToReceiveP95 = $CaptureToReceiveP95BudgetMs
        ReceiveToApplyP95 = $ReceiveToApplyP95BudgetMs
        CaptureToApplyJitterP95 = $CaptureToApplyJitterP95BudgetMs
    }
    foreach ($name in $budgets.Keys) {
        $value = $Summary[$name]
        if ($null -eq $value -or [double]::IsNaN([double]$value) -or [double]::IsInfinity([double]$value) -or $value -lt 0) {
            throw "$name missing or invalid for $($Summary.Label)"
        }
        if ($value -gt $budgets[$name]) {
            throw "$name budget exceeded for $($Summary.Label): actual=${value}ms budget=$($budgets[$name])ms"
        }
    }
}

function Get-MetricValue {
    param(
        [string]$Line,
        [string]$MetricName
    )
    $match = [regex]::Match($Line, "(?<![\w])$([regex]::Escape($MetricName))=([0-9]+)(?=\s|$)")
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
    $sorted = @($Values | Sort-Object)
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
