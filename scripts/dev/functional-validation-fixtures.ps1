[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'input-trace-contract.ps1')
. (Join-Path $PSScriptRoot 'functional-evidence.ps1')

function Assert-Rejected {
    param([string]$Name, [scriptblock]$Action, [string]$ExpectedMessage)
    $failure = $null
    try { & $Action | Out-Null }
    catch { $failure = $_ }
    if ($null -eq $failure) { throw "$Name accepted invalid evidence" }
    if ($failure.ToString() -notmatch $ExpectedMessage) {
        throw "$Name failed for an unexpected reason: $failure"
    }
}

function New-TraceSummary {
    param([int[]]$Apply = @(12, 14, 16, 18), [int[]]$Receive = @(3, 4, 5, 6), [int[]]$LocalApply = @(2, 3, 4, 5))
    $applyList = New-Object 'System.Collections.Generic.List[int]'
    $receiveList = New-Object 'System.Collections.Generic.List[int]'
    $localList = New-Object 'System.Collections.Generic.List[int]'
    $applyList.AddRange($Apply)
    $receiveList.AddRange($Receive)
    $localList.AddRange($LocalApply)
    return Summarize-Metrics -Label fixture -CaptureToApply $applyList -CaptureToReceive $receiveList -ReceiveToApply $localList -ClockSkewThresholdMs 500
}

$known = New-TraceSummary
if ($known.CaptureToApplyP50 -ne 14 -or $known.CaptureToApplyP95 -ne 18 -or $known.CaptureToApplyJitterP95 -ne 2) {
    throw 'Known input did not produce nearest-rank percentiles and consecutive-sample jitter'
}
Assert-InputTraceBudgets -Summary $known -MinimumSamples 4
Assert-Rejected 'missing samples' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Apply @()) -MinimumSamples 4 } 'fresh samples'
Assert-Rejected 'insufficient samples' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Receive @(1)) -MinimumSamples 4 } 'fresh samples'
Assert-Rejected 'slow apply' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Apply @(12, 14, 16, 500)) -MinimumSamples 4 } 'budget exceeded'
Assert-Rejected 'slow receive' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Receive @(3, 4, 5, 100)) -MinimumSamples 4 } 'budget exceeded'
Assert-Rejected 'slow injection' { Assert-InputTraceBudgets -Summary (New-TraceSummary -LocalApply @(2, 3, 4, 50)) -MinimumSamples 4 } 'budget exceeded'
Assert-Rejected 'jitter' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Apply @(1, 24, 1, 24)) -MinimumSamples 4 } 'budget exceeded'
Assert-Rejected 'clock skew cannot become a local-only pass' { Assert-InputTraceBudgets -Summary (New-TraceSummary -Apply @(1012, 1014, 1016, 1018)) -MinimumSamples 4 } 'Clock skew suspected'
Assert-Rejected 'missing metric' {
    $summary = New-TraceSummary
    $summary.CaptureToReceiveP95 = $null
    Assert-InputTraceBudgets -Summary $summary -MinimumSamples 4
} 'missing or invalid'
Assert-Rejected 'nonfinite metric' {
    $summary = New-TraceSummary
    $summary.CaptureToReceiveP95 = [double]::NaN
    Assert-InputTraceBudgets -Summary $summary -MinimumSamples 4
} 'missing or invalid'
foreach ($line in @('other_capture_to_apply_ms=2', 'capture_to_apply_ms=-1', 'capture_to_apply_ms=2junk')) {
    if ($null -ne (Get-MetricValue -Line $line -MetricName 'capture_to_apply_ms')) { throw 'Malformed sample was accepted' }
}
if ((Get-MetricValue -Line 'kind=input_inject_applied capture_to_apply_ms=12 receive_to_apply_ms=3' -MetricName 'capture_to_apply_ms') -ne 12) {
    throw 'Valid event sample was not parsed'
}

function New-PairedFixture {
    param([int]$Samples = 4)
    $identity = @{
        machine_id = 'fixture-local'
        daemon_version = '5.0.16'
        protocol_version = '4.4'
        platform = 'windows'
        architecture = 'x86_64'
        daemon_instance_id = [guid]::NewGuid().ToString()
        binary_sha256 = 'a' * 64
        source_revision = 'b' * 40
    }
    $remote = $identity.Clone()
    $remote.machine_id = 'fixture-remote'
    $remote.daemon_instance_id = [guid]::NewGuid().ToString()
    $probes = foreach ($name in @('transport_rtt', 'bulk_echo_integrity')) {
        $payload = if ($name -eq 'transport_rtt') { 64 } else { 128 }
        @{
            name = $name; requested_samples = $Samples; completed_samples = $Samples
            payload_bytes_per_sample = $payload; verified_round_trip_bytes = $Samples * $payload * 2
            latency_us = @(1..$Samples | ForEach-Object { $_ * 100 }); p50_us = [int][Math]::Ceiling($Samples * 0.5) * 100; p95_us = [int][Math]::Ceiling($Samples * 0.95) * 100; errors = @()
        }
    }
    return @{
        schema_version = 1; run_id = [guid]::NewGuid().ToString(); started_at = [DateTimeOffset]::UtcNow.ToString('o'); duration_ms = 3
        local = $identity; remote = $remote; evidence_category = 'real_paired'
        local_transport_session_id = 1; remote_transport_session_id = 2; passed = $true; tests = @($probes)
        not_tested = @('physical_keyboard_mouse_injection', 'emergency_unlock', 'clipboard', 'file_workflows', 'reconnect_recovery', 'cpu_memory_disk_budgets', 'physical_two_pc_attestation')
    } | ConvertTo-Json -Depth 12 | ConvertFrom-Json
}

$valid = New-PairedFixture
$validated = Assert-PairedTestReport -Report $valid -MinimumSamples 4 -ExpectedDaemonSha256 ('a' * 64) -ExpectedSourceRevision ('b' * 40) -RequireRealPaired
if (-not $validated.candidate_hash_bound -or $validated.physical_two_pc_acceptance -ne 'not_proven') { throw 'Valid transport evidence was misclassified' }
$cases = @(
    @{ name = 'empty samples'; mutation = { param($r) $r.tests[0].latency_us = @() }; message = 'sample count' },
    @{ name = 'forged percentile'; mutation = { param($r) $r.tests[1].p95_us = 1 }; message = 'raw samples' },
    @{ name = 'forged bytes'; mutation = { param($r) $r.tests[1].verified_round_trip_bytes = 10 }; message = 'byte count' },
    @{ name = 'fractional sample'; mutation = { param($r) $r.tests[0].latency_us[0] = 0.5 }; message = 'integer range' },
    @{ name = 'negative sample'; mutation = { param($r) $r.tests[0].latency_us[0] = -1 }; message = 'integer range' },
    @{ name = 'missing endpoint'; mutation = { param($r) $r.remote = $null }; message = 'Remote endpoint' },
    @{ name = 'same endpoint'; mutation = { param($r) $r.remote.daemon_instance_id = $r.local.daemon_instance_id }; message = 'distinct daemon' },
    @{ name = 'missing binary hash'; mutation = { param($r) $r.remote.binary_sha256 = $null }; message = 'SHA-256' },
    @{ name = 'wrong candidate hash'; mutation = { param($r) $r.remote.binary_sha256 = 'c' * 64 }; message = 'candidate SHA-256' },
    @{ name = 'wrong source'; mutation = { param($r) $r.remote.source_revision = $null }; message = 'source revision' },
    @{ name = 'missing session'; mutation = { param($r) $r.local_transport_session_id = $null }; message = 'must be a number' },
    @{ name = 'synthetic promotion'; mutation = { param($r) $r.evidence_category = 'synthetic' }; message = 'Synthetic' },
    @{ name = 'loopback hardware claim'; mutation = { param($r) $r.evidence_category = 'loopback' }; message = 'Non-loopback' },
    @{ name = 'hidden probe failure'; mutation = { param($r) $r.tests[0].errors = @('failed') }; message = 'failed samples' },
    @{ name = 'stale evidence'; mutation = { param($r) $r.started_at = [DateTimeOffset]::UtcNow.AddDays(-8).ToString('o') }; message = 'stale' },
    @{ name = 'lost scope warning'; mutation = { param($r) $r.not_tested = @() }; message = 'untested desktop' }
)
foreach ($case in $cases) {
    $report = New-PairedFixture
    & $case.mutation $report
    Assert-Rejected $case.name {
        Assert-PairedTestReport -Report $report -MinimumSamples 4 -ExpectedDaemonSha256 ('a' * 64) -ExpectedSourceRevision ('b' * 40) -RequireRealPaired
    } $case.message
}

# Exercise the user-facing command boundary, including the nonzero rejection exit.
$fixtureRoot = Join-Path $PSScriptRoot "../../artifacts/functional-validation-fixtures/$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $fixtureRoot | Out-Null
$inputPath = Join-Path $fixtureRoot 'paired-test.json'
$outputPath = Join-Path $fixtureRoot 'validated.json'
$hostExe = (Get-Process -Id $PID).Path
$valid | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $inputPath -Encoding utf8
$priorPreference = $ErrorActionPreference
try {
    $ErrorActionPreference = 'Continue'
    $validOutput = & $hostExe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'validate-paired-test.ps1') -ReportPath $inputPath -OutputPath $outputPath -MinimumSamples 4 -ExpectedDaemonSha256 ('a' * 64) *>&1
    $validExit = $LASTEXITCODE
    $valid.tests[0].p95_us = 1
    $valid | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $inputPath -Encoding utf8
    $invalidOutput = & $hostExe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'validate-paired-test.ps1') -ReportPath $inputPath -MinimumSamples 4 *>&1
    $invalidExit = $LASTEXITCODE
}
finally { $ErrorActionPreference = $priorPreference }
if ($validExit -ne 0 -or -not (Test-Path -LiteralPath $outputPath)) { throw "Valid report command failed: $validOutput" }
if ($invalidExit -eq 0 -or ($invalidOutput -join ' ') -notmatch 'raw samples') { throw 'Forged report did not fail at the command boundary' }
$packet = Get-Content -LiteralPath $outputPath -Raw | ConvertFrom-Json
if ($packet.report_sha256 -notmatch '^[a-f0-9]{64}$') { throw 'Validator did not preserve input artifact identity' }

# The readiness packet must retain validator failure, rather than turning an
# invalid supplied report into a skipped or successful gate. No runtime starts.
foreach ($corrupt in @($false, $true)) {
    $candidate = New-PairedFixture -Samples 20
    if ($corrupt) { $candidate.tests[0].p95_us = 1 }
    $candidate | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $inputPath -Encoding utf8
    $readinessOutput = Join-Path $fixtureRoot "readiness-$corrupt"
    $priorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $gateOutput = & $hostExe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'release-readiness.ps1') -SkipUnitGates -PairedTestReportPath $inputPath -ExpectedDaemonSha256 ('a' * 64) -OutputRoot $readinessOutput *>&1
        $gateExit = $LASTEXITCODE
    }
    finally { $ErrorActionPreference = $priorPreference }
    $readiness = Get-Content -LiteralPath (Join-Path $readinessOutput 'release-readiness.json') -Raw | ConvertFrom-Json
    $gate = @($readiness.results | Where-Object { $_.id -eq 'paired_transport_contract' })
    $topology = @($readiness.results | Where-Object { $_.id -eq 'layout_topology_validation' })
    if ($topology.Count -ne 1 -or $topology[0].category -ne 'unit' -or $topology[0].status -ne 'skipped') { throw 'Topology validation was incorrectly classified as an executed runtime gate' }
    $expectedStatus = if ($corrupt) { 'failed' } else { 'passed' }
    $expectedExit = if ($corrupt) { 1 } else { 0 }
    if ($gate.Count -ne 1 -or $gate[0].status -ne $expectedStatus -or $gateExit -ne $expectedExit) { throw "Readiness did not preserve paired gate result ($expectedStatus): $gateOutput" }
}

$transport = @(
    @{ kind = 'synthetic_worker'; scenario = 'connection_refused'; attempts = 3; elapsed_ms = 3250; noisy_reconcile = $true },
    @{ kind = 'synthetic_worker'; scenario = 'immediate_session_close'; attempts = 3; elapsed_ms = 3251; noisy_reconcile = $true }
)
$transport += @(1..10 | ForEach-Object { @{ kind = 'synthetic_transport'; scenario = 'unrelated_peer_input_during_stalled_bulk'; unrelated_peer_elapsed_us = 2000; stalled_peer_pending = $true; deadline_ms = 250 } })
Assert-FunctionalBenchmarkMetrics -Benchmark transport -Metrics $transport
Assert-Rejected 'empty benchmark' { Assert-FunctionalBenchmarkMetrics -Benchmark transport -Metrics @() } 'two worker and ten fairness'
$transport[0].attempts = 50000
Assert-Rejected 'retry storm' { Assert-FunctionalBenchmarkMetrics -Benchmark transport -Metrics $transport } 'integer range'
$transport[0].attempts = 3
$transport[2].unrelated_peer_elapsed_us = 250000
Assert-Rejected 'peer starvation' { Assert-FunctionalBenchmarkMetrics -Benchmark transport -Metrics $transport } 'integer range'
$logging = @{ records = 262144; bytes_processed = 268435456; elapsed_ms = 1000; throughput_mib_per_sec = 256; retained_bytes = 104857600; peak_retained_bytes = 104857600; retained_files = 10; cap_total_bytes = 104857600; cap_segment_bytes = 10485760; cap_files = 10 }
Assert-FunctionalBenchmarkMetrics -Benchmark logging -Metrics @($logging)
$logging.peak_retained_bytes = 104857601
Assert-Rejected 'peak disk overflow' { Assert-FunctionalBenchmarkMetrics -Benchmark logging -Metrics @($logging) } 'integer range'

function New-UiFixture {
    $uiCases = @()
    foreach ($fixture in @('home', 'arrange', 'files')) {
        foreach ($viewport in @(@(1100, 800), @(800, 600))) {
            $uiCases += @{
                fixture = $fixture; viewport_points = $viewport; pixels_per_point = 1; warmup_frames = 30; measured_frames = 200
                samples_ns = @(1..200 | ForEach-Object { @{ ui_layout_ns = $_ * 100; tessellation_ns = $_ * 10; combined_ns = $_ * 110 } })
                summary_ns = @{
                    ui_layout = @{ min = 100; p50 = 10000; p95 = 19000; max = 20000 }
                    tessellation = @{ min = 10; p50 = 1000; p95 = 1900; max = 2000 }
                    combined = @{ min = 110; p50 = 11000; p95 = 20900; max = 22000 }
                }
            }
        }
    }
    return @{ schema_version = 'boundless.ui_frame_benchmark.v1'; measurement = @{ percentile_method = 'nearest rank'; hardware_dependent_pass_thresholds = $false }; cases = $uiCases }
}
$ui = New-UiFixture
Assert-FunctionalBenchmarkMetrics -Benchmark ui -Metrics @($ui)
$ui.cases[0].samples_ns = @()
Assert-Rejected 'missing UI raw samples' { Assert-FunctionalBenchmarkMetrics -Benchmark ui -Metrics @($ui) } 'sample count'
$ui = New-UiFixture
$ui.cases[0].summary_ns.combined.p95 = 1
Assert-Rejected 'forged UI percentile' { Assert-FunctionalBenchmarkMetrics -Benchmark ui -Metrics @($ui) } 'raw samples'
$ui = New-UiFixture
$ui.cases[0].samples_ns[0].combined_ns = 1
Assert-Rejected 'forged UI component sum' { Assert-FunctionalBenchmarkMetrics -Benchmark ui -Metrics @($ui) } 'measured components'

Write-Host 'functional_validation_fixtures=passed trace_contract=passed paired_transport_contract=passed benchmark_contract=passed'
$global:LASTEXITCODE = 0
