param(
    [Parameter(Mandatory = $true)]
    [string]$Scenario,
    [Parameter(Mandatory = $true)]
    [ValidateSet("before", "after", "failure")]
    [string]$Phase,
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "http://127.0.0.1:50051",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [string]$OutputRoot = "",
    [int]$EventsLimit = 200
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"

if (-not (Test-Path $cliExe)) {
    throw "boundlessctl binary not found at $cliExe; run cargo build --locked -p boundless-cli first"
}

if ($EventsLimit -le 0) {
    throw "EventsLimit must be > 0"
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot "artifacts/multi-display-validation"
}

$safeScenario = ($Scenario -replace "[^a-zA-Z0-9\-_]", "_").Trim("_")
if ([string]::IsNullOrWhiteSpace($safeScenario)) {
    throw "Scenario name must contain at least one alphanumeric character"
}

$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$captureDir = Join-Path $OutputRoot ("{0}-{1}-{2}" -f $safeScenario, $Phase, $timestamp)
$null = New-Item -ItemType Directory -Force -Path $captureDir

function Invoke-CliChecked {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $allArgs = @("--endpoint", $Endpoint) + $CommandArgs
    $output = & $cliExe @allArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "CLI command failed endpoint=${Endpoint} args='$($CommandArgs -join " ")' exit=$LASTEXITCODE output=$output"
    }

    return $output
}

function Write-CommandOutput {
    param(
        [string]$Endpoint,
        [string]$Label,
        [string]$Name,
        [string[]]$CommandArgs
    )

    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs $CommandArgs
    $path = Join-Path $captureDir ("{0}-{1}.txt" -f $Label, $Name)
    Set-Content -Path $path -Value $output
}

function Capture-Snapshot {
    param(
        [string]$Endpoint,
        [string]$Label
    )

    Write-Host "[capture] collecting snapshot for $Label ($Endpoint)"
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "daemon-status" -CommandArgs @("daemon", "status")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "peer-list" -CommandArgs @("peer", "list")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "layout-show" -CommandArgs @("layout", "show")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "feature-list" -CommandArgs @("feature", "list")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "input-owner" -CommandArgs @("input", "owner")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "input-capture-target" -CommandArgs @("input", "capture-target")
    Write-CommandOutput -Endpoint $Endpoint -Label $Label -Name "transport-events" -CommandArgs @("transport", "events", "--limit", "$EventsLimit")
}

$commit = (& git rev-parse --short HEAD 2>$null)
if ($LASTEXITCODE -ne 0) {
    $commit = "unknown"
}

$metadata = @(
    "scenario=$Scenario",
    "phase=$Phase",
    "timestamp=$timestamp",
    "commit=$commit",
    "endpoint_a=$EndpointA",
    "endpoint_b=$EndpointB",
    "label_a=$LabelA",
    "label_b=$LabelB",
    "events_limit=$EventsLimit"
)
Set-Content -Path (Join-Path $captureDir "capture-metadata.txt") -Value ($metadata -join [Environment]::NewLine)

Capture-Snapshot -Endpoint $EndpointA -Label $LabelA
Capture-Snapshot -Endpoint $EndpointB -Label $LabelB

Write-Host "[capture] snapshot written to $captureDir"
