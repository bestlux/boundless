# Shared validation for reports emitted by the actual paired-test controller.
# This checks consistency and candidate identity, not authenticity of an edited JSON file.

function Assert-EvidenceInteger {
    param([object]$Value, [string]$Name, [decimal]$Minimum = 0, [decimal]$Maximum = [long]::MaxValue)
    if ($null -eq $Value -or $Value -is [string] -or $Value -is [bool]) { throw "$Name must be a number" }
    try { $number = [decimal]$Value } catch { throw "$Name must be finite" }
    if ($number -lt $Minimum -or $number -gt $Maximum -or [decimal]::Truncate($number) -ne $number) {
        throw "$Name is outside the supported integer range"
    }
}

function Assert-PairedTestReport {
    param(
        [object]$Report,
        [ValidateRange(1, 100)][int]$MinimumSamples = 20,
        [string]$ExpectedDaemonSha256 = '',
        [string]$ExpectedSourceRevision = '',
        [switch]$RequireRealPaired,
        [ValidateRange(1, 8760)][int]$MaxEvidenceAgeHours = 168
    )

    Assert-EvidenceInteger $Report.schema_version 'paired-test report schema' 1 1
    if ($Report.passed -isnot [bool] -or -not $Report.passed) { throw 'Paired test did not pass' }
    $runId = [guid]::Empty
    if (-not [guid]::TryParse([string]$Report.run_id, [ref]$runId) -or $runId -eq [guid]::Empty) { throw 'Missing or invalid run identity' }
    $started = [DateTimeOffset]::MinValue
    if (-not [DateTimeOffset]::TryParse([string]$Report.started_at, [ref]$started)) { throw 'Missing or invalid measurement start time' }
    $age = [DateTimeOffset]::UtcNow - $started
    if ($age.TotalMinutes -lt -5 -or $age.TotalHours -gt $MaxEvidenceAgeHours) { throw 'Measurement time is stale or in the future' }
    Assert-EvidenceInteger $Report.duration_ms 'duration_ms' 0 60000
    if ($Report.evidence_category -notin @('loopback', 'real_paired')) { throw 'Synthetic or missing evidence category cannot prove transport behavior' }
    if ($RequireRealPaired -and $Report.evidence_category -ne 'real_paired') { throw 'Non-loopback paired transport evidence is required' }
    if ($null -eq $Report.remote) { throw 'Remote endpoint identity is missing' }
    if ($ExpectedDaemonSha256 -and $ExpectedDaemonSha256 -notmatch '^[0-9a-fA-F]{64}$') { throw 'Expected daemon SHA-256 must contain 64 hex digits' }
    if ($ExpectedSourceRevision -and $ExpectedSourceRevision -notmatch '^[0-9a-fA-F]{40}$') { throw 'Expected source revision must be a full commit SHA' }

    foreach ($role in @('local', 'remote')) {
        $identity = $Report.$role
        foreach ($field in @('machine_id', 'daemon_version', 'protocol_version', 'platform', 'architecture', 'daemon_instance_id')) {
            if ([string]::IsNullOrWhiteSpace([string]$identity.$field)) { throw "$role endpoint is missing $field" }
        }
        if ([string]$identity.binary_sha256 -notmatch '^[0-9a-fA-F]{64}$') { throw "$role endpoint binary SHA-256 is missing or invalid" }
        if ($ExpectedDaemonSha256 -and $identity.binary_sha256 -ine $ExpectedDaemonSha256) { throw "$role endpoint binary does not match the candidate SHA-256" }
        if ($ExpectedSourceRevision -and $identity.source_revision -ine $ExpectedSourceRevision) { throw "$role endpoint source revision does not match the candidate" }
    }
    if ($Report.local.machine_id -eq $Report.remote.machine_id -or $Report.local.daemon_instance_id -eq $Report.remote.daemon_instance_id) {
        throw 'Report must identify two distinct daemon endpoints'
    }
    if ($Report.local.protocol_version -ne $Report.remote.protocol_version) { throw 'Endpoint protocol identities disagree' }
    Assert-EvidenceInteger $Report.local_transport_session_id 'local transport session' 1
    Assert-EvidenceInteger $Report.remote_transport_session_id 'remote transport session' 1

    $tests = @($Report.tests)
    $expectedNames = @('transport_rtt', 'bulk_echo_integrity')
    if ($tests.Count -ne $expectedNames.Count) { throw 'Expected both transport RTT and bulk echo integrity results' }
    foreach ($name in $expectedNames) {
        $matches = @($tests | Where-Object { $_.name -eq $name })
        if ($matches.Count -ne 1) { throw "Missing or duplicate probe: $name" }
        $probe = $matches[0]
        Assert-EvidenceInteger $probe.requested_samples "$name requested samples" $MinimumSamples 100
        Assert-EvidenceInteger $probe.completed_samples "$name completed samples" $MinimumSamples 100
        if ($probe.completed_samples -ne $probe.requested_samples -or @($probe.errors).Count -ne 0) { throw "$name has incomplete or failed samples" }
        $samples = @($probe.latency_us)
        if ($samples.Count -ne $probe.completed_samples) { throw "$name sample count does not match the raw observations" }
        foreach ($sample in $samples) { Assert-EvidenceInteger $sample "$name latency sample" 0 60000000 }
        $ordered = @($samples | Sort-Object)
        foreach ($percent in @(50, 95)) {
            $expected = $ordered[[Math]::Max(0, [int][Math]::Ceiling($ordered.Count * $percent / 100.0) - 1)]
            $actual = $probe."p${percent}_us"
            Assert-EvidenceInteger $actual "$name p$percent" 0 60000000
            if ($actual -ne $expected) { throw "$name p$percent does not match its raw samples" }
        }
        Assert-EvidenceInteger $probe.payload_bytes_per_sample "$name payload size" 1 65536
        if ($name -eq 'transport_rtt' -and $probe.payload_bytes_per_sample -ne 64) { throw 'RTT probe must use the protocol-defined 64-byte payload' }
        if ($name -eq 'bulk_echo_integrity' -and $probe.payload_bytes_per_sample -lt 1) { throw 'Bulk echo must verify a nonempty payload' }
        $expectedBytes = [decimal]$probe.completed_samples * [decimal]$probe.payload_bytes_per_sample * 2
        Assert-EvidenceInteger $probe.verified_round_trip_bytes "$name verified bytes" 0
        if ($probe.verified_round_trip_bytes -ne $expectedBytes) { throw "$name verified byte count is inconsistent" }
    }
    foreach ($scope in @('physical_keyboard_mouse_injection', 'emergency_unlock', 'clipboard', 'file_workflows', 'reconnect_recovery', 'cpu_memory_disk_budgets', 'physical_two_pc_attestation')) {
        if (@($Report.not_tested) -notcontains $scope) { throw "Transport report must declare its untested desktop surface: $scope" }
    }

    return [pscustomobject]@{
        schema_version = 'boundless.validation.paired_transport.v1'
        run_id = $Report.run_id
        scope = 'authenticated-transport-only'
        evidence_category = $Report.evidence_category
        passed = $true
        candidate_hash_bound = -not [string]::IsNullOrWhiteSpace($ExpectedDaemonSha256)
        source_revision_bound = -not [string]::IsNullOrWhiteSpace($ExpectedSourceRevision)
        local_binary_sha256 = $Report.local.binary_sha256
        remote_binary_sha256 = $Report.remote.binary_sha256
        local_version = $Report.local.daemon_version
        remote_version = $Report.remote.daemon_version
        tests = $tests
        not_tested = $Report.not_tested
        physical_two_pc_acceptance = 'not_proven'
        provenance_limit = 'Reported process hashes bind artifact identity; JSON and peer self-reports are not hardware attestation.'
    }
}

function Assert-FunctionalBenchmarkMetrics {
    param([ValidateSet('transport', 'logging', 'ui')][string]$Benchmark, [object[]]$Metrics)
    if ($Benchmark -eq 'transport') {
        if ($Metrics.Count -ne 12) { throw 'Transport benchmark must emit two worker and ten fairness measurements' }
        foreach ($scenario in @('connection_refused', 'immediate_session_close')) {
            $rows = @($Metrics | Where-Object { $_.kind -eq 'synthetic_worker' -and $_.scenario -eq $scenario })
            if ($rows.Count -ne 1) { throw "Missing or duplicate worker measurement: $scenario" }
            Assert-EvidenceInteger $rows[0].attempts "$scenario attempts" 3 3
            Assert-EvidenceInteger $rows[0].elapsed_ms "$scenario elapsed_ms" 3250 60000
            if ($rows[0].noisy_reconcile -isnot [bool] -or -not $rows[0].noisy_reconcile) { throw 'Worker benchmark did not exercise reconcile noise' }
        }
        $fairness = @($Metrics | Where-Object { $_.kind -eq 'synthetic_transport' -and $_.scenario -eq 'unrelated_peer_input_during_stalled_bulk' })
        if ($fairness.Count -ne 10) { throw 'Expected ten measured fairness samples' }
        foreach ($sample in $fairness) {
            Assert-EvidenceInteger $sample.deadline_ms 'fairness deadline' 250 250
            Assert-EvidenceInteger $sample.unrelated_peer_elapsed_us 'unrelated peer elapsed_us' 0 249999
            if ($sample.stalled_peer_pending -isnot [bool] -or -not $sample.stalled_peer_pending) { throw 'Fairness measurement did not overlap stalled bulk work' }
        }
    }
    elseif ($Benchmark -eq 'logging') {
        if ($Metrics.Count -ne 1) { throw 'Logging benchmark must emit exactly one filesystem measurement' }
        $sample = $Metrics[0]
        Assert-EvidenceInteger $sample.records 'logging records' 1
        Assert-EvidenceInteger $sample.bytes_processed 'logging processed bytes' 268435456
        Assert-EvidenceInteger $sample.elapsed_ms 'logging elapsed_ms' 1
        Assert-EvidenceInteger $sample.cap_total_bytes 'total disk cap' 104857600 104857600
        Assert-EvidenceInteger $sample.cap_segment_bytes 'segment cap' 10485760 10485760
        Assert-EvidenceInteger $sample.cap_files 'file count cap' 10 10
        Assert-EvidenceInteger $sample.retained_bytes 'retained bytes' 0 $sample.cap_total_bytes
        Assert-EvidenceInteger $sample.peak_retained_bytes 'peak retained bytes' 1 $sample.cap_total_bytes
        Assert-EvidenceInteger $sample.retained_files 'retained file count' 1 $sample.cap_files
        if ($sample.retained_bytes -gt $sample.peak_retained_bytes) { throw 'Final disk use exceeds reported peak' }
        if ([double]::IsNaN([double]$sample.throughput_mib_per_sec) -or [double]::IsInfinity([double]$sample.throughput_mib_per_sec) -or $sample.throughput_mib_per_sec -le 0) {
            throw 'Logging throughput must be finite and positive'
        }
    }
    else {
        if ($Metrics.Count -ne 1 -or $Metrics[0].schema_version -ne 'boundless.ui_frame_benchmark.v1') { throw 'Expected one UI frame benchmark report' }
        $report = $Metrics[0]
        if ($report.measurement.percentile_method -ne 'nearest rank' -or $report.measurement.hardware_dependent_pass_thresholds -isnot [bool] -or $report.measurement.hardware_dependent_pass_thresholds) { throw 'Unexpected UI measurement scope' }
        if (@($report.cases).Count -ne 6) { throw 'Expected six UI fixture and viewport cases' }
        foreach ($fixture in @('home', 'arrange', 'files')) {
            foreach ($viewport in @(@(1100, 800), @(800, 600))) {
                $cases = @($report.cases | Where-Object { $_.fixture -eq $fixture -and @($_.viewport_points).Count -eq 2 -and $_.viewport_points[0] -eq $viewport[0] -and $_.viewport_points[1] -eq $viewport[1] })
                if ($cases.Count -ne 1) { throw "Missing or duplicate UI case: $fixture $viewport" }
                $case = $cases[0]
                Assert-EvidenceInteger $case.warmup_frames 'UI warmup frames' 30 30
                Assert-EvidenceInteger $case.measured_frames 'UI measured frames' 200 200
                if ($case.pixels_per_point -ne 1 -or @($case.samples_ns).Count -ne 200) { throw 'UI sample count or pixel scale is inconsistent' }
                foreach ($sample in $case.samples_ns) {
                    foreach ($field in @('ui_layout_ns', 'tessellation_ns', 'combined_ns')) { Assert-EvidenceInteger $sample.$field "UI $field" 0 }
                    if ([decimal]$sample.combined_ns -ne ([decimal]$sample.ui_layout_ns + [decimal]$sample.tessellation_ns)) { throw 'UI combined sample does not equal its measured components' }
                }
                foreach ($metric in @('ui_layout', 'tessellation', 'combined')) {
                    $ordered = @($case.samples_ns | ForEach-Object { $_."${metric}_ns" } | Sort-Object)
                    $expected = @{ min = $ordered[0]; p50 = $ordered[99]; p95 = $ordered[189]; max = $ordered[199] }
                    foreach ($stat in @('min', 'p50', 'p95', 'max')) {
                        Assert-EvidenceInteger $case.summary_ns.$metric.$stat "UI $metric $stat" 0
                        if ($case.summary_ns.$metric.$stat -ne $expected[$stat]) { throw "UI $metric $stat does not match raw samples" }
                    }
                }
            }
        }
    }
}
