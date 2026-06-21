[CmdletBinding()]
param(
    [string]$RepoRoot = "",
    [string]$OutputRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
}
else {
    (Resolve-Path -LiteralPath $RepoRoot).Path
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/performance/two-machine-evidence-fixtures/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

$harness = Join-Path $repoRoot "scripts/dev/perf-two-machine-evidence.ps1"
if (-not (Test-Path -LiteralPath $harness)) {
    throw "Missing harness script: $harness"
}
$clipboardLab = Join-Path $repoRoot "scripts/dev/perf-clipboard-lab.ps1"
if (-not (Test-Path -LiteralPath $clipboardLab)) {
    throw "Missing clipboard lab script: $clipboardLab"
}
$fileTransferLab = Join-Path $repoRoot "scripts/dev/perf-file-transfer-lab.ps1"
if (-not (Test-Path -LiteralPath $fileTransferLab)) {
    throw "Missing file-transfer lab script: $fileTransferLab"
}

function Redact-FixtureLogLine {
    param([object]$Value)

    $line = if ($null -eq $Value) { "" } else { [string]$Value }
    if ([string]::IsNullOrWhiteSpace($line)) {
        return ""
    }

    $line = $line.Replace($repoRoot, "[repo-root]")
    $line = $line.Replace($OutputRoot, "[output-root]")
    $line = [regex]::Replace($line, "(?i)\b[A-Z]:\\[^\s,;|]+", "[redacted-path]")
    $line = [regex]::Replace($line, "\\\\[^\s,;|]+", "[redacted-path]")
    return $line
}

function Invoke-Harness {
    param(
        [string]$Name,
        [string[]]$Arguments
    )

    $caseRoot = Join-Path $OutputRoot $Name
    New-Item -ItemType Directory -Force -Path $caseRoot | Out-Null
    $logPath = Join-Path $caseRoot "$Name.log"
    $captured = & powershell -NoProfile -ExecutionPolicy Bypass -File $harness @Arguments -OutputRoot $caseRoot *>&1
    $exitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
    $captured | ForEach-Object { Redact-FixtureLogLine $_ } | Set-Content -LiteralPath $logPath -Encoding utf8
    if ($exitCode -ne 0) {
        throw "Harness fixture '$Name' failed with exit code $exitCode; see generated fixture log"
    }

    $jsonPath = Join-Path $caseRoot "two-machine-evidence.json"
    $markdownPath = Join-Path $caseRoot "two-machine-evidence.md"
    if (-not (Test-Path -LiteralPath $jsonPath)) {
        throw "Harness fixture '$Name' did not produce two-machine-evidence.json."
    }
    if (-not (Test-Path -LiteralPath $markdownPath)) {
        throw "Harness fixture '$Name' did not produce two-machine-evidence.md."
    }

    return Get-Content -LiteralPath $jsonPath -Raw | ConvertFrom-Json
}

function Assert-PacketFieldSet {
    param(
        [object[]]$Observations,
        [string]$PropertyName,
        [string[]]$ExpectedValues,
        [string]$Label
    )

    $actual = @($Observations | ForEach-Object {
            $property = $_.PSObject.Properties[$PropertyName]
            if ($null -ne $property) {
                [string]$property.Value
            }
        } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
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

function Assert-PacketVariantRows {
    param(
        [object[]]$Observations,
        [string]$Variant,
        [int]$ExpectedCount,
        [string]$ExpectedPayloadKind,
        [string]$ExpectedStatus,
        [string]$ExpectedClassification,
        [string]$ExpectedPolicy
    )

    $rows = @($Observations | Where-Object { $_.scenario_variant -eq $Variant })
    if ($rows.Count -ne $ExpectedCount) {
        throw "$Variant expected $ExpectedCount observation rows, found $($rows.Count)."
    }

    foreach ($row in $rows) {
        if ($row.payload_kind -ne $ExpectedPayloadKind) {
            throw "$Variant did not preserve payload_kind=$ExpectedPayloadKind."
        }
        if ($row.status -ne $ExpectedStatus) {
            throw "$Variant did not preserve status=$ExpectedStatus."
        }
        if ($row.provisional_classification -ne $ExpectedClassification) {
            throw "$Variant did not preserve provisional_classification=$ExpectedClassification."
        }
        if ($row.policy_expected -ne $ExpectedPolicy) {
            throw "$Variant did not preserve policy_expected=$ExpectedPolicy."
        }
        if ([bool]$row.payload_synthetic -ne $true) {
            throw "$Variant did not preserve payload_synthetic=true."
        }
    }

    return $rows
}

$validatePacket = Invoke-Harness -Name "validate" -Arguments @("-Mode", "Validate", "-Role", "coordinator")
$validateSummary = @($validatePacket.summary.scenario_summaries | Where-Object { $_.scenario -eq "text-clipboard" })[0]
if ($validateSummary.latency_ms.p50 -ne 20 -or $validateSummary.latency_ms.p95 -ne 40 -or $validateSummary.latency_ms.max -ne 40) {
    throw "Validate fixture summary was not deterministic."
}
if ($validateSummary.failure_count -ne 1) {
    throw "Validate fixture did not preserve the expected failure count."
}
if ($validateSummary.throughput_mbps -ne 0.041) {
    throw "Validate fixture did not preserve expected throughput math."
}

$emptyCapturePacket = Invoke-Harness -Name "empty-capture-release-evidence" -Arguments @("-Mode", "Capture", "-Role", "coordinator", "-ReleaseEvidence")
if ($emptyCapturePacket.release_evidence.eligible -ne $false) {
    throw "Empty Capture fixture must not be eligible for release evidence."
}
if ($emptyCapturePacket.evidence_class -ne "developer-diagnostics") {
    throw "Empty Capture fixture must remain developer diagnostics."
}

foreach ($artifact in Get-ChildItem -LiteralPath $OutputRoot -File -Recurse -Include "*.json", "*.md", "*.log") {
    $content = Get-Content -LiteralPath $artifact.FullName -Raw
    foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "12345678-1234-1234-1234-123456789abc", $repoRoot, $OutputRoot)) {
        if (-not [string]::IsNullOrWhiteSpace($forbidden) -and $content.Contains($forbidden)) {
            throw "Fixture artifact '$($artifact.Name)' leaked forbidden token '$forbidden'."
        }
    }
}

$dryRunPacket = Invoke-Harness -Name "dry-run" -Arguments @("-Mode", "DryRun", "-Role", "coordinator", "-Iterations", "3")
if ($dryRunPacket.schema_version -ne "boundless.performance.two_machine.v1") {
    throw "Dry-run fixture produced unexpected schema version '$($dryRunPacket.schema_version)'."
}
if ($dryRunPacket.evidence_class -ne "developer-diagnostics") {
    throw "Dry-run fixture must remain developer diagnostics."
}

$dryScenarios = @($dryRunPacket.summary.scenario_summaries | ForEach-Object { $_.scenario })
foreach ($required in @("text-clipboard", "image-clipboard", "file-transfer", "reconnect-input", "soak")) {
    if ($dryScenarios -notcontains $required) {
        throw "Dry-run fixture missing scenario '$required'."
    }
}

$clipboardCaseRoot = Join-Path $OutputRoot "clipboard-lab"
New-Item -ItemType Directory -Force -Path $clipboardCaseRoot | Out-Null
$clipboardCaptured = & powershell -NoProfile -ExecutionPolicy Bypass -File $clipboardLab -Mode Validate -OutputRoot $clipboardCaseRoot *>&1
$clipboardExitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
$clipboardCaptured | ForEach-Object { Redact-FixtureLogLine $_ } | Set-Content -LiteralPath (Join-Path $clipboardCaseRoot "clipboard-lab.log") -Encoding utf8
if ($clipboardExitCode -ne 0) {
    throw "Clipboard lab fixture failed with exit code $clipboardExitCode; see generated fixture log"
}

$clipboardPacketPath = Join-Path $clipboardCaseRoot "two-machine-evidence.json"
if (-not (Test-Path -LiteralPath $clipboardPacketPath)) {
    throw "Clipboard lab fixture did not produce two-machine-evidence.json."
}
$clipboardPacket = Get-Content -LiteralPath $clipboardPacketPath -Raw | ConvertFrom-Json
$clipboardTextSummary = @($clipboardPacket.summary.scenario_summaries | Where-Object { $_.scenario -eq "text-clipboard" })[0]
$clipboardImageSummary = @($clipboardPacket.summary.scenario_summaries | Where-Object { $_.scenario -eq "image-clipboard" })[0]
if ($clipboardTextSummary.success_count -ne 60 -or $clipboardTextSummary.failure_count -ne 0) {
    throw "Clipboard lab text summary did not preserve expected success/failure counts."
}
if ($clipboardImageSummary.success_count -ne 60 -or $clipboardImageSummary.skipped_count -ne 20) {
    throw "Clipboard lab image summary did not preserve expected success/skipped counts."
}
if ($clipboardImageSummary.provisional_classifications.no_op -ne 20) {
    throw "Clipboard lab image summary did not classify the 4K policy-bound rows as no-op."
}
$clipboardObservations = @($clipboardPacket.observations)
$textVariants = @("text-small", "text-medium", "text-large-policy-limit")
$imageAcceptedVariants = @("image-screenshot-scale", "image-1080p", "image-near-limit")
$allVariants = @($textVariants + $imageAcceptedVariants + @("image-4k-policy-bound"))
Assert-PacketFieldSet -Observations $clipboardObservations -PropertyName "direction" -ExpectedValues @("A-to-B", "B-to-A") -Label "clipboard lab directions"
Assert-PacketFieldSet -Observations $clipboardObservations -PropertyName "scenario_variant" -ExpectedValues $allVariants -Label "clipboard lab scenario variants"
Assert-PacketFieldSet -Observations $clipboardObservations -PropertyName "payload_kind" -ExpectedValues @("text", "image-bmp") -Label "clipboard lab payload kinds"
foreach ($variant in $textVariants) {
    Assert-PacketVariantRows -Observations $clipboardObservations -Variant $variant -ExpectedCount 20 -ExpectedPayloadKind "text" -ExpectedStatus "passed" -ExpectedClassification "acceptable" -ExpectedPolicy "accepted" | Out-Null
}
foreach ($variant in $imageAcceptedVariants) {
    Assert-PacketVariantRows -Observations $clipboardObservations -Variant $variant -ExpectedCount 20 -ExpectedPayloadKind "image-bmp" -ExpectedStatus "passed" -ExpectedClassification "acceptable" -ExpectedPolicy "accepted" | Out-Null
}
$policyBoundRows = Assert-PacketVariantRows -Observations $clipboardObservations -Variant "image-4k-policy-bound" -ExpectedCount 20 -ExpectedPayloadKind "image-bmp" -ExpectedStatus "skipped" -ExpectedClassification "no-op" -ExpectedPolicy "rejected-by-current-policy"
foreach ($row in $policyBoundRows) {
    if ([int64]$row.bytes -ne 0L) {
        throw "image-4k-policy-bound rows must preserve bytes=0 for skipped policy-bound observations."
    }
    if ([int64]$row.payload_bytes -le [int64]$row.policy_limit_bytes) {
        throw "image-4k-policy-bound rows must preserve payload_bytes greater than policy_limit_bytes."
    }
}

$fileTransferCaseRoot = Join-Path $OutputRoot "file-transfer-lab"
New-Item -ItemType Directory -Force -Path $fileTransferCaseRoot | Out-Null
$fileTransferCaptured = & powershell -NoProfile -ExecutionPolicy Bypass -File $fileTransferLab -Mode Validate -OutputRoot $fileTransferCaseRoot *>&1
$fileTransferExitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
$fileTransferCaptured | ForEach-Object { Redact-FixtureLogLine $_ } | Set-Content -LiteralPath (Join-Path $fileTransferCaseRoot "file-transfer-lab.log") -Encoding utf8
if ($fileTransferExitCode -ne 0) {
    throw "File-transfer lab fixture failed with exit code $fileTransferExitCode; see generated fixture log"
}

$fileTransferPacketPath = Join-Path $fileTransferCaseRoot "two-machine-evidence.json"
if (-not (Test-Path -LiteralPath $fileTransferPacketPath)) {
    throw "File-transfer lab fixture did not produce two-machine-evidence.json."
}
$fileTransferPacket = Get-Content -LiteralPath $fileTransferPacketPath -Raw | ConvertFrom-Json
$fileTransferSummary = @($fileTransferPacket.summary.scenario_summaries | Where-Object { $_.scenario -eq "file-transfer" })[0]
if ($fileTransferSummary.success_count -ne 18 -or $fileTransferSummary.failure_count -ne 0 -or $fileTransferSummary.skipped_count -ne 6) {
    throw "File-transfer lab summary did not preserve expected success/failure/skipped counts."
}
if ($fileTransferSummary.provisional_classifications.no_op -ne 6) {
    throw "File-transfer lab did not classify disabled large-file rows as no-op."
}
if ($fileTransferSummary.throughput_mbps -le 0) {
    throw "File-transfer lab summary did not compute throughput."
}
if ($fileTransferSummary.setup_latency_ms.p50 -le 0 -or $fileTransferSummary.setup_latency_ms.p95 -le 0 -or $fileTransferSummary.setup_latency_ms.max -le 0) {
    throw "File-transfer lab summary did not compute setup latency percentiles."
}
$fileTransferObservations = @($fileTransferPacket.observations)
Assert-PacketFieldSet -Observations $fileTransferObservations -PropertyName "direction" -ExpectedValues @("A-to-B", "B-to-A") -Label "file-transfer lab directions"
Assert-PacketFieldSet -Observations $fileTransferObservations -PropertyName "scenario_variant" -ExpectedValues @("single-small-file", "many-small-files", "medium-100mb", "large-1gb-opt-in") -Label "file-transfer lab scenario variants"
Assert-PacketFieldSet -Observations $fileTransferObservations -PropertyName "file_count_class" -ExpectedValues @("single-file", "many-small-files", "medium-file", "large-file") -Label "file-transfer lab file-count classes"
Assert-PacketFieldSet -Observations $fileTransferObservations -PropertyName "integrity_hash_status" -ExpectedValues @("synthetic-match", "not-checked") -Label "file-transfer lab hash statuses"
Assert-PacketFieldSet -Observations $fileTransferObservations -PropertyName "cleanup_status" -ExpectedValues @("clean", "not-created") -Label "file-transfer lab cleanup statuses"
$largeRows = @($fileTransferObservations | Where-Object { $_.scenario_variant -eq "large-1gb-opt-in" })
foreach ($row in $largeRows) {
    if ($row.status -ne "skipped" -or $row.provisional_classification -ne "no-op" -or [int64]$row.bytes -ne 0L) {
        throw "File-transfer large-file rows must stay skipped/no-op by default."
    }
    if ([int64]$row.payload_bytes -ne (1024L * 1024L * 1024L)) {
        throw "File-transfer large-file metadata must preserve a 1 GiB payload size."
    }
}

foreach ($artifact in Get-ChildItem -LiteralPath $OutputRoot -File -Recurse -Include "*.json", "*.md", "*.log") {
    $content = Get-Content -LiteralPath $artifact.FullName -Raw
    foreach ($forbidden in @("raw-peer", "raw-machine", "C:\Users\secret", "192.168.1.22", "12345678-1234-1234-1234-123456789abc", $repoRoot, $OutputRoot)) {
        if (-not [string]::IsNullOrWhiteSpace($forbidden) -and $content.Contains($forbidden)) {
            throw "Fixture artifact '$($artifact.Name)' leaked forbidden token '$forbidden'."
        }
    }
}

Write-Host "two_machine_perf_fixtures=passed"
Write-Host "output_root=[redacted]"
