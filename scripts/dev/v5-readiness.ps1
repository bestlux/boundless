param(
    [string]$RepoRoot = "",
    [string]$OutputRoot = "",
    [switch]$SkipUnitGates,
    [switch]$IncludeRuntimeGates,
    [switch]$IncludeInstallerSmoke,
    [string]$InstallerPath = "",
    [string]$PreviousInstallerPath = "",
    [string]$InstallerSmokeSummaryPath = "",
    [switch]$RequireSignature,
    [switch]$RequireReady,
    [switch]$IncludeServiceSmoke,
    [int]$RuntimeTimeoutSeconds = 90,
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "",
    [string]$ReleaseVersion = "",
    [string]$ReleaseManagerSignoff = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
}
else {
    (Resolve-Path -LiteralPath $RepoRoot).Path
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/v5-readiness/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
$logsRoot = Join-Path $OutputRoot "logs"
New-Item -ItemType Directory -Force -Path $logsRoot | Out-Null

$results = New-Object System.Collections.Generic.List[object]
$evidenceRoot = Join-Path $OutputRoot "evidence"
New-Item -ItemType Directory -Force -Path $evidenceRoot | Out-Null
$installerSmokeSummary = $null

function New-LogPath {
    param([string]$Id)
    return Join-Path $logsRoot "$Id.log"
}

function Add-GateResult {
    param(
        [string]$Id,
        [string]$Category,
        [string]$Command,
        [string]$Status,
        [string]$LogPath = "",
        [Nullable[int]]$ExitCode = $null,
        [string]$Reason = "",
        [string]$Impact = ""
    )

    $results.Add([pscustomobject]@{
        id = $Id
        category = $Category
        command = $Command
        status = $Status
        exit_code = $ExitCode
        log_path = $LogPath
        reason = $Reason
        impact = $Impact
    })
}

function Invoke-Gate {
    param(
        [string]$Id,
        [string]$Category,
        [string]$Command,
        [scriptblock]$Action
    )

    $logPath = New-LogPath -Id $Id
    $exitCode = 0
    $captured = @()
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        Push-Location $repoRoot
        $global:LASTEXITCODE = 0
        $ErrorActionPreference = "Continue"
        $captured = & $Action *>&1
        $exitCode = $global:LASTEXITCODE
        if ($null -eq $exitCode) {
            $exitCode = 0
        }
    }
    catch {
        $captured += $_ | Out-String
        $exitCode = 1
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
        Pop-Location
    }

    $captured | ForEach-Object { $_.ToString() } | Set-Content -LiteralPath $logPath -Encoding utf8
    if ($exitCode -eq 0) {
        Add-GateResult -Id $Id -Category $Category -Command $Command -Status "passed" -LogPath $logPath -ExitCode $exitCode
    }
    else {
        Add-GateResult -Id $Id -Category $Category -Command $Command -Status "failed" -LogPath $logPath -ExitCode $exitCode -Reason "command exited non-zero" -Impact "v5 readiness is blocked until this gate passes"
    }
}

function Add-SkippedGate {
    param(
        [string]$Id,
        [string]$Category,
        [string]$Command,
        [string]$Reason,
        [string]$Impact
    )

    Add-GateResult -Id $Id -Category $Category -Command $Command -Status "skipped" -Reason $Reason -Impact $Impact
}

function Copy-AndValidateInstallerSmokeSummary {
    param([string]$Path)

    $resolvedSummaryPath = (Resolve-Path -LiteralPath $Path).Path
    $destinationPath = Join-Path $evidenceRoot "installer-smoke.json"
    Copy-Item -LiteralPath $resolvedSummaryPath -Destination $destinationPath -Force

    $summary = Get-Content -LiteralPath $destinationPath -Raw | ConvertFrom-Json
    $requiredFields = @(
        "installer_path",
        "installer_signature",
        "tray_signature",
        "daemon_signature",
        "service_signature",
        "cli_signature",
        "status"
    )
    foreach ($field in $requiredFields) {
        if (-not ($summary.PSObject.Properties.Name -contains $field)) {
            Add-GateResult -Id "installer_smoke" -Category "release" -Command "existing installer-smoke summary" -Status "failed" -LogPath $destinationPath -Reason "installer summary missing '$field'" -Impact "installer evidence is incomplete"
            return $null
        }
    }

    if ($summary.status -ne "passed") {
        Add-GateResult -Id "installer_smoke" -Category "release" -Command "existing installer-smoke summary" -Status "failed" -LogPath $destinationPath -Reason "installer summary status was '$($summary.status)'" -Impact "installer release gate did not pass"
        return $summary
    }

    Add-GateResult -Id "installer_smoke" -Category "release" -Command "existing installer-smoke summary" -Status "passed" -LogPath $destinationPath
    return $summary
}

function Get-ParityReleaseBlockers {
    param([string]$RepoRoot)

    $matrixPath = Join-Path $RepoRoot "docs/parity/mouse-without-borders.md"
    if (-not (Test-Path -LiteralPath $matrixPath)) {
        return @()
    }

    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($line in Get-Content -LiteralPath $matrixPath) {
        if (-not $line.StartsWith("| ")) {
            continue
        }
        if ($line -match '^\|\s*-') {
            continue
        }
        if ($line -match '^\|\s*Capability\s*\|') {
            continue
        }

        $columns = @($line.Trim('|') -split '\|' | ForEach-Object { $_.Trim() })
        if ($columns.Count -lt 9) {
            continue
        }
        if ($columns[8] -ne "yes") {
            continue
        }

        $rows.Add([pscustomobject]@{
            capability = $columns[0]
            status = $columns[3]
            evidence_or_gap = $columns[7]
            release_blocker = $columns[8]
        })
    }

    return @($rows.ToArray())
}

Invoke-Gate -Id "powershell_syntax" -Category "unit" -Command "parse scripts/**/*.ps1" -Action {
    $scripts = Get-ChildItem -LiteralPath (Join-Path $repoRoot "scripts") -Recurse -Filter *.ps1 -File
    foreach ($script in $scripts) {
        $errors = $null
        [System.Management.Automation.Language.Parser]::ParseFile($script.FullName, [ref]$null, [ref]$errors) | Out-Null
        if ($errors) {
            throw "PowerShell parse errors in $($script.FullName): $($errors | Out-String)"
        }
    }
    "parsed $($scripts.Count) PowerShell scripts"
}

Invoke-Gate -Id "release_consistency" -Category "release" -Command "scripts/release/assert-release-consistency.ps1" -Action {
    & (Join-Path $repoRoot "scripts/release/assert-release-consistency.ps1")
}

if ($SkipUnitGates) {
    Add-SkippedGate -Id "cargo_fmt" -Category "unit" -Command "cargo fmt --all -- --check" -Reason "SkipUnitGates was set" -Impact "format regressions must be covered by a separate CI or local gate"
    Add-SkippedGate -Id "cargo_clippy" -Category "unit" -Command "cargo clippy --workspace --all-targets -- -D warnings" -Reason "SkipUnitGates was set" -Impact "lint regressions must be covered by a separate CI or local gate"
    Add-SkippedGate -Id "cargo_test" -Category "unit" -Command "cargo test --workspace" -Reason "SkipUnitGates was set" -Impact "unit regressions must be covered by a separate CI or local gate"
}
else {
    Invoke-Gate -Id "cargo_fmt" -Category "unit" -Command "cargo fmt --all -- --check" -Action {
        cargo fmt --all -- --check
    }
    Invoke-Gate -Id "cargo_clippy" -Category "unit" -Command "cargo clippy --workspace --all-targets -- -D warnings" -Action {
        cargo clippy --workspace --all-targets -- -D warnings
    }
    Invoke-Gate -Id "cargo_test" -Category "unit" -Command "cargo test --workspace" -Action {
        cargo test --workspace
    }
}

if ($IncludeRuntimeGates) {
    Invoke-Gate -Id "two_node_smoke" -Category "runtime" -Command "scripts/dev/two-node-smoke.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/two-node-smoke.ps1") -TimeoutSeconds $RuntimeTimeoutSeconds -KeepArtifacts
    }
    Invoke-Gate -Id "three_node_smoke" -Category "runtime" -Command "scripts/dev/three-node-smoke.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/three-node-smoke.ps1") -TimeoutSeconds ([Math]::Max($RuntimeTimeoutSeconds, 90)) -KeepArtifacts
    }
    Invoke-Gate -Id "four_node_topology" -Category "runtime" -Command "cargo test -p app-services layout_matrix_validation_accepts_four_remote_peers_plus_local" -Action {
        cargo test -p app-services layout_matrix_validation_accepts_four_remote_peers_plus_local
    }
    Invoke-Gate -Id "edge_handoff_trace" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile trace" -Action {
        & (Join-Path $repoRoot "scripts/dev/test-suite.ps1") -Profile trace -TimeoutSeconds $RuntimeTimeoutSeconds -EndpointA $EndpointA -EndpointB $EndpointB -KeepArtifacts
    }
    Invoke-Gate -Id "clipboard_file_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile clipboard" -Action {
        & (Join-Path $repoRoot "scripts/dev/test-suite.ps1") -Profile clipboard -TimeoutSeconds $RuntimeTimeoutSeconds -KeepArtifacts
    }

    if ([string]::IsNullOrWhiteSpace($EndpointB)) {
        Add-SkippedGate -Id "pairing_recovery_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile recovery" -Reason "EndpointB was not provided" -Impact "pairing recovery must be validated before release signoff"
    }
    else {
        Invoke-Gate -Id "pairing_recovery_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile recovery" -Action {
            & (Join-Path $repoRoot "scripts/dev/test-suite.ps1") -Profile recovery -TimeoutSeconds $RuntimeTimeoutSeconds -EndpointA $EndpointA -EndpointB $EndpointB -KeepArtifacts
        }
    }
}
else {
    Add-SkippedGate -Id "two_node_smoke" -Category "runtime" -Command "scripts/dev/two-node-smoke.ps1" -Reason "IncludeRuntimeGates was not set" -Impact "two-node runtime behavior remains release-candidate evidence"
    Add-SkippedGate -Id "three_node_smoke" -Category "runtime" -Command "scripts/dev/three-node-smoke.ps1" -Reason "IncludeRuntimeGates was not set" -Impact "three-node runtime behavior remains release-candidate evidence"
    Add-SkippedGate -Id "four_node_topology" -Category "runtime" -Command "cargo test -p app-services layout_matrix_validation_accepts_four_remote_peers_plus_local" -Reason "IncludeRuntimeGates was not set" -Impact "four-machine deterministic topology evidence remains release-candidate evidence"
    Add-SkippedGate -Id "edge_handoff_trace" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile trace" -Reason "IncludeRuntimeGates was not set" -Impact "input latency budgets remain release-candidate evidence"
    Add-SkippedGate -Id "clipboard_file_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile clipboard" -Reason "IncludeRuntimeGates was not set" -Impact "clipboard/file workflow evidence remains release-candidate evidence"
    Add-SkippedGate -Id "pairing_recovery_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile recovery" -Reason "IncludeRuntimeGates was not set" -Impact "pairing recovery evidence remains release-candidate evidence"
}

if (-not [string]::IsNullOrWhiteSpace($InstallerSmokeSummaryPath)) {
    $installerSmokeSummary = Copy-AndValidateInstallerSmokeSummary -Path $InstallerSmokeSummaryPath
}
elseif ($IncludeInstallerSmoke) {
    if ([string]::IsNullOrWhiteSpace($InstallerPath)) {
        Add-GateResult -Id "installer_smoke" -Category "release" -Command "scripts/dev/installer-smoke.ps1" -Status "failed" -Reason "IncludeInstallerSmoke was set without InstallerPath" -Impact "installer release gate cannot run"
    }
    else {
        Invoke-Gate -Id "installer_smoke" -Category "release" -Command "scripts/dev/installer-smoke.ps1" -Action {
            $params = @{
                InstallerPath = $InstallerPath
                OutputRoot = (Join-Path $OutputRoot "installer-smoke")
                RequireSignature = $RequireSignature.IsPresent
                KeepArtifacts = $true
            }
            if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
                $params.PreviousInstallerPath = $PreviousInstallerPath
            }
            & (Join-Path $repoRoot "scripts/dev/installer-smoke.ps1") @params
        }
    }
}
else {
    Add-SkippedGate -Id "installer_smoke" -Category "release" -Command "scripts/dev/installer-smoke.ps1" -Reason "IncludeInstallerSmoke was not set and no InstallerSmokeSummaryPath was provided" -Impact "installer validation must be supplied before release signoff"
}

if ($IncludeServiceSmoke) {
    Add-SkippedGate -Id "service_smoke" -Category "release" -Command "future service smoke" -Reason "dedicated service smoke is not implemented" -Impact "service mode remains explicitly deferred for elevated-app and lock-screen claims"
}
else {
    Add-SkippedGate -Id "service_smoke" -Category "release" -Command "future service smoke" -Reason "IncludeServiceSmoke was not set" -Impact "service mode remains explicitly deferred for elevated-app and lock-screen claims"
}

$failed = @($results | Where-Object { $_.status -eq "failed" })
$skipped = @($results | Where-Object { $_.status -eq "skipped" })
$risk = if ($failed.Count -gt 0) {
    "blocked"
}
elseif ($skipped.Count -gt 0) {
    "at-risk"
}
else {
    "ready"
}

$gitBranch = ""
$gitCommit = ""
try {
    $gitBranch = (& git -C $repoRoot rev-parse --abbrev-ref HEAD 2>&1 | Out-String).Trim()
    $gitCommit = (& git -C $repoRoot rev-parse HEAD 2>&1 | Out-String).Trim()
}
catch {
}

$packageManifestPath = Join-Path $repoRoot "packaging/windows/package-manifest.json"
$packageManifestVersion = ""
if (Test-Path -LiteralPath $packageManifestPath) {
    $packageManifestVersion = (Get-Content -LiteralPath $packageManifestPath -Raw | ConvertFrom-Json).version
}

$parityReleaseBlockers = Get-ParityReleaseBlockers -RepoRoot $repoRoot
$environment = [pscustomobject]@{
    os = [System.Environment]::OSVersion.VersionString
    machine_name = [System.Environment]::MachineName
    user_domain = [System.Environment]::UserDomainName
    powershell = $PSVersionTable.PSVersion.ToString()
}

$packet = [pscustomobject]@{
    generated_at_utc = [DateTime]::UtcNow.ToString("o")
    repo_root = $repoRoot
    git_branch = $gitBranch
    git_commit = $gitCommit
    release_version = if ([string]::IsNullOrWhiteSpace($ReleaseVersion)) { $packageManifestVersion } else { $ReleaseVersion }
    risk_classification = $risk
    release_manager_signoff = $ReleaseManagerSignoff
    environment = $environment
    parity_matrix = "docs/parity/mouse-without-borders.md"
    parity_release_blockers = $parityReleaseBlockers
    installer_smoke_summary = $installerSmokeSummary
    results = @($results.ToArray())
}

$jsonPath = Join-Path $OutputRoot "v5-readiness.json"
$markdownPath = Join-Path $OutputRoot "v5-readiness.md"
$packet | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding utf8

$markdown = New-Object System.Collections.Generic.List[string]
$markdown.Add("# Boundless V5 Readiness Packet")
$markdown.Add("")
$markdown.Add("- Generated UTC: $($packet.generated_at_utc)")
$markdown.Add("- Git branch: $($packet.git_branch)")
$markdown.Add("- Git commit: $($packet.git_commit)")
$markdown.Add("- Release version: $($packet.release_version)")
$markdown.Add("- Risk classification: $risk")
$markdown.Add("- Release manager signoff: $($packet.release_manager_signoff)")
$markdown.Add("- Parity matrix: ``docs/parity/mouse-without-borders.md``")
$markdown.Add("- Release-blocking parity rows: $($parityReleaseBlockers.Count)")
$markdown.Add("")
$markdown.Add("| Gate | Category | Status | Command | Evidence | Reason | Impact |")
$markdown.Add("| --- | --- | --- | --- | --- | --- | --- |")
foreach ($result in $results) {
    $evidence = if ([string]::IsNullOrWhiteSpace($result.log_path)) { "" } else { $result.log_path }
    $markdown.Add("| $($result.id) | $($result.category) | $($result.status) | ``$($result.command)`` | ``$evidence`` | $($result.reason) | $($result.impact) |")
}
$markdown.Add("")
$markdown.Add("## Release-Blocking Parity Rows")
$markdown.Add("")
$markdown.Add("| Capability | Status | Evidence Or Gap |")
$markdown.Add("| --- | --- | --- |")
foreach ($row in $parityReleaseBlockers) {
    $markdown.Add("| $($row.capability) | $($row.status) | $($row.evidence_or_gap) |")
}
$markdown | Set-Content -LiteralPath $markdownPath -Encoding utf8

Write-Host "v5_readiness=$risk"
Write-Host "v5_readiness_json=$jsonPath"
Write-Host "v5_readiness_markdown=$markdownPath"

if ($failed.Count -gt 0 -or ($RequireReady -and $skipped.Count -gt 0)) {
    exit 1
}
