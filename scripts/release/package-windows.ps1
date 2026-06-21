[CmdletBinding()]
param(
    [string]$RepoRoot = "",

    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$DaemonPath,

    [string]$ServicePath = "",

    [Parameter(Mandatory = $true)]
    [string]$CliPath,

    [Parameter(Mandatory = $true)]
    [string]$TrayPath,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath,

    [string]$WorkingDirectory = "",

    [switch]$KeepWorkingDirectory
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

function ConvertTo-MsiVersion {
    param([string]$SemanticVersion)

    if ($SemanticVersion -notmatch '^(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)(?<suffix>[-+].+)?$') {
        throw "Version must be SemVer-like (for example 2.0.4 or 2.0.4-beta.1): $SemanticVersion"
    }

    return "$($Matches.major).$($Matches.minor).$($Matches.patch)"
}

$repoRoot = if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
}
else {
    (Resolve-Path -LiteralPath $RepoRoot).Path
}
$daemonBinary = Resolve-RequiredPath -Path $DaemonPath -Label "Daemon binary"
$servicePathCandidate = if ([string]::IsNullOrWhiteSpace($ServicePath)) {
    Join-Path (Split-Path -Parent $daemonBinary) "boundless-service.exe"
}
else {
    $ServicePath
}
$serviceBinary = Resolve-RequiredPath -Path $servicePathCandidate -Label "Service binary"
$cliBinary = Resolve-RequiredPath -Path $CliPath -Label "CLI binary"
$trayBinary = Resolve-RequiredPath -Path $TrayPath -Label "Tray binary"
$packageAssetRoot = Resolve-RequiredPath -Path (Join-Path $repoRoot "packaging\windows") -Label "Packaging asset root"
$installerProject = Resolve-RequiredPath -Path (Join-Path $repoRoot "packaging\windows\installer\Boundless.Installer.wixproj") -Label "Installer project"
$trayIconPath = Resolve-RequiredPath -Path (Join-Path $repoRoot "crates\tray\assets\app-icon.ico") -Label "Tray app icon"
$licensePath = Resolve-RequiredPath -Path (Join-Path $repoRoot "LICENSE") -Label "LICENSE"
$changeLogPath = Resolve-RequiredPath -Path (Join-Path $repoRoot "CHANGELOG.md") -Label "CHANGELOG"

$outputPathResolved = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputFileName = [System.IO.Path]::GetFileNameWithoutExtension($outputPathResolved)
if ([string]::IsNullOrWhiteSpace($outputFileName)) {
    throw "OutputPath must include a file name: $OutputPath"
}

if ([string]::IsNullOrWhiteSpace($WorkingDirectory)) {
    $WorkingDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("boundless-package-" + [guid]::NewGuid().ToString("N"))
}

$WorkingDirectory = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($WorkingDirectory)
Ensure-Directory -Path $WorkingDirectory

$stageRoot = Join-Path $WorkingDirectory "payload"
if (Test-Path -LiteralPath $stageRoot) {
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
Ensure-Directory -Path $stageRoot

$buildOutputRoot = Join-Path $WorkingDirectory "build"
if (Test-Path -LiteralPath $buildOutputRoot) {
    Remove-Item -LiteralPath $buildOutputRoot -Recurse -Force
}
Ensure-Directory -Path $buildOutputRoot

Copy-Item -LiteralPath $daemonBinary -Destination (Join-Path $stageRoot "boundlessd.exe")
Copy-Item -LiteralPath $serviceBinary -Destination (Join-Path $stageRoot "boundless-service.exe")
Copy-Item -LiteralPath $cliBinary -Destination (Join-Path $stageRoot "boundlessctl.exe")
Copy-Item -LiteralPath $trayBinary -Destination (Join-Path $stageRoot "boundlesstray.exe")
Copy-Item -LiteralPath $trayIconPath -Destination (Join-Path $stageRoot "Boundless.ico")
Copy-Item -LiteralPath $licensePath -Destination (Join-Path $stageRoot "LICENSE.txt")
Copy-Item -LiteralPath $changeLogPath -Destination (Join-Path $stageRoot "CHANGELOG.md")

$packageFiles = @(
    "Boundless-Install.ps1"
    "Boundless-Reset.ps1"
    "Boundless-ConnectivityDiagnostics.ps1"
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
$manifest.package_name = [System.IO.Path]::GetFileName($outputPathResolved)
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8

$outputParent = Split-Path -Parent $outputPathResolved
if (-not [string]::IsNullOrWhiteSpace($outputParent)) {
    Ensure-Directory -Path $outputParent
}

if (Test-Path -LiteralPath $outputPathResolved) {
    Remove-Item -LiteralPath $outputPathResolved -Force
}

$msiVersion = ConvertTo-MsiVersion -SemanticVersion $Version
$buildArguments = @(
    "build",
    $installerProject,
    "-c",
    "Release",
    "-p:PayloadDir=$stageRoot",
    "-p:ProductVersion=$msiVersion",
    "-p:OutputName=$outputFileName",
    "-p:OutputPath=$buildOutputRoot"
)

Push-Location $repoRoot
try {
    & dotnet @buildArguments
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet build failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

$builtInstaller = Get-ChildItem -LiteralPath $buildOutputRoot -Recurse -Filter "*.msi" -File |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1

if ($null -eq $builtInstaller) {
    throw "The WiX build did not produce an MSI under $buildOutputRoot"
}

Copy-Item -LiteralPath $builtInstaller.FullName -Destination $outputPathResolved -Force

$installHelperOutputPath = Join-Path $outputParent "$outputFileName-install.ps1"
Copy-Item -LiteralPath (Join-Path $packageAssetRoot "Boundless-Install.ps1") -Destination $installHelperOutputPath -Force

Write-Host "package_root=$stageRoot"
Write-Host "installer_path=$outputPathResolved"
Write-Host "installer_helper_path=$installHelperOutputPath"
Write-Host "msi_version=$msiVersion"

if (-not $KeepWorkingDirectory -and (Test-Path -LiteralPath $WorkingDirectory)) {
    Remove-Item -LiteralPath $WorkingDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
