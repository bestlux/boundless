[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$DaemonPath,

    [Parameter(Mandatory = $true)]
    [string]$CliPath,

    [Parameter(Mandatory = $true)]
    [string]$TrayPath,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath,

    [string]$WorkingDirectory = "",

    [switch]$SkipArchive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-RequiredPath {
    param(
        [string]$Path,
        [string]$Label
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "$Label was not found: $Path"
    }

    return (Resolve-Path -LiteralPath $Path).Path
}

function Ensure-Directory {
    param([string]$Path)

    New-Item -ItemType Directory -Force -Path $Path | Out-Null
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$daemonBinary = Resolve-RequiredPath -Path $DaemonPath -Label "Daemon binary"
$cliBinary = Resolve-RequiredPath -Path $CliPath -Label "CLI binary"
$trayBinary = Resolve-RequiredPath -Path $TrayPath -Label "Tray binary"
$packageAssetRoot = Resolve-RequiredPath -Path (Join-Path $repoRoot "packaging\windows") -Label "Packaging asset root"
$licensePath = Resolve-RequiredPath -Path (Join-Path $repoRoot "LICENSE") -Label "LICENSE"
$changeLogPath = Resolve-RequiredPath -Path (Join-Path $repoRoot "CHANGELOG.md") -Label "CHANGELOG"

$outputFileName = [System.IO.Path]::GetFileNameWithoutExtension($OutputPath)
if ([string]::IsNullOrWhiteSpace($outputFileName)) {
    throw "OutputPath must include a file name: $OutputPath"
}

if ([string]::IsNullOrWhiteSpace($WorkingDirectory)) {
    $WorkingDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("boundless-package-" + [guid]::NewGuid().ToString("N"))
}

Ensure-Directory -Path $WorkingDirectory
$stageRoot = Join-Path $WorkingDirectory $outputFileName
if (Test-Path -LiteralPath $stageRoot) {
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
Ensure-Directory -Path $stageRoot

Copy-Item -LiteralPath $daemonBinary -Destination (Join-Path $stageRoot "boundlessd.exe")
Copy-Item -LiteralPath $cliBinary -Destination (Join-Path $stageRoot "boundlessctl.exe")
Copy-Item -LiteralPath $trayBinary -Destination (Join-Path $stageRoot "boundlesstray.exe")
Copy-Item -LiteralPath $licensePath -Destination (Join-Path $stageRoot "LICENSE.txt")
Copy-Item -LiteralPath $changeLogPath -Destination (Join-Path $stageRoot "CHANGELOG.md")

$packageFiles = @(
    "Boundless-Install.ps1"
    "Boundless-Uninstall.ps1"
    "Boundless-Reset.ps1"
    "README.txt"
    "package-manifest.json"
)

foreach ($file in $packageFiles) {
    Copy-Item -LiteralPath (Join-Path $packageAssetRoot $file) -Destination (Join-Path $stageRoot $file)
}

$manifestPath = Join-Path $stageRoot "package-manifest.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$manifest.version = $Version
$manifest.generated_at_utc = [DateTime]::UtcNow.ToString("o")
$manifest.package_name = $outputFileName
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8

$outputPathResolved = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputParent = Split-Path -Parent $outputPathResolved
if (-not [string]::IsNullOrWhiteSpace($outputParent)) {
    Ensure-Directory -Path $outputParent
}

if (Test-Path -LiteralPath $outputPathResolved) {
    Remove-Item -LiteralPath $outputPathResolved -Force
}

if (-not $SkipArchive) {
    Compress-Archive -LiteralPath $stageRoot -DestinationPath $outputPathResolved
}

Write-Host "package_root=$stageRoot"
if (-not $SkipArchive) {
    Write-Host "package_archive=$outputPathResolved"
}
