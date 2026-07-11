[CmdletBinding()]
param(
    [ValidateSet("DryRun", "Validate")]
    [string]$Mode = "DryRun",

    [ValidateRange(1, 10000)]
    [int]$Iterations = 10,

    [ValidateSet("coordinator", "peer", "standalone")]
    [string]$Role = "standalone",

    [ValidateSet("A-to-B", "B-to-A")]
    [string[]]$Direction = @("A-to-B", "B-to-A"),

    [string]$OutputRoot = "",
    [switch]$ReleaseEvidence
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

if ($ReleaseEvidence.IsPresent) {
    throw "The clipboard lab dry-run/validation script cannot emit release evidence. Use real two-machine observations with perf-two-machine-evidence.ps1 instead."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/performance/clipboard-lab/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

$harness = Join-Path $repoRoot "scripts/dev/perf-two-machine-evidence.ps1"
if (-not (Test-Path -LiteralPath $harness)) {
    throw "Missing two-machine evidence harness: $harness"
}

$MaxClipboardTextBytes = 255 * 1024
$MaxClipboardImageBytes = 8 * 1024 * 1024
$BmpHeaderBytes = 54

function Get-BmpPayloadBytes {
    param(
        [int]$Width,
        [int]$Height
    )

    return ([int64]$Width * [int64]$Height * 4L) + [int64]$BmpHeaderBytes
}

function Get-ClipboardLabPresets {
    $presets = New-Object System.Collections.Generic.List[object]
    $presets.Add([pscustomobject]@{
            scenario = "text-clipboard"
            variant = "text-small"
            payload_kind = "text"
            payload_label = "synthetic:text:small"
            payload_bytes = 128L
            policy_limit_bytes = [int64]$MaxClipboardTextBytes
            policy_expected = "accepted"
            base_latency_ms = 12.0
        })
    $presets.Add([pscustomobject]@{
            scenario = "text-clipboard"
            variant = "text-medium"
            payload_kind = "text"
            payload_label = "synthetic:text:medium"
            payload_bytes = 8192L
            policy_limit_bytes = [int64]$MaxClipboardTextBytes
            policy_expected = "accepted"
            base_latency_ms = 24.0
        })
    $presets.Add([pscustomobject]@{
            scenario = "text-clipboard"
            variant = "text-large-policy-limit"
            payload_kind = "text"
            payload_label = "synthetic:text:large-policy-limit"
            payload_bytes = [int64]$MaxClipboardTextBytes
            policy_limit_bytes = [int64]$MaxClipboardTextBytes
            policy_expected = "accepted"
            base_latency_ms = 45.0
        })

    foreach ($image in @(
            @{ variant = "image-screenshot-scale"; label = "synthetic:image:screenshot-scale"; width = 1366; height = 768; base = 90.0 },
            @{ variant = "image-1080p"; label = "synthetic:image:1080p"; width = 1920; height = 1080; base = 145.0 },
            @{ variant = "image-4k-policy-bound"; label = "synthetic:image:4k-policy-bound"; width = 3840; height = 2160; base = 0.0 },
            @{ variant = "image-near-limit"; label = "synthetic:image:near-limit"; width = 1448; height = 1448; base = 160.0 }
        )) {
        $payloadBytes = Get-BmpPayloadBytes -Width $image.width -Height $image.height
        $presets.Add([pscustomobject]@{
                scenario = "image-clipboard"
                variant = $image.variant
                payload_kind = "image-bmp"
                payload_label = $image.label
                payload_bytes = [int64]$payloadBytes
                policy_limit_bytes = [int64]$MaxClipboardImageBytes
                policy_expected = if ($payloadBytes -le $MaxClipboardImageBytes) { "accepted" } else { "rejected-by-current-policy" }
                base_latency_ms = [double]$image.base
            })
    }

    return @($presets.ToArray())
}

function Get-ProvisionalClassification {
    param(
        [string]$Status,
        [Nullable[double]]$LatencyMs
    )

    if ($Status -eq "skipped") {
        return "no-op"
    }
    if ($Status -eq "failed") {
        return "fail"
    }
    if ($null -eq $LatencyMs) {
        return "warning"
    }
    if ($LatencyMs -le 250.0) {
        return "acceptable"
    }
    if ($LatencyMs -le 750.0) {
        return "warning"
    }

    return "fail"
}

function New-ClipboardLabObservations {
    $items = New-Object System.Collections.Generic.List[object]
    $presets = Get-ClipboardLabPresets
    foreach ($preset in $presets) {
        foreach ($directionName in $Direction) {
            for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
                $acceptedByPolicy = $preset.policy_expected -eq "accepted"
                $status = if ($acceptedByPolicy) { "passed" } else { "skipped" }
                $directionOffset = if ($directionName -eq "B-to-A") { 5.0 } else { 0.0 }
                $latency = if ($acceptedByPolicy) { [double]$preset.base_latency_ms + $directionOffset + (($iteration - 1) * 3.2) } else { $null }
                $duration = if ($null -ne $latency) { $latency } else { $null }
                $classification = Get-ProvisionalClassification -Status $status -LatencyMs $latency
                $reason = if ($status -eq "skipped") {
                    "provisional no-op: estimated BMP bytes exceed current clipboard image policy limit"
                }
                else {
                    "provisional synthetic dry-run classification only"
                }

                $items.Add([pscustomobject]@{
                        scenario = $preset.scenario
                        scenario_variant = $preset.variant
                        direction = $directionName
                        iteration = $iteration
                        role = $Role
                        status = $status
                        started_at_utc = [DateTime]::UtcNow.ToString("o")
                        latency_ms = if ($null -ne $latency) { [Math]::Round($latency, 3) } else { $null }
                        duration_ms = if ($null -ne $duration) { [Math]::Round($duration, 3) } else { $null }
                        bytes = if ($acceptedByPolicy) { [int64]$preset.payload_bytes } else { 0L }
                        payload_kind = $preset.payload_kind
                        payload_label = $preset.payload_label
                        payload_bytes = [int64]$preset.payload_bytes
                        policy_limit_bytes = [int64]$preset.policy_limit_bytes
                        policy_expected = $preset.policy_expected
                        payload_synthetic = $true
                        provisional_classification = $classification
                        provisional_classification_reason = $reason
                        failure_kind = if ($status -eq "skipped") { "policy-limit" } else { "" }
                    })
            }
        }
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

function Invoke-ClipboardLabDryRun {
    $observations = New-ClipboardLabObservations
    $observationPath = Join-Path $OutputRoot "clipboard-lab-observations.json"
    $observations | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $observationPath -Encoding utf8
    & $harness -Mode Summarize -Role $Role -Scenario @("text-clipboard", "image-clipboard") -ObservationPath $observationPath -OutputRoot $OutputRoot
    return [pscustomobject]@{
        observations = $observations
        observation_path = $observationPath
        packet_path = Join-Path $OutputRoot "two-machine-evidence.json"
        markdown_path = Join-Path $OutputRoot "two-machine-evidence.md"
    }
}

function Assert-SummaryMatchesObservations {
    param(
        [object]$Packet,
        [object[]]$Observations,
        [string]$ScenarioName,
        [int]$ExpectedSuccess,
        [int]$ExpectedFailure,
        [int]$ExpectedSkipped,
        [int]$ExpectedNoOp,
        [int]$ExpectedAcceptable,
        [int64]$ExpectedPayloadMin,
        [int64]$ExpectedPayloadMax
    )

    $summary = @($Packet.summary.scenario_summaries | Where-Object { $_.scenario -eq $ScenarioName })[0]
    if ($summary.success_count -ne $ExpectedSuccess) {
        throw "$ScenarioName success_count expected $ExpectedSuccess, found $($summary.success_count)."
    }
    if ($summary.failure_count -ne $ExpectedFailure) {
        throw "$ScenarioName failure_count expected $ExpectedFailure, found $($summary.failure_count)."
    }
    if ($summary.skipped_count -ne $ExpectedSkipped) {
        throw "$ScenarioName skipped_count expected $ExpectedSkipped, found $($summary.skipped_count)."
    }
    if ($summary.provisional_classifications.no_op -ne $ExpectedNoOp) {
        throw "$ScenarioName no-op classification expected $ExpectedNoOp, found $($summary.provisional_classifications.no_op)."
    }
    if ($summary.provisional_classifications.acceptable -ne $ExpectedAcceptable) {
        throw "$ScenarioName acceptable classification expected $ExpectedAcceptable, found $($summary.provisional_classifications.acceptable)."
    }
    if ($summary.payload_bytes.min -ne $ExpectedPayloadMin) {
        throw "$ScenarioName payload min expected $ExpectedPayloadMin, found $($summary.payload_bytes.min)."
    }
    if ($summary.payload_bytes.max -ne $ExpectedPayloadMax) {
        throw "$ScenarioName payload max expected $ExpectedPayloadMax, found $($summary.payload_bytes.max)."
    }

    $passedLatencies = @($Observations | Where-Object { $_.scenario -eq $ScenarioName -and $_.status -eq "passed" -and $null -ne $_.latency_ms } | ForEach-Object { [double]$_.latency_ms })
    $expectedP50 = Get-Percentile -Values $passedLatencies -Percentile 50
    $expectedP95 = Get-Percentile -Values $passedLatencies -Percentile 95
    $expectedMax = if ($passedLatencies.Count -gt 0) { [Math]::Round([double](@($passedLatencies | Measure-Object -Maximum).Maximum), 3) } else { $null }
    if ($summary.latency_ms.p50 -ne $expectedP50 -or $summary.latency_ms.p95 -ne $expectedP95 -or $summary.latency_ms.max -ne $expectedMax) {
        throw "$ScenarioName latency summary did not match fixture observations."
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

function Assert-ObservationRows {
    param(
        [object[]]$Rows,
        [int]$ExpectedCount,
        [string]$Label,
        [string]$ExpectedPayloadKind,
        [string]$ExpectedStatus,
        [string]$ExpectedClassification,
        [string]$ExpectedPolicy,
        [bool]$ExpectSynthetic
    )

    if ($Rows.Count -ne $ExpectedCount) {
        throw "$Label expected $ExpectedCount observation rows, found $($Rows.Count)."
    }

    foreach ($row in $Rows) {
        if ((Get-ObservationField -Observation $row -Name "payload_kind") -ne $ExpectedPayloadKind) {
            throw "$Label did not preserve payload_kind=$ExpectedPayloadKind."
        }
        if ((Get-ObservationField -Observation $row -Name "status") -ne $ExpectedStatus) {
            throw "$Label did not preserve status=$ExpectedStatus."
        }
        if ((Get-ObservationField -Observation $row -Name "provisional_classification") -ne $ExpectedClassification) {
            throw "$Label did not preserve provisional_classification=$ExpectedClassification."
        }
        if ((Get-ObservationField -Observation $row -Name "policy_expected") -ne $ExpectedPolicy) {
            throw "$Label did not preserve policy_expected=$ExpectedPolicy."
        }
        if ([bool]$row.payload_synthetic -ne $ExpectSynthetic) {
            throw "$Label did not preserve payload_synthetic=$ExpectSynthetic."
        }
    }
}

function Assert-ClipboardObservationMetadata {
    param(
        [object[]]$PacketObservations,
        [int]$ExpectedIterations,
        [string[]]$ExpectedDirections
    )

    $expectedTextVariants = @("text-small", "text-medium", "text-large-policy-limit")
    $expectedImageVariants = @("image-screenshot-scale", "image-1080p", "image-4k-policy-bound", "image-near-limit")
    $expectedVariants = @($expectedTextVariants + $expectedImageVariants)
    $expectedDirectionsSorted = @($ExpectedDirections | Sort-Object -Unique)
    $directionCount = $expectedDirectionsSorted.Count
    $rowsPerVariant = $directionCount * $ExpectedIterations

    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "direction" -ExpectedValues $expectedDirectionsSorted -Label "clipboard lab directions"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "scenario_variant" -ExpectedValues $expectedVariants -Label "clipboard lab scenario variants"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "payload_kind" -ExpectedValues @("text", "image-bmp") -Label "clipboard lab payload kinds"

    foreach ($variant in $expectedTextVariants) {
        $rows = @($PacketObservations | Where-Object { $_.scenario_variant -eq $variant })
        Assert-ObservationRows -Rows $rows -ExpectedCount $rowsPerVariant -Label $variant -ExpectedPayloadKind "text" -ExpectedStatus "passed" -ExpectedClassification "acceptable" -ExpectedPolicy "accepted" -ExpectSynthetic $true
    }

    foreach ($variant in @("image-screenshot-scale", "image-1080p", "image-near-limit")) {
        $rows = @($PacketObservations | Where-Object { $_.scenario_variant -eq $variant })
        Assert-ObservationRows -Rows $rows -ExpectedCount $rowsPerVariant -Label $variant -ExpectedPayloadKind "image-bmp" -ExpectedStatus "passed" -ExpectedClassification "acceptable" -ExpectedPolicy "accepted" -ExpectSynthetic $true
    }

    $policyBoundRows = @($PacketObservations | Where-Object { $_.scenario_variant -eq "image-4k-policy-bound" })
    Assert-ObservationRows -Rows $policyBoundRows -ExpectedCount $rowsPerVariant -Label "image-4k-policy-bound" -ExpectedPayloadKind "image-bmp" -ExpectedStatus "skipped" -ExpectedClassification "no-op" -ExpectedPolicy "rejected-by-current-policy" -ExpectSynthetic $true
    foreach ($row in $policyBoundRows) {
        if ([int64]$row.bytes -ne 0L) {
            throw "image-4k-policy-bound rows must preserve bytes=0 for skipped policy-bound observations."
        }
        if ([int64]$row.payload_bytes -le [int64]$row.policy_limit_bytes) {
            throw "image-4k-policy-bound rows must preserve payload_bytes greater than policy_limit_bytes."
        }
    }
}

function Invoke-ClipboardLabValidation {
    $result = Invoke-ClipboardLabDryRun
    $packet = Get-Content -LiteralPath $result.packet_path -Raw | ConvertFrom-Json
    $observations = @($result.observations)
    $packetObservations = @($packet.observations)
    $directionCount = $Direction.Count

    Assert-SummaryMatchesObservations -Packet $packet -Observations $observations -ScenarioName "text-clipboard" -ExpectedSuccess (3 * $directionCount * $Iterations) -ExpectedFailure 0 -ExpectedSkipped 0 -ExpectedNoOp 0 -ExpectedAcceptable (3 * $directionCount * $Iterations) -ExpectedPayloadMin 128L -ExpectedPayloadMax ([int64]$MaxClipboardTextBytes)
    Assert-SummaryMatchesObservations -Packet $packet -Observations $observations -ScenarioName "image-clipboard" -ExpectedSuccess (3 * $directionCount * $Iterations) -ExpectedFailure 0 -ExpectedSkipped (1 * $directionCount * $Iterations) -ExpectedNoOp (1 * $directionCount * $Iterations) -ExpectedAcceptable (3 * $directionCount * $Iterations) -ExpectedPayloadMin (Get-BmpPayloadBytes -Width 1366 -Height 768) -ExpectedPayloadMax (Get-BmpPayloadBytes -Width 3840 -Height 2160)
    Assert-ClipboardObservationMetadata -PacketObservations $packetObservations -ExpectedIterations $Iterations -ExpectedDirections $Direction

    if ($packet.privacy.payload_contents_recorded -ne $false -or $packet.privacy.raw_peer_ids_recorded -ne $false -or $packet.privacy.raw_paths_recorded -ne $false) {
        throw "Clipboard lab packet did not preserve privacy flags."
    }

    foreach ($artifact in Get-ChildItem -LiteralPath $OutputRoot -File -Recurse -Include "*.json", "*.md", "*.log") {
        $content = Get-Content -LiteralPath $artifact.FullName -Raw
        foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "actual clipboard", "private clipboard")) {
            if ($content.Contains($forbidden)) {
                throw "Clipboard lab artifact '$($artifact.Name)' leaked forbidden token '$forbidden'."
            }
        }
    }

    Write-Host "clipboard_lab_fixture_validation=passed"
    Write-Host "output_root=[redacted]"
}

switch ($Mode) {
    "DryRun" {
        Invoke-ClipboardLabDryRun | Out-Null
    }
    "Validate" {
        Invoke-ClipboardLabValidation
    }
}
