[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("workspace", "smoke", "installer", "service", "release", "docs/status")]
    [string]$Area,

    [ValidateSet("text", "json")]
    [string]$Format = "text",

    [string]$OutputRoot = "",
    [int]$TimeoutSeconds = 60,
    [switch]$KeepArtifacts,
    [string]$InstallerPath = "",
    [string]$PreviousInstallerPath = "",
    [string]$InstallerSmokeSummaryPath = "",
    [ValidateSet("stable", "prerelease")]
    [string]$ReleasePolicy = "stable",
    [switch]$IncludeRuntimeGates,
    [switch]$IncludeServiceSmoke,
    [switch]$RequireSignature,
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$runningOnWindows = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputRoot = Join-Path $repoRoot "artifacts/check/$Area/$stamp"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
$logsRoot = Join-Path $OutputRoot "logs"
New-Item -ItemType Directory -Force -Path $logsRoot | Out-Null

function Get-GitValue {
    param([string[]]$Arguments)

    try {
        return (& git -C $repoRoot @Arguments 2>$null | Out-String).Trim()
    }
    catch {
        return ""
    }
}

function New-CommandResult {
    param(
        [string]$Id,
        [string]$Command,
        [string]$Status,
        [Nullable[int]]$ExitCode = $null,
        [string]$LogPath = "",
        [string]$Reason = ""
    )

    return [pscustomobject]@{
        id = $Id
        command = $Command
        status = $Status
        exit_code = $ExitCode
        log_path = $LogPath
        reason = $Reason
    }
}

function Invoke-ScriptCommand {
    param(
        [string]$Id,
        [string]$ScriptPath,
        [string[]]$Arguments = @()
    )

    $logPath = Join-Path $logsRoot "$Id.log"
    $displayCommand = "$ScriptPath $($Arguments -join ' ')".Trim()
    $captured = @()
    $exitCode = 0
    $previousErrorActionPreference = $ErrorActionPreference
    try {
        Push-Location $repoRoot
        $global:LASTEXITCODE = 0
        $ErrorActionPreference = "Continue"
        $captured = & powershell -NoProfile -ExecutionPolicy Bypass -File $ScriptPath @Arguments *>&1
        $exitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
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
        return New-CommandResult -Id $Id -Command $displayCommand -Status "passed" -ExitCode $exitCode -LogPath $logPath
    }

    return New-CommandResult -Id $Id -Command $displayCommand -Status "failed" -ExitCode $exitCode -LogPath $logPath -Reason "command exited non-zero"
}

function New-SkippedCommand {
    param(
        [string]$Id,
        [string]$Command,
        [string]$Reason
    )

    return New-CommandResult -Id $Id -Command $Command -Status "skipped" -Reason $Reason
}

function Test-DocContains {
    param(
        [string]$Path,
        [string[]]$RequiredPatterns
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return @("missing file: $Path")
    }

    $content = Get-Content -LiteralPath $Path -Raw
    $missing = New-Object System.Collections.Generic.List[string]
    foreach ($pattern in $RequiredPatterns) {
        if ($content -notmatch $pattern) {
            $missing.Add("missing pattern '$pattern' in $Path")
        }
    }
    return @($missing.ToArray())
}

function Invoke-DocsStatusCheck {
    $projectStatusPath = Join-Path $repoRoot "docs/project-status.md"
    $componentMapPath = Join-Path $repoRoot "docs/architecture/component-map.md"
    $missing = New-Object System.Collections.Generic.List[string]

    $projectRequired = @(
        "Release Baseline",
        "Support Posture",
        "Service Mode Boundary",
        "Canonical Release Flow",
        "Known Validation Gaps",
        "Current Work"
    )
    foreach ($item in Test-DocContains -Path $projectStatusPath -RequiredPatterns $projectRequired) {
        $missing.Add($item)
    }

    $componentRequired = @(
        "component_id",
        "Owner",
        "Durable State",
        "Ephemeral State",
        "Locks, Queues, Tasks",
        "IPC Surface",
        "Sensitive Data",
        "Required Tests"
    )
    foreach ($item in Test-DocContains -Path $componentMapPath -RequiredPatterns $componentRequired) {
        $missing.Add($item)
    }

    $logPath = Join-Path $logsRoot "docs-status.log"
    if ($missing.Count -eq 0) {
        "docs/status passed" | Set-Content -LiteralPath $logPath -Encoding utf8
        return New-CommandResult -Id "docs_status" -Command "validate docs/project-status.md and docs/architecture/component-map.md" -Status "passed" -ExitCode 0 -LogPath $logPath
    }

    $missing | Set-Content -LiteralPath $logPath -Encoding utf8
    return New-CommandResult -Id "docs_status" -Command "validate docs/project-status.md and docs/architecture/component-map.md" -Status "failed" -ExitCode 1 -LogPath $logPath -Reason ($missing -join "; ")
}

$commands = New-Object System.Collections.Generic.List[object]
$artifacts = New-Object System.Collections.Generic.List[object]

switch ($Area) {
    "workspace" {
        $args = @("-Profile", "quick", "-TimeoutSeconds", [string]$TimeoutSeconds)
        if ($KeepArtifacts) { $args += "-KeepArtifacts" }
        $commands.Add((Invoke-ScriptCommand -Id "workspace" -ScriptPath (Join-Path $repoRoot "scripts/dev/test-suite.ps1") -Arguments $args))
    }
    "smoke" {
        $args = @("-Profile", "smoke", "-TimeoutSeconds", [string]$TimeoutSeconds)
        if ($KeepArtifacts) { $args += "-KeepArtifacts" }
        $commands.Add((Invoke-ScriptCommand -Id "smoke" -ScriptPath (Join-Path $repoRoot "scripts/dev/test-suite.ps1") -Arguments $args))
    }
    "installer" {
        if ([string]::IsNullOrWhiteSpace($InstallerPath)) {
            $commands.Add((New-SkippedCommand -Id "installer_smoke" -Command "scripts/dev/installer-smoke.ps1" -Reason "InstallerPath was not provided"))
        }
        else {
            $args = @("-InstallerPath", $InstallerPath, "-OutputRoot", (Join-Path $OutputRoot "installer-smoke"))
            if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) { $args += @("-PreviousInstallerPath", $PreviousInstallerPath) }
            if ($RequireSignature) { $args += "-RequireSignature" }
            if ($KeepArtifacts) { $args += "-KeepArtifacts" }
            $commands.Add((Invoke-ScriptCommand -Id "installer_smoke" -ScriptPath (Join-Path $repoRoot "scripts/dev/installer-smoke.ps1") -Arguments $args))
            $summaryPath = Join-Path $OutputRoot "installer-smoke/installer-smoke.json"
            if (Test-Path -LiteralPath $summaryPath) {
                $artifacts.Add([pscustomobject]@{ kind = "installer_smoke_summary"; path = $summaryPath })
            }
        }
    }
    "service" {
        if (-not $runningOnWindows) {
            $commands.Add((New-SkippedCommand -Id "service_smoke" -Command "scripts/dev/service-smoke.ps1" -Reason "service smoke is Windows-only"))
        }
        else {
            $args = @("-OutputRoot", (Join-Path $OutputRoot "service-smoke"))
            $commands.Add((Invoke-ScriptCommand -Id "service_smoke" -ScriptPath (Join-Path $repoRoot "scripts/dev/service-smoke.ps1") -Arguments $args))
            $summaryPath = Join-Path $OutputRoot "service-smoke/service-smoke.json"
            if (Test-Path -LiteralPath $summaryPath) {
                $artifacts.Add([pscustomobject]@{ kind = "service_smoke_summary"; path = $summaryPath })
            }
        }
    }
    "release" {
        $args = @("-Policy", $ReleasePolicy, "-OutputRoot", (Join-Path $OutputRoot "release-readiness"))
        if (-not [string]::IsNullOrWhiteSpace($InstallerSmokeSummaryPath)) { $args += @("-InstallerSmokeSummaryPath", $InstallerSmokeSummaryPath) }
        if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) { $args += @("-IncludeInstallerSmoke", "-InstallerPath", $InstallerPath) }
        if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) { $args += @("-PreviousInstallerPath", $PreviousInstallerPath) }
        if ($IncludeRuntimeGates) { $args += "-IncludeRuntimeGates" }
        if ($IncludeServiceSmoke) { $args += "-IncludeServiceSmoke" }
        if ($RequireSignature) { $args += "-RequireSignature" }
        if (-not [string]::IsNullOrWhiteSpace($EndpointA)) { $args += @("-EndpointA", $EndpointA) }
        if (-not [string]::IsNullOrWhiteSpace($EndpointB)) { $args += @("-EndpointB", $EndpointB) }
        $commands.Add((Invoke-ScriptCommand -Id "release_readiness" -ScriptPath (Join-Path $repoRoot "scripts/dev/release-readiness.ps1") -Arguments $args))
        $jsonPath = Join-Path $OutputRoot "release-readiness/release-readiness.json"
        $markdownPath = Join-Path $OutputRoot "release-readiness/release-readiness.md"
        if (Test-Path -LiteralPath $jsonPath) { $artifacts.Add([pscustomobject]@{ kind = "release_readiness_json"; path = $jsonPath }) }
        if (Test-Path -LiteralPath $markdownPath) { $artifacts.Add([pscustomobject]@{ kind = "release_readiness_markdown"; path = $markdownPath }) }
    }
    "docs/status" {
        $commands.Add((Invoke-DocsStatusCheck))
        $artifacts.Add([pscustomobject]@{ kind = "project_status"; path = (Join-Path $repoRoot "docs/project-status.md") })
        $artifacts.Add([pscustomobject]@{ kind = "component_map"; path = (Join-Path $repoRoot "docs/architecture/component-map.md") })
    }
}

$failed = @($commands | Where-Object { $_.status -eq "failed" })
$skipped = @($commands | Where-Object { $_.status -eq "skipped" })
$status = if ($failed.Count -gt 0) {
    "failed"
}
elseif ($commands.Count -gt 0 -and $skipped.Count -eq $commands.Count) {
    "skipped"
}
else {
    "passed"
}

$packet = [pscustomobject]@{
    schema_version = "boundless.check.v1"
    generated_at_utc = [DateTime]::UtcNow.ToString("o")
    repo_root = $repoRoot
    git_branch = Get-GitValue -Arguments @("rev-parse", "--abbrev-ref", "HEAD")
    git_commit = Get-GitValue -Arguments @("rev-parse", "HEAD")
    area = $Area
    status = $status
    output_root = $OutputRoot
    commands = @($commands.ToArray())
    artifacts = @($artifacts.ToArray())
}

if ($Format -eq "json") {
    $packet | ConvertTo-Json -Depth 8
}
else {
    Write-Host "[check] area=$Area status=$status output=$OutputRoot"
    foreach ($command in $commands) {
        Write-Host "[check] $($command.id) $($command.status) log=$($command.log_path) reason=$($command.reason)"
    }
    foreach ($artifact in $artifacts) {
        Write-Host "[check] artifact $($artifact.kind)=$($artifact.path)"
    }
}

if ($status -eq "failed") {
    exit 1
}
