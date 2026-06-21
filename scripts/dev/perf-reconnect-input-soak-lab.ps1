[CmdletBinding()]
param(
    [ValidateSet("DryRun", "Validate")]
    [string]$Mode = "DryRun",

    [ValidateRange(1, 10000)]
    [int]$Iterations = 3,

    [ValidateSet("coordinator", "peer", "standalone")]
    [string]$Role = "standalone",

    [ValidateSet("A-to-B", "B-to-A")]
    [string[]]$Direction = @("A-to-B", "B-to-A"),

    [string]$OutputRoot = "",
    [switch]$IncludeManualLongSoak,
    [switch]$ReleaseEvidence
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

if ($ReleaseEvidence.IsPresent) {
    throw "The reconnect/input/soak lab dry-run/validation script cannot emit release evidence. Use real two-machine observations with perf-two-machine-evidence.ps1 instead."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/performance/reconnect-input-soak-lab/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

$harness = Join-Path $repoRoot "scripts/dev/perf-two-machine-evidence.ps1"
if (-not (Test-Path -LiteralPath $harness)) {
    throw "Missing two-machine evidence harness: $harness"
}

function Get-ReconnectInputPresets {
    $presets = New-Object System.Collections.Generic.List[object]
    $presets.Add([pscustomobject]@{
            variant = "reconnect-service-restart"
            base_latency_ms = 1650.0
            base_duration_ms = 1850.0
            retry_count = 1
            reconnect_count = 1
            input_capture_state = "not-captured"
            active_peer_class = "trusted-peer"
            transport_event_summary = "service-restart-reconnect"
            policy_expected = "manual-runbook-required"
            manual_disruptive = $true
            status = "passed"
            failure_subsystem = ""
        })
    $presets.Add([pscustomobject]@{
            variant = "reconnect-tray-restart"
            base_latency_ms = 920.0
            base_duration_ms = 1040.0
            retry_count = 0
            reconnect_count = 0
            input_capture_state = "not-captured"
            active_peer_class = "trusted-peer"
            transport_event_summary = "tray-restart-control-rejoin"
            policy_expected = "manual-runbook-required"
            manual_disruptive = $true
            status = "passed"
            failure_subsystem = ""
        })
    $presets.Add([pscustomobject]@{
            variant = "reconnect-network-loss-manual"
            base_latency_ms = $null
            base_duration_ms = $null
            retry_count = 0
            reconnect_count = 0
            input_capture_state = "unavailable"
            active_peer_class = "no-active-peer"
            transport_event_summary = "manual-network-interruption-skipped"
            policy_expected = "manual-disruptive-opt-in"
            manual_disruptive = $true
            status = "skipped"
            failure_subsystem = "network"
        })
    $presets.Add([pscustomobject]@{
            variant = "input-edge-handoff"
            base_latency_ms = 44.0
            base_duration_ms = 64.0
            retry_count = 0
            reconnect_count = 0
            input_capture_state = "locked-to-peer"
            active_peer_class = "trusted-peer"
            transport_event_summary = "input-handoff-attempt"
            policy_expected = "metadata-only-fixture"
            manual_disruptive = $false
            status = "passed"
            failure_subsystem = ""
        })

    return @($presets.ToArray())
}

function Get-SoakPresets {
    $presets = New-Object System.Collections.Generic.List[object]
    $presets.Add([pscustomobject]@{
            variant = "soak-30-minute"
            soak_profile = "30-minute"
            soak_duration_minutes = 30.0
            status = "passed"
            policy_expected = "metadata-only-fixture"
            provisional_classification = "acceptable"
            failure_subsystem = ""
            transport_event_summary = "steady-state-synthetic"
            active_peer_class = "trusted-peer"
            input_capture_state = "released"
        })
    $presets.Add([pscustomobject]@{
            variant = "soak-2-hour-manual"
            soak_profile = "2-hour"
            soak_duration_minutes = 120.0
            status = if ($IncludeManualLongSoak.IsPresent) { "passed" } else { "skipped" }
            policy_expected = "manual-long-run-required"
            provisional_classification = if ($IncludeManualLongSoak.IsPresent) { "acceptable" } else { "no-op" }
            failure_subsystem = if ($IncludeManualLongSoak.IsPresent) { "" } else { "unknown" }
            transport_event_summary = if ($IncludeManualLongSoak.IsPresent) { "long-soak-synthetic" } else { "manual-long-soak-skipped" }
            active_peer_class = if ($IncludeManualLongSoak.IsPresent) { "trusted-peer" } else { "unknown" }
            input_capture_state = if ($IncludeManualLongSoak.IsPresent) { "released" } else { "unknown" }
        })

    return @($presets.ToArray())
}

function Get-ProvisionalClassification {
    param(
        [string]$Status,
        [string]$ScenarioVariant,
        [Nullable[double]]$LatencyMs
    )

    if ($Status -eq "skipped") {
        return "no-op"
    }
    if ($Status -eq "failed") {
        return "fail"
    }
    if ($ScenarioVariant -eq "input-edge-handoff") {
        if ($LatencyMs -le 100.0) {
            return "acceptable"
        }
        if ($LatencyMs -le 250.0) {
            return "warning"
        }
        return "fail"
    }
    if ($LatencyMs -le 3000.0) {
        return "acceptable"
    }
    if ($LatencyMs -le 10000.0) {
        return "warning"
    }

    return "fail"
}

function New-ResourceTrendSamples {
    param(
        [double]$DurationMinutes,
        [double]$CpuBase,
        [double]$MemoryBaseMb
    )

    $samples = New-Object System.Collections.Generic.List[object]
    $sampleCount = 4
    for ($i = 1; $i -le $sampleCount; $i++) {
        $elapsed = [Math]::Round((($DurationMinutes * 60.0) / ($sampleCount - 1)) * ($i - 1), 3)
        $samples.Add([pscustomobject]@{
                sample_index = $i
                elapsed_seconds = $elapsed
                cpu_percent = [Math]::Round($CpuBase + ($i * 0.4), 3)
                memory_mb = [Math]::Round($MemoryBaseMb + ($i * 3.5), 3)
            })
    }

    return @($samples.ToArray())
}

function New-ReconnectInputSoakLabObservations {
    $items = New-Object System.Collections.Generic.List[object]
    $reconnectPresets = Get-ReconnectInputPresets
    foreach ($preset in $reconnectPresets) {
        foreach ($directionName in $Direction) {
            for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
                $status = [string]$preset.status
                $failureSubsystem = [string]$preset.failure_subsystem
                $inputCaptureState = [string]$preset.input_capture_state
                $directionOffset = if ($directionName -eq "B-to-A") { 18.0 } else { 0.0 }
                $iterationOffset = ($iteration - 1) * 7.0
                $latency = if ($null -ne $preset.base_latency_ms) { [double]$preset.base_latency_ms + $directionOffset + $iterationOffset } else { $null }
                $duration = if ($null -ne $preset.base_duration_ms) { [double]$preset.base_duration_ms + $directionOffset + $iterationOffset } else { $null }
                if ($preset.variant -eq "input-edge-handoff" -and $directionName -eq "B-to-A" -and $iteration -eq $Iterations) {
                    $status = "failed"
                    $failureSubsystem = "input"
                    $inputCaptureState = "capture-failed"
                    $latency = $null
                    $duration = $null
                }
                $classification = Get-ProvisionalClassification -Status $status -ScenarioVariant $preset.variant -LatencyMs $latency
                $reason = if ($status -eq "skipped") {
                    "provisional no-op: disruptive/manual step is not executed by fixture mode"
                }
                else {
                    "provisional synthetic metadata only; not a product reliability threshold"
                }

                $items.Add([pscustomobject]@{
                        scenario = "reconnect-input"
                        scenario_variant = $preset.variant
                        direction = $directionName
                        iteration = $iteration
                        role = $Role
                        status = $status
                        started_at_utc = [DateTime]::UtcNow.ToString("o")
                        latency_ms = if ($null -ne $latency) { [Math]::Round($latency, 3) } else { $null }
                        duration_ms = if ($null -ne $duration) { [Math]::Round($duration, 3) } else { $null }
                        bytes = 0L
                        retry_count = if ($status -eq "passed") { [int]$preset.retry_count } else { 0 }
                        reconnect_count = if ($status -eq "passed") { [int]$preset.reconnect_count } else { 0 }
                        failure_subsystem = $failureSubsystem
                        failure_kind = if ($status -eq "failed") { "synthetic-input-handoff-failure" } elseif ($status -eq "skipped") { "manual-step-not-run" } else { "" }
                        input_capture_state = $inputCaptureState
                        active_peer_class = $preset.active_peer_class
                        transport_event_summary = $preset.transport_event_summary
                        manual_disruptive = [bool]$preset.manual_disruptive
                        policy_expected = $preset.policy_expected
                        payload_synthetic = $true
                        provisional_classification = $classification
                        provisional_classification_reason = $reason
                    })
            }
        }
    }

    foreach ($preset in Get-SoakPresets) {
        $resourceSamples = @()
        if ($preset.status -eq "passed") {
            $resourceSamples = New-ResourceTrendSamples -DurationMinutes ([double]$preset.soak_duration_minutes) -CpuBase 4.5 -MemoryBaseMb 180.0
        }
        $items.Add([pscustomobject]@{
                scenario = "soak"
                scenario_variant = $preset.variant
                direction = "bidirectional"
                iteration = 1
                role = $Role
                status = $preset.status
                started_at_utc = [DateTime]::UtcNow.ToString("o")
                latency_ms = $null
                duration_ms = if ($preset.status -eq "passed") { [Math]::Round([double]$preset.soak_duration_minutes * 60000.0, 3) } else { $null }
                bytes = 0L
                retry_count = 0
                reconnect_count = 0
                failure_subsystem = $preset.failure_subsystem
                failure_kind = if ($preset.status -eq "skipped") { "manual-long-soak-not-run" } else { "" }
                input_capture_state = $preset.input_capture_state
                active_peer_class = $preset.active_peer_class
                transport_event_summary = $preset.transport_event_summary
                soak_profile = $preset.soak_profile
                soak_duration_minutes = [double]$preset.soak_duration_minutes
                resource_trend_samples = $resourceSamples
                manual_disruptive = $false
                policy_expected = $preset.policy_expected
                payload_synthetic = $true
                provisional_classification = $preset.provisional_classification
                provisional_classification_reason = "provisional synthetic soak metadata only; not a product reliability threshold"
            })
    }

    return @($items.ToArray())
}

function Get-Percentile {
    param(
        [double[]]$Values,
        [ValidateRange(0, 100)]
        [int]$Percentile
    )

    if ($Values.Count -eq 0) {
        return $null
    }

    $sorted = @($Values | Sort-Object)
    $rank = [Math]::Ceiling(($Percentile / 100.0) * $sorted.Count)
    if ($rank -lt 1) {
        $rank = 1
    }

    return [Math]::Round([double]$sorted[[int]$rank - 1], 3)
}

function Invoke-ReconnectInputSoakLabDryRun {
    $observations = New-ReconnectInputSoakLabObservations
    $observationPath = Join-Path $OutputRoot "reconnect-input-soak-lab-observations.json"
    $observations | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $observationPath -Encoding utf8
    & $harness -Mode Summarize -Role $Role -Scenario @("reconnect-input", "soak") -ObservationPath $observationPath -OutputRoot $OutputRoot
    return [pscustomobject]@{
        observations = $observations
        observation_path = $observationPath
        packet_path = Join-Path $OutputRoot "two-machine-evidence.json"
        markdown_path = Join-Path $OutputRoot "two-machine-evidence.md"
    }
}

function Get-ObservationField {
    param(
        [object]$Observation,
        [string]$Name
    )

    $property = $Observation.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return ""
    }

    return [string]$property.Value
}

function Assert-ObservationFieldSet {
    param(
        [object[]]$Observations,
        [string]$PropertyName,
        [string[]]$ExpectedValues,
        [string]$Label
    )

    $actual = @($Observations | ForEach-Object { Get-ObservationField -Observation $_ -Name $PropertyName } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    $expected = @($ExpectedValues | Sort-Object -Unique)
    if ($actual.Count -ne $expected.Count) {
        throw "$Label expected values '$($expected -join ",")', found '$($actual -join ",")'."
    }

    foreach ($value in $expected) {
        if ($actual -notcontains $value) {
            throw "$Label missing expected value '$value'; found '$($actual -join ",")'."
        }
    }
}

function Assert-SummaryMatchesObservations {
    param(
        [object]$Packet,
        [object[]]$Observations
    )

    $reconnectSummary = @($Packet.summary.scenario_summaries | Where-Object { $_.scenario -eq "reconnect-input" })[0]
    $soakSummary = @($Packet.summary.scenario_summaries | Where-Object { $_.scenario -eq "soak" })[0]
    if ($null -eq $reconnectSummary -or $null -eq $soakSummary) {
        throw "Missing reconnect-input or soak summary."
    }

    $expectedReconnectRows = 4 * @($Direction | Sort-Object -Unique).Count * $Iterations
    if ($reconnectSummary.iterations -ne $expectedReconnectRows) {
        throw "reconnect-input iterations expected $expectedReconnectRows, found $($reconnectSummary.iterations)."
    }
    if ($reconnectSummary.failure_count -ne 1) {
        throw "reconnect-input failure_count expected 1 synthetic input failure, found $($reconnectSummary.failure_count)."
    }
    $expectedSkipped = @($Direction | Sort-Object -Unique).Count * $Iterations
    if ($reconnectSummary.skipped_count -ne $expectedSkipped) {
        throw "reconnect-input skipped_count expected $expectedSkipped manual network rows, found $($reconnectSummary.skipped_count)."
    }
    if ($reconnectSummary.failure_subsystems.input -ne 1 -or $reconnectSummary.failure_subsystems.network -ne $expectedSkipped) {
        throw "reconnect-input failure subsystem counts did not preserve input/network classification."
    }
    if ($reconnectSummary.manual_disruptive_count -ne (3 * @($Direction | Sort-Object -Unique).Count * $Iterations)) {
        throw "reconnect-input manual_disruptive_count did not include service, tray, and network presets."
    }
    if ($reconnectSummary.retry_count_total -le 0 -or $reconnectSummary.reconnect_count_total -le 0) {
        throw "reconnect-input retry/reconnect totals were not preserved."
    }

    $passedLatencies = @($Observations | Where-Object { $_.scenario -eq "reconnect-input" -and $_.status -eq "passed" -and $null -ne $_.latency_ms } | ForEach-Object { [double]$_.latency_ms })
    $expectedP50 = Get-Percentile -Values $passedLatencies -Percentile 50
    $expectedP95 = Get-Percentile -Values $passedLatencies -Percentile 95
    $expectedMax = [Math]::Round([double](@($passedLatencies | Measure-Object -Maximum).Maximum), 3)
    if ($reconnectSummary.latency_ms.p50 -ne $expectedP50 -or $reconnectSummary.latency_ms.p95 -ne $expectedP95 -or $reconnectSummary.latency_ms.max -ne $expectedMax) {
        throw "reconnect-input latency percentile summary did not match fixture observations."
    }

    if ($soakSummary.success_count -ne 1 -or $soakSummary.skipped_count -ne 1) {
        throw "soak summary expected one 30-minute synthetic pass and one skipped 2-hour manual row."
    }
    if ($soakSummary.soak_profiles.'30_minute' -ne 1 -or $soakSummary.soak_profiles.'2_hour' -ne 1) {
        throw "soak summary did not preserve 30-minute and 2-hour profile metadata."
    }
    if ($soakSummary.soak_duration_minutes.max -ne 30.0) {
        throw "soak max duration should use passed rows only by default."
    }
    if ($soakSummary.resource_trend.sample_count -ne 4) {
        throw "soak resource trend expected 4 bounded samples, found $($soakSummary.resource_trend.sample_count)."
    }
    if ($soakSummary.resource_trend.cpu_percent.p50 -le 0 -or $soakSummary.resource_trend.memory_mb.p95 -le 0) {
        throw "soak resource trend did not summarize CPU/memory samples."
    }
}

function Assert-ReconnectInputSoakObservationMetadata {
    param([object[]]$PacketObservations)

    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "scenario_variant" -ExpectedValues @("input-edge-handoff", "reconnect-network-loss-manual", "reconnect-service-restart", "reconnect-tray-restart", "soak-30-minute", "soak-2-hour-manual") -Label "reconnect/input/soak scenario variants"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "failure_subsystem" -ExpectedValues @("input", "network", "unknown") -Label "reconnect/input/soak failure subsystems"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "active_peer_class" -ExpectedValues @("no-active-peer", "trusted-peer", "unknown") -Label "reconnect/input/soak active peer classes"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "input_capture_state" -ExpectedValues @("capture-failed", "locked-to-peer", "not-captured", "released", "unavailable", "unknown") -Label "reconnect/input/soak input capture states"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "transport_event_summary" -ExpectedValues @("input-handoff-attempt", "manual-long-soak-skipped", "manual-network-interruption-skipped", "service-restart-reconnect", "steady-state-synthetic", "tray-restart-control-rejoin") -Label "reconnect/input/soak transport event summaries"

    $networkRows = @($PacketObservations | Where-Object { $_.scenario_variant -eq "reconnect-network-loss-manual" })
    foreach ($row in $networkRows) {
        if ($row.status -ne "skipped" -or $row.provisional_classification -ne "no-op" -or $row.manual_disruptive -ne $true) {
            throw "network-loss rows must stay skipped/no-op/manual-disruptive in default fixture mode."
        }
    }

    $inputFailures = @($PacketObservations | Where-Object { $_.scenario_variant -eq "input-edge-handoff" -and $_.status -eq "failed" })
    if ($inputFailures.Count -ne 1 -or $inputFailures[0].failure_subsystem -ne "input" -or $inputFailures[0].input_capture_state -ne "capture-failed") {
        throw "input handoff fixture must preserve one classified synthetic input failure."
    }

    foreach ($row in $PacketObservations) {
        if ([bool]$row.payload_synthetic -ne $true) {
            throw "reconnect/input/soak observations must preserve payload_synthetic=true."
        }
        if ([string]$row.active_peer_class -like "*:*" -or [string]$row.active_peer_class -like "*.*.*.*") {
            throw "active_peer_class must not contain raw endpoints or IDs."
        }
        if (@($row.resource_trend_samples).Count -gt 6) {
            throw "resource trend samples must stay bounded per observation."
        }
    }
}

function Invoke-ReconnectInputSoakLabValidation {
    $result = Invoke-ReconnectInputSoakLabDryRun
    $packet = Get-Content -LiteralPath $result.packet_path -Raw | ConvertFrom-Json
    $observations = @($result.observations)
    $packetObservations = @($packet.observations)

    Assert-SummaryMatchesObservations -Packet $packet -Observations $observations
    Assert-ReconnectInputSoakObservationMetadata -PacketObservations $packetObservations

    if ($packet.privacy.payload_contents_recorded -ne $false -or $packet.privacy.raw_peer_ids_recorded -ne $false -or $packet.privacy.raw_paths_recorded -ne $false) {
        throw "Reconnect/input/soak lab packet did not preserve privacy flags."
    }

    foreach ($artifact in Get-ChildItem -LiteralPath $OutputRoot -File -Recurse -Include "*.json", "*.md", "*.log") {
        if ($artifact.Length -gt 524288) {
            throw "Reconnect/input/soak lab artifact '$($artifact.Name)' exceeded the 512 KiB fixture bound."
        }
        $content = Get-Content -LiteralPath $artifact.FullName -Raw
        foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "12345678-1234-1234-1234-123456789abc", "npipe://", "\\.\pipe", "actual clipboard", "private file", $repoRoot, $OutputRoot)) {
            if (-not [string]::IsNullOrWhiteSpace($forbidden) -and $content.Contains($forbidden)) {
                throw "Reconnect/input/soak lab artifact '$($artifact.Name)' leaked forbidden token '$forbidden'."
            }
        }
    }

    Write-Host "reconnect_input_soak_lab_fixture_validation=passed"
    Write-Host "output_root=[redacted]"
}

switch ($Mode) {
    "DryRun" {
        Invoke-ReconnectInputSoakLabDryRun | Out-Null
    }
    "Validate" {
        Invoke-ReconnectInputSoakLabValidation
    }
}
