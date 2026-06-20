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
    [string]$ReleaseManagerSignoff = "",
    [ValidateSet("stable", "prerelease")]
    [string]$Policy = "prerelease",
    [ValidateSet("msi-owned", "service-self-update", "tray-self-update")]
    [string]$ServiceUpdateMode = "msi-owned",
    [int]$MaxEvidenceAgeHours = 168
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
    $OutputRoot = Join-Path $repoRoot "artifacts/release-readiness/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
$logsRoot = Join-Path $OutputRoot "logs"
New-Item -ItemType Directory -Force -Path $logsRoot | Out-Null

$results = New-Object System.Collections.Generic.List[object]
$evidenceRoot = Join-Path $OutputRoot "evidence"
New-Item -ItemType Directory -Force -Path $evidenceRoot | Out-Null
$installerSmokeSummary = $null
$nMinusOneMsiCommand = "scripts/dev/installer-smoke.ps1 -InstallerPath <current-msi> -PreviousInstallerPath <prior-msi> -KeepArtifacts"

$packageManifestPath = Join-Path $repoRoot "packaging/windows/package-manifest.json"
$packageManifestVersion = ""
if (Test-Path -LiteralPath $packageManifestPath) {
    $packageManifestVersion = (Get-Content -LiteralPath $packageManifestPath -Raw | ConvertFrom-Json).version
}
$effectiveReleaseVersion = if ([string]::IsNullOrWhiteSpace($ReleaseVersion)) { $packageManifestVersion } else { $ReleaseVersion }

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
        Add-GateResult -Id $Id -Category $Category -Command $Command -Status "failed" -LogPath $logPath -ExitCode $exitCode -Reason "command exited non-zero" -Impact "release readiness is blocked until this gate passes"
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
    param(
        [string]$Path,
        [string]$ExpectedVersion
    )

    $resolvedSummaryItem = Resolve-Path -LiteralPath $Path
    $resolvedSummaryPath = $resolvedSummaryItem.Path
    $destinationPath = Join-Path $evidenceRoot "installer-smoke.json"

    if ($MaxEvidenceAgeHours -gt 0) {
        $age = [DateTime]::UtcNow - (Get-Item -LiteralPath $resolvedSummaryPath).LastWriteTimeUtc
        if ($age.TotalHours -gt $MaxEvidenceAgeHours) {
            $staleStatus = if ($Policy -eq "stable") { "failed" } else { "skipped" }
            $staleImpact = if ($Policy -eq "stable") {
                "stable release is blocked until fresh installer smoke evidence is supplied"
            }
            else {
                "prerelease packet must document why stale installer evidence is acceptable"
            }
            Add-GateResult -Id "installer_smoke_evidence_freshness" -Category "release" -Command "existing installer-smoke summary freshness" -Status $staleStatus -LogPath $resolvedSummaryPath -Reason "installer summary is older than $MaxEvidenceAgeHours hours" -Impact $staleImpact
        }
        else {
            Add-GateResult -Id "installer_smoke_evidence_freshness" -Category "release" -Command "existing installer-smoke summary freshness" -Status "passed" -LogPath $resolvedSummaryPath
        }
    }

    Copy-Item -LiteralPath $resolvedSummaryPath -Destination $destinationPath -Force

    $summary = Get-Content -LiteralPath $destinationPath -Raw | ConvertFrom-Json
    $requiredFields = @(
        "installer_path",
        "installer_signature",
        "tray_signature",
        "daemon_signature",
        "service_signature",
        "cli_signature",
        "service_version_output",
        "service_version_exit_code",
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
    Add-ServiceVersionGate -Summary $summary -ExpectedVersion $ExpectedVersion -LogPath $destinationPath
    return $summary
}

function Get-SummaryPropertyValue {
    param(
        [object]$Summary,
        [string]$Name
    )

    if ($null -eq $Summary) {
        return $null
    }
    $property = $Summary.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Normalize-ReleaseVersion {
    param([string]$Version)

    return $Version.Trim().TrimStart("v")
}

function Test-StableReleaseVersion {
    param([string]$Version)

    $normalized = Normalize-ReleaseVersion -Version $Version
    return -not [string]::IsNullOrWhiteSpace($normalized) -and -not $normalized.Contains("-")
}

function Parse-ServiceVersionOutput {
    param([string]$Output)

    $trimmed = $Output.Trim()
    if ([string]::IsNullOrWhiteSpace($trimmed)) {
        return [pscustomobject]@{
            ok = $false
            version = ""
            reason = "service version output was empty"
        }
    }

    if ($trimmed -notmatch '^boundless-service (?<version>\S+)$') {
        return [pscustomobject]@{
            ok = $false
            version = ""
            reason = "service version output must exactly match 'boundless-service <version>'"
        }
    }

    return [pscustomobject]@{
        ok = $true
        version = $Matches.version
        reason = ""
    }
}

function Add-ServiceVersionGate {
    param(
        [object]$Summary,
        [string]$ExpectedVersion,
        [string]$LogPath
    )

    $normalizedExpected = Normalize-ReleaseVersion -Version $ExpectedVersion
    $stableRelease = Test-StableReleaseVersion -Version $ExpectedVersion
    if ([string]::IsNullOrWhiteSpace($normalizedExpected)) {
        Add-GateResult -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Status "failed" -LogPath $LogPath -Reason "release version was empty" -Impact "service/runtime parity cannot be evaluated"
        return
    }

    if ($Summary.service_version_exit_code -ne 0) {
        Add-GateResult -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Status "failed" -LogPath $LogPath -Reason "service version command exited $($Summary.service_version_exit_code)" -Impact "service host build version evidence is incomplete"
        return
    }

    $serviceVersionOutput = [string]$Summary.service_version_output
    $parsedVersion = Parse-ServiceVersionOutput -Output $serviceVersionOutput
    if (-not $parsedVersion.ok) {
        Add-GateResult -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Status "failed" -LogPath $LogPath -Reason $parsedVersion.reason -Impact "service host build version evidence is incomplete"
        return
    }

    if ($parsedVersion.version -eq $normalizedExpected) {
        Add-GateResult -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Status "passed" -LogPath $LogPath
        return
    }

    $status = if ($stableRelease) { "failed" } else { "skipped" }
    $reason = "service version '$($parsedVersion.version)' did not exactly match expected version '$normalizedExpected'"
    $impact = if ($stableRelease) {
        "stable release is blocked until the installed service host version matches"
    }
    else {
        "prerelease service host version mismatch must be reviewed before promotion"
    }
    Add-GateResult -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Status $status -LogPath $LogPath -Reason $reason -Impact $impact
}

function Add-ServiceUpdateOwnershipGate {
    param([string]$Mode)

    if ($Mode -eq "msi-owned") {
        Add-GateResult -Id "service_update_ownership" -Category "release" -Command "release-readiness -ServiceUpdateMode msi-owned" -Status "passed" -Reason "MSI installer owns install, upgrade, repair, and uninstall of packaged tray, daemon, and service payloads"
        return
    }

    $owner = if ($Mode -eq "service-self-update") { "service self-update" } else { "tray self-update" }
    Add-GateResult -Id "service_update_ownership" -Category "release" -Command "release-readiness -ServiceUpdateMode $Mode" -Status "failed" -Reason "$owner is unsupported/deferred" -Impact "release readiness accepts MSI-owned update evidence only; service and tray self-update modes must not be treated as supported update evidence"
}

function Add-NMinusOneMsiUpgradeGate {
    param(
        [object]$Summary,
        [string]$Mode,
        [string]$Command
    )

    if ($Mode -ne "msi-owned") {
        Add-GateResult -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Status "failed" -Reason "N-1 upgrade validation requires MSI-owned update mode, not $Mode" -Impact "unsupported service/tray self-update evidence cannot satisfy MSI upgrade readiness"
        return
    }

    if ($null -eq $Summary) {
        Add-SkippedGate -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Reason "installer smoke summary was not provided" -Impact "provide current and prior MSI artifacts, then run $Command before stable release signoff"
        return
    }

    $upgradedFrom = [string](Get-SummaryPropertyValue -Summary $Summary -Name "upgraded_from")
    if ([string]::IsNullOrWhiteSpace($upgradedFrom)) {
        Add-SkippedGate -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Reason "installer smoke summary did not include a prior MSI in upgraded_from" -Impact "provide the previous release MSI with -PreviousInstallerPath; current supported source is a GitHub Release asset named Boundless-<version>-windows-x64.msi"
        return
    }

    $previousInstallExitCode = Get-SummaryPropertyValue -Summary $Summary -Name "previous_install_exit_code"
    $previousInstallExitCodeText = [string]$previousInstallExitCode
    if ($null -eq $previousInstallExitCode -or [string]::IsNullOrWhiteSpace($previousInstallExitCodeText)) {
        Add-GateResult -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Status "failed" -Reason "installer smoke summary included upgraded_from but missing or empty previous_install_exit_code" -Impact "N-1 MSI evidence is malformed and cannot prove the prior installer ran"
        return
    }

    $parsedPreviousInstallExitCode = 0
    if (-not [int]::TryParse($previousInstallExitCodeText, [ref]$parsedPreviousInstallExitCode)) {
        Add-GateResult -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Status "failed" -Reason "installer smoke summary previous_install_exit_code was not an integer: $previousInstallExitCodeText" -Impact "N-1 MSI evidence is malformed and cannot prove the prior installer ran"
        return
    }

    if ($parsedPreviousInstallExitCode -ne 0) {
        Add-GateResult -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Status "failed" -Reason "previous MSI install exited $parsedPreviousInstallExitCode" -Impact "N-1 MSI upgrade validation is blocked until the prior installer succeeds before current MSI upgrade"
        return
    }

    Add-GateResult -Id "n_minus_1_msi_upgrade" -Category "release" -Command $Command -Status "passed" -LogPath $upgradedFrom
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
    Add-SkippedGate -Id "pairing_recovery_matrix" -Category "runtime" -Command "scripts/dev/test-suite.ps1 -Profile recovery" -Reason "IncludeRuntimeGates was not set" -Impact "pairing recovery evidence remains release-candidate evidence"
}

if (-not [string]::IsNullOrWhiteSpace($InstallerSmokeSummaryPath)) {
    $installerSmokeSummary = Copy-AndValidateInstallerSmokeSummary -Path $InstallerSmokeSummaryPath -ExpectedVersion $effectiveReleaseVersion
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
        $generatedInstallerSmokeSummaryPath = Join-Path $OutputRoot "installer-smoke/installer-smoke.json"
        if (Test-Path -LiteralPath $generatedInstallerSmokeSummaryPath) {
            $installerSmokeSummary = Get-Content -LiteralPath $generatedInstallerSmokeSummaryPath -Raw | ConvertFrom-Json
            Add-ServiceVersionGate -Summary $installerSmokeSummary -ExpectedVersion $effectiveReleaseVersion -LogPath $generatedInstallerSmokeSummaryPath
        }
    }
}
else {
    Add-SkippedGate -Id "installer_smoke" -Category "release" -Command "scripts/dev/installer-smoke.ps1" -Reason "IncludeInstallerSmoke was not set and no InstallerSmokeSummaryPath was provided" -Impact "installer validation must be supplied before release signoff"
    Add-SkippedGate -Id "service_version_parity" -Category "release" -Command "installed boundless-service.exe --version" -Reason "installer smoke summary was not provided" -Impact "service/runtime version parity evidence must be supplied before stable release signoff"
}

Add-ServiceUpdateOwnershipGate -Mode $ServiceUpdateMode
Add-NMinusOneMsiUpgradeGate -Summary $installerSmokeSummary -Mode $ServiceUpdateMode -Command $nMinusOneMsiCommand

if ($IncludeServiceSmoke) {
    Invoke-Gate -Id "service_smoke" -Category "release" -Command "scripts/dev/service-smoke.ps1" -Action {
        & (Join-Path $repoRoot "scripts/dev/service-smoke.ps1") -OutputRoot (Join-Path $OutputRoot "service-smoke")
    }
}
else {
    Add-SkippedGate -Id "service_smoke" -Category "release" -Command "scripts/dev/service-smoke.ps1" -Reason "IncludeServiceSmoke was not set" -Impact "service install/start/status/stop/uninstall evidence must be supplied before service mode release signoff"
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
    release_version = $effectiveReleaseVersion
    release_policy = $Policy
    service_update_mode = $ServiceUpdateMode
    risk_classification = $risk
    release_manager_signoff = $ReleaseManagerSignoff
    environment = $environment
    parity_matrix = "docs/parity/mouse-without-borders.md"
    parity_release_blockers = $parityReleaseBlockers
    installer_smoke_summary = $installerSmokeSummary
    results = @($results.ToArray())
}

$jsonPath = Join-Path $OutputRoot "release-readiness.json"
$markdownPath = Join-Path $OutputRoot "release-readiness.md"
$packet | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding utf8

$markdown = New-Object System.Collections.Generic.List[string]
$markdown.Add("# Boundless Release Readiness Packet")
$markdown.Add("")
$markdown.Add("- Generated UTC: $($packet.generated_at_utc)")
$markdown.Add("- Git branch: $($packet.git_branch)")
$markdown.Add("- Git commit: $($packet.git_commit)")
$markdown.Add("- Release version: $($packet.release_version)")
$markdown.Add("- Release policy: $($packet.release_policy)")
$markdown.Add("- Service update mode: $($packet.service_update_mode)")
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

Write-Host "release_readiness=$risk"
Write-Host "release_readiness_json=$jsonPath"
Write-Host "release_readiness_markdown=$markdownPath"

$effectiveRequireReady = $RequireReady -or ($Policy -eq "stable")
if ($failed.Count -gt 0 -or ($effectiveRequireReady -and $skipped.Count -gt 0)) {
    exit 1
}
