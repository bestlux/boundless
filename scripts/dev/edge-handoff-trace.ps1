param(
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [int]$DurationSeconds = 30,
    [int]$PollMilliseconds = 150,
    [int]$EventsLimit = 200,
    [string]$OutputPath = ""
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

$targets = @(
    @{ Endpoint = $EndpointA; Label = $LabelA }
)
if (-not [string]::IsNullOrWhiteSpace($EndpointB)) {
    $targets += @{ Endpoint = $EndpointB; Label = $LabelB }
}

$states = @{}
$seenEvents = @{}

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
    Write-TraceLine "target label=$($target.Label) endpoint=$($target.Endpoint)"

    $feature = Invoke-CliUnchecked -Endpoint $target.Endpoint -CommandArgs @("feature", "list")
    if ($feature.ExitCode -eq 0) {
        Write-TraceLine "snapshot label=$($target.Label) feature_list=$(($feature.Output -replace '\r?\n',' | ').Trim())"
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
            }
        }
        else {
            Write-TraceLine "transport_events_error label=$label endpoint=$endpoint exit=$($events.ExitCode) output=$(($events.Output -replace '\r?\n',' | ').Trim())"
        }

        $states[$label] = $state
    }

    Start-Sleep -Milliseconds $PollMilliseconds
}

Write-TraceLine "trace_end"
Write-Host "[trace] wrote $OutputPath"
