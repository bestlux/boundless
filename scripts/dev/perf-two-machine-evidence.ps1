[CmdletBinding()]
param(
    [ValidateSet("DryRun", "Capture", "Summarize", "Validate")]
    [string]$Mode = "DryRun",

    [ValidateSet("coordinator", "peer", "standalone")]
    [string]$Role = "standalone",

    [ValidateSet("text-clipboard", "image-clipboard", "file-transfer", "reconnect-input", "soak")]
    [string[]]$Scenario = @("text-clipboard", "image-clipboard", "file-transfer", "reconnect-input", "soak"),

    [ValidateRange(1, 10000)]
    [int]$Iterations = 5,

    [string]$HostLabel = "",
    [string]$ObservationPath = "",
    [string]$OutputRoot = "",
    [switch]$ReleaseEvidence
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/performance/two-machine-evidence/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

$jsonPath = Join-Path $OutputRoot "two-machine-evidence.json"
$markdownPath = Join-Path $OutputRoot "two-machine-evidence.md"

function Get-RelativeArtifactPath {
    param([string]$Path)

    $resolvedPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
    $resolvedRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
    if ($resolvedPath.StartsWith($resolvedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $resolvedPath.Substring($resolvedRoot.Length).TrimStart([System.IO.Path]::DirectorySeparatorChar, [System.IO.Path]::AltDirectorySeparatorChar)
    }

    return Split-Path -Leaf $resolvedPath
}

function Redact-Text {
    param([object]$Value)

    if ($null -eq $Value) {
        return ""
    }

    $text = [string]$Value
    if ([string]::IsNullOrWhiteSpace($text)) {
        return ""
    }

    $text = [regex]::Replace($text, "(?i)\b(peer_id|machine_id|request_id|transfer_id|owner_peer_id|capture_target_peer_id)=\S+", '$1=[redacted]')
    $text = [regex]::Replace($text, "\bS-\d(?:-\d+){2,}\b", "[redacted-sid]")
    $text = [regex]::Replace($text, "\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b", "[redacted-id]")
    $text = [regex]::Replace($text, "\b(?:\d{1,3}\.){3}\d{1,3}\b", "[redacted-ip]")
    $text = [regex]::Replace($text, "(?i)\b[A-Z]:\\[^\s,;|]+", "[redacted-path]")
    $text = [regex]::Replace($text, "\\\\[^\s,;|]+", "[redacted-path]")
    if ($text.Length -gt 160) {
        $text = $text.Substring(0, 160)
    }

    return $text
}

function Get-GitValue {
    param([string[]]$Arguments)

    try {
        return (& git -C $repoRoot @Arguments 2>$null | Out-String).Trim()
    }
    catch {
        return ""
    }
}

function Get-BuildEvidence {
    $binaries = New-Object System.Collections.Generic.List[object]
    foreach ($item in @(
            @{ name = "boundlessctl"; relative_path = "target/debug/boundlessctl.exe" },
            @{ name = "boundlessd"; relative_path = "target/debug/boundlessd.exe" },
            @{ name = "boundlesstray"; relative_path = "target/debug/boundlesstray.exe" }
        )) {
        $path = Join-Path $repoRoot $item.relative_path
        $present = Test-Path -LiteralPath $path
        $versionOutput = ""
        if ($present -and $item.name -ne "boundlesstray") {
            try {
                $versionOutput = (& $path --version 2>$null | Out-String).Trim()
            }
            catch {
                $versionOutput = ""
            }
        }

        $binaries.Add([pscustomobject]@{
                name = $item.name
                present = $present
                path_class = "repo-target"
                version_output = Redact-Text $versionOutput
            })
    }

    return [pscustomobject]@{
        source = "repo-worktree"
        binaries_probed = $true
        binary_paths_recorded = $false
        binaries = @($binaries.ToArray())
    }
}

function Get-NetworkEvidence {
    $profiles = @()
    $ipFamilies = [ordered]@{
        ipv4_available = $false
        ipv6_available = $false
    }

    try {
        $profiles = @(Get-NetConnectionProfile -ErrorAction Stop | ForEach-Object {
                [pscustomobject]@{
                    category = Redact-Text $_.NetworkCategory
                    ipv4_connectivity = Redact-Text $_.IPv4Connectivity
                    ipv6_connectivity = Redact-Text $_.IPv6Connectivity
                }
            })
    }
    catch {
        $profiles = @()
    }

    try {
        $addresses = @(Get-NetIPAddress -AddressFamily IPv4, IPv6 -ErrorAction Stop)
        $ipFamilies.ipv4_available = @($addresses | Where-Object { $_.AddressFamily -eq "IPv4" }).Count -gt 0
        $ipFamilies.ipv6_available = @($addresses | Where-Object { $_.AddressFamily -eq "IPv6" }).Count -gt 0
    }
    catch {
    }

    return [pscustomobject]@{
        profiles = $profiles
        ip_families = $ipFamilies
        raw_addresses_recorded = $false
    }
}

function Get-PathClass {
    param([string]$PathName)

    if ([string]::IsNullOrWhiteSpace($PathName)) {
        return "unknown"
    }
    if ($PathName -match '(?i)^["'']?C:\\Program Files\\Boundless\\') {
        return "program-files-boundless"
    }
    if ($PathName -match "(?i)\\Users\\") {
        return "user-profile-redacted"
    }

    return "other-redacted"
}

function Get-ServiceAccountClass {
    param([string]$StartName)

    if ([string]::IsNullOrWhiteSpace($StartName)) {
        return "unknown"
    }

    switch -Regex ($StartName) {
        "^(?i:LocalSystem)$" { return "LocalSystem" }
        "^(?i:NT AUTHORITY\\LocalService|LocalService)$" { return "LocalService" }
        "^(?i:NT AUTHORITY\\NetworkService|NetworkService)$" { return "NetworkService" }
        default { return "user-or-domain-redacted" }
    }
}

function Get-BoundlessProcessEvidence {
    $service = [ordered]@{
        installed = $false
        status = "not-installed"
        start_type = ""
        start_account_class = ""
        binary_path_class = "unknown"
    }
    try {
        $svc = Get-CimInstance -ClassName Win32_Service -Filter "Name = 'BoundlessService'" -ErrorAction Stop
        if ($null -ne $svc) {
            $service.installed = $true
            $service.status = Redact-Text $svc.State
            $service.start_type = Redact-Text $svc.StartMode
            $service.start_account_class = Get-ServiceAccountClass $svc.StartName
            $service.binary_path_class = Get-PathClass $svc.PathName
        }
    }
    catch {
    }

    $trayCount = 0
    $daemonCount = 0
    try {
        $trayCount = @(Get-Process -Name "boundlesstray" -ErrorAction SilentlyContinue).Count
        $daemonCount = @(Get-Process -Name "boundlessd", "boundless-service" -ErrorAction SilentlyContinue).Count
    }
    catch {
    }

    return [pscustomobject]@{
        service = $service
        tray_process_count = $trayCount
        daemon_process_count = $daemonCount
        process_paths_recorded = $false
    }
}

function Get-EnvironmentEvidence {
    param(
        [string]$RunRole,
        [string]$RunHostLabel
    )

    $os = [ordered]@{
        platform = [System.Environment]::OSVersion.Platform.ToString()
        version = [System.Environment]::OSVersion.Version.ToString()
        build = [System.Environment]::OSVersion.Version.Build
        caption = ""
    }
    try {
        $osInfo = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop
        if ($null -ne $osInfo) {
            $os.caption = Redact-Text $osInfo.Caption
            $os.version = Redact-Text $osInfo.Version
            $os.build = $osInfo.BuildNumber
        }
    }
    catch {
    }

    $effectiveHostLabel = $RunHostLabel
    $hostLabelSource = "user-provided"
    if ([string]::IsNullOrWhiteSpace($effectiveHostLabel)) {
        $effectiveHostLabel = $RunRole
        $hostLabelSource = "role-default"
    }

    return [pscustomobject]@{
        role = $RunRole
        host_label = Redact-Text $effectiveHostLabel
        host_label_source = $hostLabelSource
        raw_machine_name_recorded = $false
        os = $os
        powershell = $PSVersionTable.PSVersion.ToString()
        process_architecture = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
        network = Get-NetworkEvidence
        boundless = Get-BoundlessProcessEvidence
    }
}

function Get-ScenarioBytes {
    param([string]$ScenarioName)

    switch ($ScenarioName) {
        "text-clipboard" { return 128L }
        "image-clipboard" { return 8388608L }
        "file-transfer" { return 16777216L }
        "reconnect-input" { return 0L }
        "soak" { return 1048576L }
        default { return 0L }
    }
}

function Get-ScenarioBaseLatency {
    param([string]$ScenarioName)

    switch ($ScenarioName) {
        "text-clipboard" { return 18.0 }
        "image-clipboard" { return 85.0 }
        "file-transfer" { return 240.0 }
        "reconnect-input" { return 320.0 }
        "soak" { return 1000.0 }
        default { return 50.0 }
    }
}

function New-Observation {
    param(
        [string]$RunScenario,
        [int]$RunIteration,
        [string]$RunRole,
        [string]$Status,
        [Nullable[double]]$LatencyMs,
        [Nullable[double]]$DurationMs,
        [Nullable[int64]]$Bytes,
        [string]$MeasurementSource,
        [string]$FailureKind = "",
        [string]$StartedAtUtc = ""
    )

    if ([string]::IsNullOrWhiteSpace($StartedAtUtc)) {
        $StartedAtUtc = [DateTime]::UtcNow.ToString("o")
    }

    $throughputMbps = $null
    if ($null -ne $Bytes -and $null -ne $DurationMs -and $Bytes -gt 0 -and $DurationMs -gt 0) {
        $throughputMbps = [Math]::Round((($Bytes * 8.0) / ($DurationMs / 1000.0)) / 1000000.0, 3)
    }

    return [pscustomobject]@{
        id = "$RunScenario-$RunRole-$RunIteration"
        scenario = $RunScenario
        iteration = $RunIteration
        role = $RunRole
        status = $Status
        started_at_utc = Redact-Text $StartedAtUtc
        latency_ms = if ($null -ne $LatencyMs) { [Math]::Round($LatencyMs, 3) } else { $null }
        duration_ms = if ($null -ne $DurationMs) { [Math]::Round($DurationMs, 3) } else { $null }
        bytes = if ($null -ne $Bytes) { $Bytes } else { $null }
        throughput_mbps = $throughputMbps
        measurement_source = $MeasurementSource
        failure_kind = Redact-Text $FailureKind
        payload_contents_recorded = $false
        raw_peer_ids_recorded = $false
        raw_paths_recorded = $false
    }
}

function New-DryRunObservations {
    $items = New-Object System.Collections.Generic.List[object]
    foreach ($item in $Scenario) {
        $baseLatency = Get-ScenarioBaseLatency -ScenarioName $item
        $bytes = Get-ScenarioBytes -ScenarioName $item
        for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
            $latency = $baseLatency + (($iteration - 1) * 7.0)
            $duration = $latency
            if ($item -eq "file-transfer") {
                $duration = $latency + 180.0
            }
            elseif ($item -eq "soak") {
                $duration = 60000.0 + (($iteration - 1) * 1000.0)
            }

            $items.Add((New-Observation -RunScenario $item -RunIteration $iteration -RunRole $Role -Status "passed" -LatencyMs $latency -DurationMs $duration -Bytes $bytes -MeasurementSource "dry-run"))
        }
    }

    return @($items.ToArray())
}

function ConvertTo-ObservationNumber {
    param(
        [object]$Value,
        [switch]$Integer
    )

    if ($null -eq $Value -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        return $null
    }

    if ($Integer) {
        return [int64]$Value
    }

    return [double]$Value
}

function Get-ObjectProperty {
    param(
        [object]$Object,
        [string]$Name
    )

    if ($null -eq $Object) {
        return $null
    }

    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }

    return $property.Value
}

function Read-ObservationFile {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "ObservationPath is required for Mode=$Mode."
    }
    $resolved = Resolve-Path -LiteralPath $Path
    $raw = Get-Content -LiteralPath $resolved.Path -Raw
    $data = $raw | ConvertFrom-Json
    $sourceItems = @()
    if ($data -is [array]) {
        $sourceItems = @($data)
    }
    elseif ($null -ne (Get-ObjectProperty -Object $data -Name "observations")) {
        $sourceItems = @(Get-ObjectProperty -Object $data -Name "observations")
    }
    else {
        $sourceItems = @($data)
    }

    $items = New-Object System.Collections.Generic.List[object]
    foreach ($source in $sourceItems) {
        $scenarioName = Redact-Text (Get-ObjectProperty -Object $source -Name "scenario")
        if ($Scenario -notcontains $scenarioName) {
            throw "Observation scenario '$scenarioName' is not in the requested scenario set."
        }

        $iteration = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "iteration") -Integer
        if ($null -eq $iteration) {
            $iteration = $items.Count + 1
        }

        $status = (Redact-Text (Get-ObjectProperty -Object $source -Name "status")).ToLowerInvariant()
        if ([string]::IsNullOrWhiteSpace($status)) {
            $status = "passed"
        }
        if ($status -eq "success" -or $status -eq "succeeded") {
            $status = "passed"
        }
        if ($status -ne "passed" -and $status -ne "failed" -and $status -ne "skipped") {
            $status = "failed"
        }

        $sourceRole = Redact-Text (Get-ObjectProperty -Object $source -Name "role")
        if ([string]::IsNullOrWhiteSpace($sourceRole)) {
            $sourceRole = $Role
        }

        $latencyMs = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "latency_ms")
        $durationMs = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "duration_ms")
        $bytes = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "bytes") -Integer
        $items.Add((New-Observation -RunScenario $scenarioName -RunIteration ([int]$iteration) -RunRole $sourceRole -Status $status -LatencyMs $latencyMs -DurationMs $durationMs -Bytes $bytes -MeasurementSource "observation-file" -FailureKind (Redact-Text (Get-ObjectProperty -Object $source -Name "failure_kind")) -StartedAtUtc (Redact-Text (Get-ObjectProperty -Object $source -Name "started_at_utc"))))
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
    $index = [int]$rank - 1
    return [Math]::Round([double]$sorted[$index], 3)
}

function New-ScenarioSummary {
    param(
        [string]$ScenarioName,
        [object[]]$Observations
    )

    $rows = @($Observations | Where-Object { $_.scenario -eq $ScenarioName })
    $passed = @($rows | Where-Object { $_.status -eq "passed" })
    $failed = @($rows | Where-Object { $_.status -eq "failed" })
    $skipped = @($rows | Where-Object { $_.status -eq "skipped" })
    $latencies = @($passed | Where-Object { $null -ne $_.latency_ms } | ForEach-Object { [double]$_.latency_ms })
    $durations = @($passed | Where-Object { $null -ne $_.duration_ms -and $_.duration_ms -gt 0 } | ForEach-Object { [double]$_.duration_ms })
    $bytesTotal = 0L
    foreach ($row in $passed) {
        if ($null -ne $row.bytes) {
            $bytesTotal += [int64]$row.bytes
        }
    }

    $durationTotalMs = 0.0
    foreach ($duration in $durations) {
        $durationTotalMs += [double]$duration
    }

    $throughputMbps = $null
    if ($bytesTotal -gt 0 -and $durationTotalMs -gt 0) {
        $throughputMbps = [Math]::Round((($bytesTotal * 8.0) / ($durationTotalMs / 1000.0)) / 1000000.0, 3)
    }

    return [pscustomobject]@{
        scenario = $ScenarioName
        iterations = $rows.Count
        success_count = $passed.Count
        failure_count = $failed.Count
        skipped_count = $skipped.Count
        latency_ms = [pscustomobject]@{
            p50 = Get-Percentile -Values $latencies -Percentile 50
            p95 = Get-Percentile -Values $latencies -Percentile 95
            max = if ($latencies.Count -gt 0) { [Math]::Round([double](@($latencies | Measure-Object -Maximum).Maximum), 3) } else { $null }
        }
        bytes_total = $bytesTotal
        throughput_mbps = $throughputMbps
    }
}

function New-EvidencePacket {
    param([object[]]$Observations)

    $summaries = New-Object System.Collections.Generic.List[object]
    foreach ($item in $Scenario) {
        $summaries.Add((New-ScenarioSummary -ScenarioName $item -Observations $Observations))
    }

    $failedCount = @($Observations | Where-Object { $_.status -eq "failed" }).Count
    $evidenceClass = if ($ReleaseEvidence) { "release-evidence-candidate" } else { "developer-diagnostics" }
    if ($Mode -eq "DryRun" -or $Mode -eq "Validate") {
        $evidenceClass = "developer-diagnostics"
    }

    $eligible = $ReleaseEvidence.IsPresent -and $Mode -ne "DryRun" -and $Mode -ne "Validate"
    $reason = "dry-run, validation, or unmarked capture is developer diagnostics only"
    if ($eligible) {
        $reason = "caller marked real two-machine evidence candidate"
    }

    return [pscustomobject]@{
        schema_version = "boundless.performance.two_machine.v1"
        generated_at_utc = [DateTime]::UtcNow.ToString("o")
        mode = $Mode
        evidence_class = $evidenceClass
        release_evidence = [pscustomobject]@{
            eligible = $eligible
            reason = $reason
        }
        command = "scripts/dev/perf-two-machine-evidence.ps1 -Mode $Mode -Role $Role"
        repo = [pscustomobject]@{
            branch = Get-GitValue -Arguments @("rev-parse", "--abbrev-ref", "HEAD")
            commit = Get-GitValue -Arguments @("rev-parse", "HEAD")
            dirty = -not [string]::IsNullOrWhiteSpace((Get-GitValue -Arguments @("status", "--porcelain")))
        }
        build = Get-BuildEvidence
        environment = Get-EnvironmentEvidence -RunRole $Role -RunHostLabel $HostLabel
        privacy = [pscustomobject]@{
            redaction_policy = "default"
            payload_contents_recorded = $false
            raw_peer_ids_recorded = $false
            raw_machine_ids_recorded = $false
            raw_paths_recorded = $false
            raw_ip_addresses_recorded = $false
        }
        scenarios_requested = $Scenario
        iteration_count = $Iterations
        observations = @($Observations)
        summary = [pscustomobject]@{
            scenario_summaries = @($summaries.ToArray())
            total_observations = $Observations.Count
            total_failures = $failedCount
        }
        artifacts = [pscustomobject]@{
            json = Get-RelativeArtifactPath -Path $jsonPath
            markdown = Get-RelativeArtifactPath -Path $markdownPath
        }
        notes = @(
            "This harness does not mutate pairing, firewall, service, trust, installer, clipboard contents, or file contents.",
            "DryRun and Validate output are developer diagnostics only, not release evidence.",
            "Use release evidence only for real two-machine runs with sanitized observations from both roles."
        )
    }
}

function Write-MarkdownSummary {
    param([object]$Packet)

    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("# Boundless Two-Machine Performance Evidence")
    $lines.Add("")
    $lines.Add("- Generated UTC: $($Packet.generated_at_utc)")
    $lines.Add("- Schema: $($Packet.schema_version)")
    $lines.Add("- Mode: $($Packet.mode)")
    $lines.Add("- Evidence class: $($Packet.evidence_class)")
    $lines.Add("- Role: $($Packet.environment.role)")
    $lines.Add("- Host label: $($Packet.environment.host_label)")
    $lines.Add("- Commit: $($Packet.repo.commit)")
    $lines.Add("- Payload contents recorded: $($Packet.privacy.payload_contents_recorded)")
    $lines.Add("- Raw peer IDs recorded: $($Packet.privacy.raw_peer_ids_recorded)")
    $lines.Add("- Raw paths recorded: $($Packet.privacy.raw_paths_recorded)")
    $lines.Add("")
    $lines.Add("| Scenario | Iterations | Success | Failed | Skipped | p50 ms | p95 ms | max ms | Bytes | Throughput Mbps |")
    $lines.Add("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    foreach ($summary in $Packet.summary.scenario_summaries) {
        $lines.Add("| $($summary.scenario) | $($summary.iterations) | $($summary.success_count) | $($summary.failure_count) | $($summary.skipped_count) | $($summary.latency_ms.p50) | $($summary.latency_ms.p95) | $($summary.latency_ms.max) | $($summary.bytes_total) | $($summary.throughput_mbps) |")
    }
    $lines.Add("")
    $lines.Add("## Artifact Paths")
    $lines.Add("")
    $lines.Add("- JSON: $($Packet.artifacts.json)")
    $lines.Add("- Markdown: $($Packet.artifacts.markdown)")
    $lines.Add("")
    $lines.Add("## Evidence Boundary")
    $lines.Add("")
    $lines.Add("Dry-run and fixture output is developer diagnostics only. Release evidence requires a real two-machine run, matching sanitized role packets, and review of missing or failed scenario rows.")
    $lines | Set-Content -LiteralPath $markdownPath -Encoding utf8
}

function Write-Packet {
    param([object]$Packet)

    $Packet | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $jsonPath -Encoding utf8
    Write-MarkdownSummary -Packet $Packet
    Write-Host "two_machine_evidence_json=$jsonPath"
    Write-Host "two_machine_evidence_markdown=$markdownPath"
}

function Invoke-Validation {
    $fixtureRows = @(
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 1; role = "coordinator"; status = "passed"; latency_ms = 10; duration_ms = 10; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 2; role = "coordinator"; status = "passed"; latency_ms = 20; duration_ms = 20; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 3; role = "coordinator"; status = "passed"; latency_ms = 30; duration_ms = 30; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 4; role = "coordinator"; status = "passed"; latency_ms = 40; duration_ms = 40; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 5; role = "coordinator"; status = "failed"; latency_ms = $null; duration_ms = $null; bytes = 0; failure_kind = "peer_id=raw-peer machine_id=raw-machine C:\Users\secret\payload.txt 192.168.1.22 12345678-1234-1234-1234-123456789abc" }
    )
    $fixturePath = Join-Path $OutputRoot "fixture-observations.json"
    $fixtureRows | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $fixturePath -Encoding utf8
    $observations = Read-ObservationFile -Path $fixturePath
    $packet = New-EvidencePacket -Observations $observations
    $summary = @($packet.summary.scenario_summaries | Where-Object { $_.scenario -eq "text-clipboard" })[0]
    if ($summary.latency_ms.p50 -ne 20) {
        throw "Expected fixture p50=20, found $($summary.latency_ms.p50)."
    }
    if ($summary.latency_ms.p95 -ne 40) {
        throw "Expected fixture p95=40, found $($summary.latency_ms.p95)."
    }
    if ($summary.latency_ms.max -ne 40) {
        throw "Expected fixture max=40, found $($summary.latency_ms.max)."
    }
    if ($summary.failure_count -ne 1) {
        throw "Expected fixture failure_count=1, found $($summary.failure_count)."
    }

    Write-Packet -Packet $packet
    $json = Get-Content -LiteralPath $jsonPath -Raw
    foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "12345678-1234-1234-1234-123456789abc")) {
        if ($json.Contains($forbidden)) {
            throw "Fixture output leaked forbidden token '$forbidden'."
        }
    }

    $dryRows = New-DryRunObservations
    $dryScenarios = @($dryRows | Select-Object -ExpandProperty scenario -Unique)
    foreach ($required in $Scenario) {
        if ($dryScenarios -notcontains $required) {
            throw "Dry-run observations missing scenario '$required'."
        }
    }

    Write-Host "fixture_validation=passed"
}

if ($ReleaseEvidence -and ($Mode -eq "DryRun" -or $Mode -eq "Validate")) {
    throw "DryRun and Validate modes cannot be marked as release evidence."
}

switch ($Mode) {
    "DryRun" {
        $observations = New-DryRunObservations
        $packet = New-EvidencePacket -Observations $observations
        Write-Packet -Packet $packet
    }
    "Capture" {
        $observations = @()
        if (-not [string]::IsNullOrWhiteSpace($ObservationPath)) {
            $observations = Read-ObservationFile -Path $ObservationPath
        }
        $packet = New-EvidencePacket -Observations $observations
        Write-Packet -Packet $packet
    }
    "Summarize" {
        $observations = Read-ObservationFile -Path $ObservationPath
        $packet = New-EvidencePacket -Observations $observations
        Write-Packet -Packet $packet
    }
    "Validate" {
        Invoke-Validation
    }
}
