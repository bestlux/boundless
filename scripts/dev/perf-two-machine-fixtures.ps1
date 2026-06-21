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
