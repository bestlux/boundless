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
    [switch]$IncludeLarge,
    [switch]$ReleaseEvidence
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

if ($ReleaseEvidence.IsPresent) {
    throw "The file-transfer lab dry-run/validation script cannot emit release evidence. Use real two-machine observations with perf-two-machine-evidence.ps1 instead."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/performance/file-transfer-lab/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

$harness = Join-Path $repoRoot "scripts/dev/perf-two-machine-evidence.ps1"
if (-not (Test-Path -LiteralPath $harness)) {
    throw "Missing two-machine evidence harness: $harness"
}

$OneMiB = 1024L * 1024L
$MediumBytes = 100L * $OneMiB
$LargeBytes = 1024L * $OneMiB

function Get-FileTransferLabPresets {
    $presets = New-Object System.Collections.Generic.List[object]
    $presets.Add([pscustomobject]@{
            variant = "single-small-file"
            payload_kind = "file-set"
            payload_label = "synthetic:file-transfer:single-small-file"
            payload_bytes = 4L * 1024L
            file_count = 1
            file_count_class = "single-file"
            policy_expected = "enabled-by-default"
            base_setup_ms = 42.0
            base_duration_ms = 95.0
            large_opt_in = $false
        })
    $presets.Add([pscustomobject]@{
            variant = "many-small-files"
            payload_kind = "file-set"
            payload_label = "synthetic:file-transfer:many-small-files"
            payload_bytes = 128L * 4096L
            file_count = 128
            file_count_class = "many-small-files"
            policy_expected = "enabled-by-default"
            base_setup_ms = 85.0
            base_duration_ms = 780.0
            large_opt_in = $false
        })
    $presets.Add([pscustomobject]@{
            variant = "medium-100mb"
            payload_kind = "file-set"
            payload_label = "synthetic:file-transfer:medium-100mb"
            payload_bytes = [int64]$MediumBytes
            file_count = 1
            file_count_class = "medium-file"
            policy_expected = "metadata-only-fixture"
            base_setup_ms = 130.0
            base_duration_ms = 14500.0
            large_opt_in = $false
        })
    $presets.Add([pscustomobject]@{
            variant = "large-1gb-opt-in"
            payload_kind = "file-set"
            payload_label = "synthetic:file-transfer:large-1gb-opt-in"
            payload_bytes = [int64]$LargeBytes
            file_count = 1
            file_count_class = "large-file"
            policy_expected = "opt-in-required"
            base_setup_ms = 210.0
            base_duration_ms = 145000.0
            large_opt_in = $true
        })

    return @($presets.ToArray())
}

function Get-ProvisionalClassification {
    param(
        [string]$Status,
        [Nullable[double]]$DurationMs
    )

    if ($Status -eq "skipped") {
        return "no-op"
    }
    if ($Status -eq "failed") {
        return "fail"
    }
    if ($null -eq $DurationMs) {
        return "warning"
    }
    if ($DurationMs -le 30000.0) {
        return "acceptable"
    }
    if ($DurationMs -le 180000.0) {
        return "warning"
    }

    return "fail"
}

function New-FileTransferLabObservations {
    $items = New-Object System.Collections.Generic.List[object]
    $presets = Get-FileTransferLabPresets
    foreach ($preset in $presets) {
        foreach ($directionName in $Direction) {
            for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
                $enabled = (-not [bool]$preset.large_opt_in) -or $IncludeLarge.IsPresent
                $status = if ($enabled) { "passed" } else { "skipped" }
                $directionOffset = if ($directionName -eq "B-to-A") { 24.0 } else { 0.0 }
                $iterationOffset = ($iteration - 1) * 11.0
                $setupLatency = if ($enabled) { [double]$preset.base_setup_ms + ($directionOffset / 3.0) + (($iteration - 1) * 2.0) } else { $null }
                $duration = if ($enabled) { [double]$preset.base_duration_ms + $directionOffset + $iterationOffset } else { $null }
                $classification = Get-ProvisionalClassification -Status $status -DurationMs $duration
                $hashLabel = "sha256:synthetic:$($preset.variant)"
                $reason = if ($status -eq "skipped") {
                    "provisional no-op: large file scenario is opt-in and writes no payload in fixture mode"
                }
                else {
                    "provisional synthetic metadata only; not a product speed threshold"
                }

                $items.Add([pscustomobject]@{
                        scenario = "file-transfer"
                        scenario_variant = $preset.variant
                        direction = $directionName
                        iteration = $iteration
                        role = $Role
                        status = $status
                        started_at_utc = [DateTime]::UtcNow.ToString("o")
                        latency_ms = if ($null -ne $duration) { [Math]::Round($duration, 3) } else { $null }
                        setup_latency_ms = if ($null -ne $setupLatency) { [Math]::Round($setupLatency, 3) } else { $null }
                        duration_ms = if ($null -ne $duration) { [Math]::Round($duration, 3) } else { $null }
                        bytes = if ($enabled) { [int64]$preset.payload_bytes } else { 0L }
                        payload_kind = $preset.payload_kind
                        payload_label = $preset.payload_label
                        payload_bytes = [int64]$preset.payload_bytes
                        policy_limit_bytes = $null
                        policy_expected = $preset.policy_expected
                        payload_synthetic = $true
                        file_count = [int]$preset.file_count
                        file_count_class = $preset.file_count_class
                        integrity_hash_status = if ($enabled) { "synthetic-match" } else { "not-checked" }
                        expected_hash_label = $hashLabel
                        received_hash_label = if ($enabled) { $hashLabel } else { "" }
                        partial_file_status = if ($enabled) { "none" } else { "not-created" }
                        receive_path_class = if ($enabled) { "expected-lab-receive-root" } else { "not-created" }
                        cleanup_status = if ($enabled) { "clean" } else { "not-created" }
                        retry_count = if ($enabled -and $preset.variant -eq "many-small-files" -and $iteration -eq 2) { 1 } else { 0 }
                        reconnect_count = 0
                        provisional_classification = $classification
                        provisional_classification_reason = $reason
                        failure_kind = if ($status -eq "skipped") { "large-file-opt-in-required" } else { "" }
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

function Invoke-FileTransferLabDryRun {
    $observations = New-FileTransferLabObservations
    $observationPath = Join-Path $OutputRoot "file-transfer-lab-observations.json"
    $observations | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $observationPath -Encoding utf8
    & $harness -Mode Summarize -Role $Role -Scenario @("file-transfer") -ObservationPath $observationPath -OutputRoot $OutputRoot
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

function Assert-FileTransferReliabilitySignals {
    param([object[]]$PacketObservations)

    foreach ($row in $PacketObservations) {
        $status = Get-ObservationField -Observation $row -Name "status"
        $hashStatus = Get-ObservationField -Observation $row -Name "integrity_hash_status"
        $partialStatus = Get-ObservationField -Observation $row -Name "partial_file_status"
        $receivePathClass = Get-ObservationField -Observation $row -Name "receive_path_class"
        $cleanupStatus = Get-ObservationField -Observation $row -Name "cleanup_status"

        if ($hashStatus -eq "mismatch") {
            throw "File-transfer fixture represented a hash mismatch."
        }
        if ($status -eq "passed" -and $hashStatus -notin @("matched", "synthetic-match")) {
            throw "Passed file-transfer row must have matched or synthetic-match hash status."
        }
        if ($partialStatus -notin @("none", "not-created")) {
            throw "File-transfer fixture represented a partial file status: $partialStatus."
        }
        if ($receivePathClass -notin @("expected-lab-receive-root", "not-created")) {
            throw "File-transfer fixture represented an unexpected receive path class: $receivePathClass."
        }
        if ($cleanupStatus -notin @("clean", "not-created")) {
            throw "File-transfer fixture represented an unsafe cleanup status: $cleanupStatus."
        }
        if ($status -eq "passed" -and $cleanupStatus -ne "clean") {
            throw "Passed file-transfer row must have cleanup_status=clean."
        }
    }
}

function Copy-ObservationRows {
    param([object[]]$Rows)

    return @($Rows | ConvertTo-Json -Depth 8 | ConvertFrom-Json)
}

function Assert-ReliabilityMutationFails {
    param(
        [object[]]$Rows,
        [string]$Label
    )

    $threw = $false
    try {
        Assert-FileTransferReliabilitySignals -PacketObservations $Rows
    }
    catch {
        $threw = $true
    }

    if (-not $threw) {
        throw "Expected reliability validation to reject $Label."
    }
}

function Assert-SummaryMatchesObservations {
    param(
        [object]$Packet,
        [object[]]$Observations
    )

    $summary = @($Packet.summary.scenario_summaries | Where-Object { $_.scenario -eq "file-transfer" })[0]
    if ($null -eq $summary) {
        throw "Missing file-transfer summary."
    }

    $directionCount = @($Direction | Sort-Object -Unique).Count
    $enabledVariantCount = if ($IncludeLarge.IsPresent) { 4 } else { 3 }
    $expectedSuccess = $enabledVariantCount * $directionCount * $Iterations
    $expectedSkipped = if ($IncludeLarge.IsPresent) { 0 } else { 1 * $directionCount * $Iterations }
    if ($summary.success_count -ne $expectedSuccess) {
        throw "file-transfer success_count expected $expectedSuccess, found $($summary.success_count)."
    }
    if ($summary.failure_count -ne 0) {
        throw "file-transfer failure_count expected 0, found $($summary.failure_count)."
    }
    if ($summary.skipped_count -ne $expectedSkipped) {
        throw "file-transfer skipped_count expected $expectedSkipped, found $($summary.skipped_count)."
    }
    if ($summary.provisional_classifications.no_op -ne $expectedSkipped) {
        throw "file-transfer no-op classification expected $expectedSkipped, found $($summary.provisional_classifications.no_op)."
    }

    $passedDurations = @($Observations | Where-Object { $_.status -eq "passed" -and $null -ne $_.latency_ms } | ForEach-Object { [double]$_.latency_ms })
    $passedSetups = @($Observations | Where-Object { $_.status -eq "passed" -and $null -ne $_.setup_latency_ms } | ForEach-Object { [double]$_.setup_latency_ms })
    $expectedP50 = Get-Percentile -Values $passedDurations -Percentile 50
    $expectedP95 = Get-Percentile -Values $passedDurations -Percentile 95
    $expectedMax = if ($passedDurations.Count -gt 0) { [Math]::Round([double](@($passedDurations | Measure-Object -Maximum).Maximum), 3) } else { $null }
    if ($summary.latency_ms.p50 -ne $expectedP50 -or $summary.latency_ms.p95 -ne $expectedP95 -or $summary.latency_ms.max -ne $expectedMax) {
        throw "file-transfer duration percentile summary did not match fixture observations."
    }

    $expectedSetupP50 = Get-Percentile -Values $passedSetups -Percentile 50
    $expectedSetupP95 = Get-Percentile -Values $passedSetups -Percentile 95
    $expectedSetupMax = if ($passedSetups.Count -gt 0) { [Math]::Round([double](@($passedSetups | Measure-Object -Maximum).Maximum), 3) } else { $null }
    if ($summary.setup_latency_ms.p50 -ne $expectedSetupP50 -or $summary.setup_latency_ms.p95 -ne $expectedSetupP95 -or $summary.setup_latency_ms.max -ne $expectedSetupMax) {
        throw "file-transfer setup latency summary did not match fixture observations."
    }

    $expectedBytes = 0L
    foreach ($row in @($Observations | Where-Object { $_.status -eq "passed" })) {
        $expectedBytes += [int64]$row.bytes
    }
    if ([int64]$summary.bytes_total -ne $expectedBytes) {
        throw "file-transfer bytes_total expected $expectedBytes, found $($summary.bytes_total)."
    }
    if ($summary.throughput_mbps -le 0) {
        throw "file-transfer throughput_mbps should be positive for passed synthetic rows."
    }
}

function Assert-FileTransferObservationMetadata {
    param([object[]]$PacketObservations)

    $expectedVariants = @("single-small-file", "many-small-files", "medium-100mb", "large-1gb-opt-in")
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "direction" -ExpectedValues @($Direction) -Label "file-transfer directions"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "scenario_variant" -ExpectedValues $expectedVariants -Label "file-transfer scenario variants"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "file_count_class" -ExpectedValues @("single-file", "many-small-files", "medium-file", "large-file") -Label "file-transfer file-count classes"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "cleanup_status" -ExpectedValues $(if ($IncludeLarge.IsPresent) { @("clean") } else { @("clean", "not-created") }) -Label "file-transfer cleanup statuses"
    Assert-ObservationFieldSet -Observations $PacketObservations -PropertyName "integrity_hash_status" -ExpectedValues $(if ($IncludeLarge.IsPresent) { @("synthetic-match") } else { @("synthetic-match", "not-checked") }) -Label "file-transfer hash statuses"

    $directionCount = @($Direction | Sort-Object -Unique).Count
    $rowsPerVariant = $directionCount * $Iterations
    foreach ($variant in $expectedVariants) {
        $rows = @($PacketObservations | Where-Object { $_.scenario_variant -eq $variant })
        if ($rows.Count -ne $rowsPerVariant) {
            throw "$variant expected $rowsPerVariant observation rows, found $($rows.Count)."
        }
    }

    $largeRows = @($PacketObservations | Where-Object { $_.scenario_variant -eq "large-1gb-opt-in" })
    foreach ($row in $largeRows) {
        if (-not $IncludeLarge.IsPresent) {
            if ($row.status -ne "skipped" -or $row.provisional_classification -ne "no-op" -or [int64]$row.bytes -ne 0L) {
                throw "large-1gb-opt-in must remain skipped/no-op with bytes=0 unless -IncludeLarge is supplied."
            }
        }
        if ([int64]$row.payload_bytes -ne [int64]$LargeBytes) {
            throw "large-1gb-opt-in payload metadata must remain 1 GiB."
        }
    }

    foreach ($row in $PacketObservations) {
        if ([bool]$row.payload_synthetic -ne $true) {
            throw "file-transfer observations must preserve payload_synthetic=true."
        }
        if ([string]$row.payload_label -like "*:\*" -or [string]$row.payload_label -like "\\*") {
            throw "file-transfer payload labels must not contain local paths."
        }
    }
}

function Assert-ObservationIdsUnique {
    param([object[]]$PacketObservations)

    $ids = @($PacketObservations | ForEach-Object { [string]$_.id })
    $missingIds = @($ids | Where-Object { [string]::IsNullOrWhiteSpace($_) })
    if ($missingIds.Count -gt 0) {
        throw "file-transfer observations must include non-empty ids."
    }

    $duplicateIds = @($ids | Group-Object | Where-Object { $_.Count -gt 1 } | ForEach-Object { $_.Name })
    if ($duplicateIds.Count -gt 0) {
        throw "file-transfer observation ids must be unique across variant/direction/iteration rows; duplicates: $($duplicateIds -join ",")."
    }
}

function Invoke-FileTransferLabValidation {
    $result = Invoke-FileTransferLabDryRun
    $packet = Get-Content -LiteralPath $result.packet_path -Raw | ConvertFrom-Json
    $observations = @($result.observations)
    $packetObservations = @($packet.observations)

    Assert-SummaryMatchesObservations -Packet $packet -Observations $observations
    Assert-FileTransferObservationMetadata -PacketObservations $packetObservations
    Assert-ObservationIdsUnique -PacketObservations $packetObservations
    Assert-FileTransferReliabilitySignals -PacketObservations $packetObservations

    $passedRows = @($packetObservations | Where-Object { $_.status -eq "passed" })
    $badHash = Copy-ObservationRows -Rows $packetObservations
    @($badHash | Where-Object { $_.id -eq $passedRows[0].id })[0].integrity_hash_status = "mismatch"
    Assert-ReliabilityMutationFails -Rows $badHash -Label "hash mismatch"

    $partialFile = Copy-ObservationRows -Rows $packetObservations
    @($partialFile | Where-Object { $_.id -eq $passedRows[0].id })[0].partial_file_status = "partial-present"
    Assert-ReliabilityMutationFails -Rows $partialFile -Label "partial file status"

    $unexpectedPath = Copy-ObservationRows -Rows $packetObservations
    @($unexpectedPath | Where-Object { $_.id -eq $passedRows[0].id })[0].receive_path_class = "unexpected-local-path"
    Assert-ReliabilityMutationFails -Rows $unexpectedPath -Label "unexpected receive path"

    $staleCleanup = Copy-ObservationRows -Rows $packetObservations
    @($staleCleanup | Where-Object { $_.id -eq $passedRows[0].id })[0].cleanup_status = "stale-temp-detected"
    Assert-ReliabilityMutationFails -Rows $staleCleanup -Label "stale temp cleanup"

    if ($packet.privacy.payload_contents_recorded -ne $false -or $packet.privacy.raw_peer_ids_recorded -ne $false -or $packet.privacy.raw_paths_recorded -ne $false) {
        throw "File-transfer lab packet did not preserve privacy flags."
    }

    foreach ($artifact in Get-ChildItem -LiteralPath $OutputRoot -File -Recurse -Include "*.json", "*.md", "*.log") {
        $content = Get-Content -LiteralPath $artifact.FullName -Raw
        foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "actual file name", "private file", $repoRoot, $OutputRoot)) {
            if (-not [string]::IsNullOrWhiteSpace($forbidden) -and $content.Contains($forbidden)) {
                throw "File-transfer lab artifact '$($artifact.Name)' leaked forbidden token '$forbidden'."
            }
        }
    }

    Write-Host "file_transfer_lab_fixture_validation=passed"
    Write-Host "output_root=[redacted]"
}

switch ($Mode) {
    "DryRun" {
        Invoke-FileTransferLabDryRun | Out-Null
    }
    "Validate" {
        Invoke-FileTransferLabValidation
    }
}
