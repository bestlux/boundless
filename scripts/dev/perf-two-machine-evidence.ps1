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
        [string]$ObservationId = "",
        [string]$FailureKind = "",
        [string]$StartedAtUtc = "",
        [string]$ScenarioVariant = "",
        [string]$Direction = "",
        [string]$PayloadKind = "",
        [string]$PayloadLabel = "",
        [Nullable[int64]]$PayloadBytes = $null,
        [Nullable[int64]]$PolicyLimitBytes = $null,
        [string]$PolicyExpected = "",
        [bool]$PayloadSynthetic = $false,
        [Nullable[double]]$SetupLatencyMs = $null,
        [string]$IntegrityHashStatus = "",
        [string]$ExpectedHashLabel = "",
        [string]$ReceivedHashLabel = "",
        [string]$PartialFileStatus = "",
        [string]$ReceivePathClass = "",
        [string]$CleanupStatus = "",
        [string]$FileCountClass = "",
        [Nullable[int]]$FileCount = $null,
        [Nullable[int]]$RetryCount = $null,
        [Nullable[int]]$ReconnectCount = $null,
        [string]$FailureSubsystem = "",
        [string]$InputCaptureState = "",
        [string]$ActivePeerClass = "",
        [string]$TransportEventSummary = "",
        [string]$SoakProfile = "",
        [Nullable[double]]$SoakDurationMinutes = $null,
        [bool]$ManualDisruptive = $false,
        [object[]]$ResourceTrendSamples = @(),
        [string]$ProvisionalClassification = "",
        [string]$ProvisionalClassificationReason = ""
    )

    if ([string]::IsNullOrWhiteSpace($StartedAtUtc)) {
        $StartedAtUtc = [DateTime]::UtcNow.ToString("o")
    }

    $throughputMbps = $null
    if ($null -ne $Bytes -and $null -ne $DurationMs -and $Bytes -gt 0 -and $DurationMs -gt 0) {
        $throughputMbps = [Math]::Round((($Bytes * 8.0) / ($DurationMs / 1000.0)) / 1000000.0, 3)
    }

    $effectivePayloadBytes = if ($null -ne $PayloadBytes) { $PayloadBytes } elseif ($null -ne $Bytes) { $Bytes } else { $null }
    $sanitizedObservationId = Redact-Text $ObservationId
    $sanitizedScenarioVariant = Redact-Text $ScenarioVariant
    $sanitizedDirection = Redact-Text $Direction
    $generatedIdParts = @(
        $RunScenario,
        $sanitizedScenarioVariant,
        $sanitizedDirection,
        $RunRole,
        [string]$RunIteration
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $effectiveObservationId = if (-not [string]::IsNullOrWhiteSpace($sanitizedObservationId)) {
        $sanitizedObservationId
    }
    else {
        $generatedIdParts -join "-"
    }

    return [pscustomobject]@{
        id = $effectiveObservationId
        scenario = $RunScenario
        scenario_variant = $sanitizedScenarioVariant
        direction = $sanitizedDirection
        iteration = $RunIteration
        role = $RunRole
        status = $Status
        started_at_utc = Redact-Text $StartedAtUtc
        latency_ms = if ($null -ne $LatencyMs) { [Math]::Round($LatencyMs, 3) } else { $null }
        duration_ms = if ($null -ne $DurationMs) { [Math]::Round($DurationMs, 3) } else { $null }
        bytes = if ($null -ne $Bytes) { $Bytes } else { $null }
        payload_kind = Redact-Text $PayloadKind
        payload_label = Redact-Text $PayloadLabel
        payload_bytes = if ($null -ne $effectivePayloadBytes) { $effectivePayloadBytes } else { $null }
        policy_limit_bytes = if ($null -ne $PolicyLimitBytes) { $PolicyLimitBytes } else { $null }
        policy_expected = Redact-Text $PolicyExpected
        payload_synthetic = $PayloadSynthetic
        setup_latency_ms = if ($null -ne $SetupLatencyMs) { [Math]::Round($SetupLatencyMs, 3) } else { $null }
        integrity_hash_status = Redact-Text $IntegrityHashStatus
        expected_hash_label = Redact-Text $ExpectedHashLabel
        received_hash_label = Redact-Text $ReceivedHashLabel
        partial_file_status = Redact-Text $PartialFileStatus
        receive_path_class = Redact-Text $ReceivePathClass
        cleanup_status = Redact-Text $CleanupStatus
        file_count_class = Redact-Text $FileCountClass
        file_count = if ($null -ne $FileCount) { [int]$FileCount } else { $null }
        retry_count = if ($null -ne $RetryCount) { [int]$RetryCount } else { $null }
        reconnect_count = if ($null -ne $ReconnectCount) { [int]$ReconnectCount } else { $null }
        failure_subsystem = Redact-Text $FailureSubsystem
        input_capture_state = Redact-Text $InputCaptureState
        active_peer_class = Redact-Text $ActivePeerClass
        transport_event_summary = Redact-Text $TransportEventSummary
        soak_profile = Redact-Text $SoakProfile
        soak_duration_minutes = if ($null -ne $SoakDurationMinutes) { [Math]::Round($SoakDurationMinutes, 3) } else { $null }
        manual_disruptive = $ManualDisruptive
        resource_trend_samples = @(ConvertTo-ResourceTrendSamples -Value $ResourceTrendSamples)
        throughput_mbps = $throughputMbps
        measurement_source = $MeasurementSource
        failure_kind = Redact-Text $FailureKind
        provisional_classification = Redact-Text $ProvisionalClassification
        provisional_classification_reason = Redact-Text $ProvisionalClassificationReason
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

function ConvertTo-ObservationBoolean {
    param([object]$Value)

    if ($null -eq $Value -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        return $false
    }

    if ($Value -is [bool]) {
        return [bool]$Value
    }

    return ([string]$Value).Equals("true", [System.StringComparison]::OrdinalIgnoreCase)
}

function ConvertTo-ResourceTrendSamples {
    param([object]$Value)

    $items = New-Object System.Collections.Generic.List[object]
    if ($null -eq $Value) {
        return @()
    }

    $sourceItems = @($Value)
    foreach ($sample in $sourceItems) {
        if ($items.Count -ge 60) {
            break
        }

        $sampleIndex = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $sample -Name "sample_index") -Integer
        $elapsedSeconds = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $sample -Name "elapsed_seconds")
        $cpuPercent = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $sample -Name "cpu_percent")
        $memoryMb = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $sample -Name "memory_mb")

        if ($null -eq $sampleIndex -and $null -eq $elapsedSeconds -and $null -eq $cpuPercent -and $null -eq $memoryMb) {
            continue
        }

        $items.Add([pscustomobject]@{
                sample_index = if ($null -ne $sampleIndex) { [int]$sampleIndex } else { $items.Count + 1 }
                elapsed_seconds = if ($null -ne $elapsedSeconds) { [Math]::Round([double]$elapsedSeconds, 3) } else { $null }
                cpu_percent = if ($null -ne $cpuPercent) { [Math]::Round([double]$cpuPercent, 3) } else { $null }
                memory_mb = if ($null -ne $memoryMb) { [Math]::Round([double]$memoryMb, 3) } else { $null }
            })
    }

    return @($items.ToArray())
}

function Normalize-ProvisionalClassification {
    param([object]$Value)

    $classification = (Redact-Text $Value).ToLowerInvariant()
    if ([string]::IsNullOrWhiteSpace($classification)) {
        return ""
    }

    if ($classification -eq "noop") {
        return "no-op"
    }

    if ($classification -in @("no-op", "acceptable", "warning", "fail")) {
        return $classification
    }

    return "warning"
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
        $payloadBytes = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "payload_bytes") -Integer
        $policyLimitBytes = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "policy_limit_bytes") -Integer
        $setupLatencyMs = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "setup_latency_ms")
        $fileCount = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "file_count") -Integer
        $retryCount = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "retry_count") -Integer
        $reconnectCount = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "reconnect_count") -Integer
        $soakDurationMinutes = ConvertTo-ObservationNumber -Value (Get-ObjectProperty -Object $source -Name "soak_duration_minutes")
        $resourceTrendSamples = @(ConvertTo-ResourceTrendSamples -Value (Get-ObjectProperty -Object $source -Name "resource_trend_samples"))
        $observationArgs = @{
            RunScenario = $scenarioName
            RunIteration = [int]$iteration
            RunRole = $sourceRole
            Status = $status
            LatencyMs = $latencyMs
            DurationMs = $durationMs
            Bytes = $bytes
            MeasurementSource = "observation-file"
            ObservationId = Redact-Text (Get-ObjectProperty -Object $source -Name "id")
            FailureKind = Redact-Text (Get-ObjectProperty -Object $source -Name "failure_kind")
            StartedAtUtc = Redact-Text (Get-ObjectProperty -Object $source -Name "started_at_utc")
            ScenarioVariant = Redact-Text (Get-ObjectProperty -Object $source -Name "scenario_variant")
            Direction = Redact-Text (Get-ObjectProperty -Object $source -Name "direction")
            PayloadKind = Redact-Text (Get-ObjectProperty -Object $source -Name "payload_kind")
            PayloadLabel = Redact-Text (Get-ObjectProperty -Object $source -Name "payload_label")
            PayloadBytes = $payloadBytes
            PolicyLimitBytes = $policyLimitBytes
            PolicyExpected = Redact-Text (Get-ObjectProperty -Object $source -Name "policy_expected")
            PayloadSynthetic = ConvertTo-ObservationBoolean (Get-ObjectProperty -Object $source -Name "payload_synthetic")
            SetupLatencyMs = $setupLatencyMs
            IntegrityHashStatus = Redact-Text (Get-ObjectProperty -Object $source -Name "integrity_hash_status")
            ExpectedHashLabel = Redact-Text (Get-ObjectProperty -Object $source -Name "expected_hash_label")
            ReceivedHashLabel = Redact-Text (Get-ObjectProperty -Object $source -Name "received_hash_label")
            PartialFileStatus = Redact-Text (Get-ObjectProperty -Object $source -Name "partial_file_status")
            ReceivePathClass = Redact-Text (Get-ObjectProperty -Object $source -Name "receive_path_class")
            CleanupStatus = Redact-Text (Get-ObjectProperty -Object $source -Name "cleanup_status")
            FileCountClass = Redact-Text (Get-ObjectProperty -Object $source -Name "file_count_class")
            FileCount = $fileCount
            RetryCount = $retryCount
            ReconnectCount = $reconnectCount
            FailureSubsystem = Redact-Text (Get-ObjectProperty -Object $source -Name "failure_subsystem")
            InputCaptureState = Redact-Text (Get-ObjectProperty -Object $source -Name "input_capture_state")
            ActivePeerClass = Redact-Text (Get-ObjectProperty -Object $source -Name "active_peer_class")
            TransportEventSummary = Redact-Text (Get-ObjectProperty -Object $source -Name "transport_event_summary")
            SoakProfile = Redact-Text (Get-ObjectProperty -Object $source -Name "soak_profile")
            SoakDurationMinutes = $soakDurationMinutes
            ManualDisruptive = ConvertTo-ObservationBoolean (Get-ObjectProperty -Object $source -Name "manual_disruptive")
            ResourceTrendSamples = $resourceTrendSamples
            ProvisionalClassification = Normalize-ProvisionalClassification (Get-ObjectProperty -Object $source -Name "provisional_classification")
            ProvisionalClassificationReason = Redact-Text (Get-ObjectProperty -Object $source -Name "provisional_classification_reason")
        }
        $items.Add((New-Observation @observationArgs))
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

function Get-ObservationValueCounts {
    param(
        [object[]]$Rows,
        [string]$PropertyName
    )

    $counts = [ordered]@{}
    foreach ($row in $Rows) {
        $property = $row.PSObject.Properties[$PropertyName]
        if ($null -eq $property) {
            continue
        }

        $value = [string]$property.Value
        if ([string]::IsNullOrWhiteSpace($value)) {
            continue
        }

        $key = $value.Replace("-", "_")
        if (-not $counts.Contains($key)) {
            $counts[$key] = 0
        }
        $counts[$key] += 1
    }

    return [pscustomobject]$counts
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
    $setupLatencies = @($passed | Where-Object { $null -ne $_.setup_latency_ms } | ForEach-Object { [double]$_.setup_latency_ms })
    $durations = @($passed | Where-Object { $null -ne $_.duration_ms -and $_.duration_ms -gt 0 } | ForEach-Object { [double]$_.duration_ms })
    $bytesTotal = 0L
    $retryCountTotal = 0
    $reconnectCountTotal = 0
    $resourceSampleCount = 0
    $cpuValues = New-Object System.Collections.Generic.List[double]
    $memoryValues = New-Object System.Collections.Generic.List[double]
    foreach ($row in $passed) {
        if ($null -ne $row.bytes) {
            $bytesTotal += [int64]$row.bytes
        }
        if ($null -ne $row.retry_count) {
            $retryCountTotal += [int]$row.retry_count
        }
        if ($null -ne $row.reconnect_count) {
            $reconnectCountTotal += [int]$row.reconnect_count
        }
    }
    foreach ($row in $rows) {
        foreach ($sample in @($row.resource_trend_samples)) {
            $resourceSampleCount += 1
            if ($null -ne $sample.cpu_percent) {
                $cpuValues.Add([double]$sample.cpu_percent)
            }
            if ($null -ne $sample.memory_mb) {
                $memoryValues.Add([double]$sample.memory_mb)
            }
        }
    }
    $payloadByteValues = @($rows | Where-Object { $null -ne $_.payload_bytes } | ForEach-Object { [int64]$_.payload_bytes })
    $classificationCounts = [ordered]@{
        no_op = @($rows | Where-Object { $_.provisional_classification -eq "no-op" }).Count
        acceptable = @($rows | Where-Object { $_.provisional_classification -eq "acceptable" }).Count
        warning = @($rows | Where-Object { $_.provisional_classification -eq "warning" }).Count
        fail = @($rows | Where-Object { $_.provisional_classification -eq "fail" }).Count
        unclassified = @($rows | Where-Object { [string]::IsNullOrWhiteSpace([string]$_.provisional_classification) }).Count
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
        setup_latency_ms = [pscustomobject]@{
            p50 = Get-Percentile -Values $setupLatencies -Percentile 50
            p95 = Get-Percentile -Values $setupLatencies -Percentile 95
            max = if ($setupLatencies.Count -gt 0) { [Math]::Round([double](@($setupLatencies | Measure-Object -Maximum).Maximum), 3) } else { $null }
        }
        bytes_total = $bytesTotal
        payload_bytes = [pscustomobject]@{
            min = if ($payloadByteValues.Count -gt 0) { [int64](@($payloadByteValues | Measure-Object -Minimum).Minimum) } else { $null }
            max = if ($payloadByteValues.Count -gt 0) { [int64](@($payloadByteValues | Measure-Object -Maximum).Maximum) } else { $null }
        }
        throughput_mbps = $throughputMbps
        retry_count_total = $retryCountTotal
        reconnect_count_total = $reconnectCountTotal
        manual_disruptive_count = @($rows | Where-Object { $_.manual_disruptive -eq $true }).Count
        soak_duration_minutes = [pscustomobject]@{
            p50 = Get-Percentile -Values @($passed | Where-Object { $null -ne $_.soak_duration_minutes } | ForEach-Object { [double]$_.soak_duration_minutes }) -Percentile 50
            p95 = Get-Percentile -Values @($passed | Where-Object { $null -ne $_.soak_duration_minutes } | ForEach-Object { [double]$_.soak_duration_minutes }) -Percentile 95
            max = if (@($passed | Where-Object { $null -ne $_.soak_duration_minutes }).Count -gt 0) { [Math]::Round([double](@($passed | Where-Object { $null -ne $_.soak_duration_minutes } | ForEach-Object { [double]$_.soak_duration_minutes } | Measure-Object -Maximum).Maximum), 3) } else { $null }
        }
        resource_trend = [pscustomobject]@{
            sample_count = $resourceSampleCount
            cpu_percent = [pscustomobject]@{
                p50 = Get-Percentile -Values @($cpuValues.ToArray()) -Percentile 50
                p95 = Get-Percentile -Values @($cpuValues.ToArray()) -Percentile 95
                max = if ($cpuValues.Count -gt 0) { [Math]::Round([double](@($cpuValues.ToArray()) | Measure-Object -Maximum).Maximum, 3) } else { $null }
            }
            memory_mb = [pscustomobject]@{
                p50 = Get-Percentile -Values @($memoryValues.ToArray()) -Percentile 50
                p95 = Get-Percentile -Values @($memoryValues.ToArray()) -Percentile 95
                max = if ($memoryValues.Count -gt 0) { [Math]::Round([double](@($memoryValues.ToArray()) | Measure-Object -Maximum).Maximum, 3) } else { $null }
            }
        }
        directions = @($rows | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_.direction) } | ForEach-Object { [string]$_.direction } | Sort-Object -Unique)
        scenario_variants = @($rows | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_.scenario_variant) } | ForEach-Object { [string]$_.scenario_variant } | Sort-Object -Unique)
        active_peer_classes = Get-ObservationValueCounts -Rows $rows -PropertyName "active_peer_class"
        input_capture_states = Get-ObservationValueCounts -Rows $rows -PropertyName "input_capture_state"
        failure_subsystems = Get-ObservationValueCounts -Rows $rows -PropertyName "failure_subsystem"
        soak_profiles = Get-ObservationValueCounts -Rows $rows -PropertyName "soak_profile"
        transport_event_summaries = Get-ObservationValueCounts -Rows $rows -PropertyName "transport_event_summary"
        file_count_classes = Get-ObservationValueCounts -Rows $rows -PropertyName "file_count_class"
        integrity_hash_statuses = Get-ObservationValueCounts -Rows $rows -PropertyName "integrity_hash_status"
        cleanup_statuses = Get-ObservationValueCounts -Rows $rows -PropertyName "cleanup_status"
        partial_file_statuses = Get-ObservationValueCounts -Rows $rows -PropertyName "partial_file_status"
        receive_path_classes = Get-ObservationValueCounts -Rows $rows -PropertyName "receive_path_class"
        provisional_classifications = [pscustomobject]$classificationCounts
    }
}

function Get-ReleaseEvidenceEligibility {
    param([object[]]$ScenarioSummaries)

    if (-not $ReleaseEvidence.IsPresent) {
        return [pscustomobject]@{
            eligible = $false
            reason = "ReleaseEvidence was not set"
        }
    }
    if ($Mode -eq "DryRun" -or $Mode -eq "Validate") {
        return [pscustomobject]@{
            eligible = $false
            reason = "dry-run and validation output is developer diagnostics only"
        }
    }

    $missing = @($ScenarioSummaries | Where-Object { $_.iterations -lt 1 })
    if ($missing.Count -gt 0) {
        $missingNames = ($missing | ForEach-Object { $_.scenario }) -join ", "
        return [pscustomobject]@{
            eligible = $false
            reason = "missing observations for selected scenario(s): $missingNames"
        }
    }

    return [pscustomobject]@{
        eligible = $true
        reason = "caller marked real two-machine evidence candidate with observations for each selected scenario"
    }
}

function New-EvidencePacket {
    param([object[]]$Observations)

    $summaries = New-Object System.Collections.Generic.List[object]
    foreach ($item in $Scenario) {
        $summaries.Add((New-ScenarioSummary -ScenarioName $item -Observations $Observations))
    }
    $summaryArray = @($summaries.ToArray())

    $failedCount = @($Observations | Where-Object { $_.status -eq "failed" }).Count
    $eligibility = Get-ReleaseEvidenceEligibility -ScenarioSummaries $summaryArray
    $evidenceClass = if ($eligibility.eligible) { "release-evidence-candidate" } else { "developer-diagnostics" }

    return [pscustomobject]@{
        schema_version = "boundless.performance.two_machine.v1"
        generated_at_utc = [DateTime]::UtcNow.ToString("o")
        mode = $Mode
        evidence_class = $evidenceClass
        release_evidence = [pscustomobject]@{
            eligible = $eligibility.eligible
            reason = $eligibility.reason
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
            scenario_summaries = $summaryArray
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
    $lines.Add("| Scenario | Iterations | Success | Failed | Skipped | p50 ms | p95 ms | max ms | Bytes | Payload min | Payload max | Acceptable | Warning | Fail | No-op | Throughput Mbps |")
    $lines.Add("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    foreach ($summary in $Packet.summary.scenario_summaries) {
        $lines.Add("| $($summary.scenario) | $($summary.iterations) | $($summary.success_count) | $($summary.failure_count) | $($summary.skipped_count) | $($summary.latency_ms.p50) | $($summary.latency_ms.p95) | $($summary.latency_ms.max) | $($summary.bytes_total) | $($summary.payload_bytes.min) | $($summary.payload_bytes.max) | $($summary.provisional_classifications.acceptable) | $($summary.provisional_classifications.warning) | $($summary.provisional_classifications.fail) | $($summary.provisional_classifications.no_op) | $($summary.throughput_mbps) |")
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
    Write-Host "two_machine_evidence_json=$($Packet.artifacts.json)"
    Write-Host "two_machine_evidence_markdown=$($Packet.artifacts.markdown)"
}

function Invoke-Validation {
    $fixtureRows = @(
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 1; role = "coordinator"; status = "passed"; latency_ms = 10; duration_ms = 10; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 2; role = "coordinator"; status = "passed"; latency_ms = 20; duration_ms = 20; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 3; role = "coordinator"; status = "passed"; latency_ms = 30; duration_ms = 30; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 4; role = "coordinator"; status = "passed"; latency_ms = 40; duration_ms = 40; bytes = 128 },
        [pscustomobject]@{ scenario = "text-clipboard"; iteration = 5; role = "coordinator"; status = "failed"; latency_ms = $null; duration_ms = $null; bytes = 0; failure_kind = "peer_id=raw-peer machine_id=raw-machine C:\Users\secret\payload.txt 192.168.1.22 12345678-1234-1234-1234-123456789abc" }
    )
    $fixturePath = New-TemporaryFile
    try {
        $fixtureRows | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $fixturePath.FullName -Encoding utf8
        $observations = Read-ObservationFile -Path $fixturePath.FullName
    }
    finally {
        Remove-Item -LiteralPath $fixturePath.FullName -Force -ErrorAction SilentlyContinue
    }
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
    if ($summary.throughput_mbps -ne 0.041) {
        throw "Expected fixture throughput_mbps=0.041, found $($summary.throughput_mbps)."
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
