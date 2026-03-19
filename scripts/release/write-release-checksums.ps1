[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]]$AssetPaths,

    [Parameter(Mandatory = $true)]
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$resolvedAssets = @()
foreach ($assetPath in $AssetPaths) {
    if ([string]::IsNullOrWhiteSpace($assetPath)) {
        continue
    }

    if (-not (Test-Path -LiteralPath $assetPath)) {
        throw "Asset was not found: $assetPath"
    }

    $resolvedAssets += (Resolve-Path -LiteralPath $assetPath).Path
}

if ($resolvedAssets.Count -eq 0) {
    throw "At least one asset path is required."
}

$lines = foreach ($asset in ($resolvedAssets | Sort-Object)) {
    $hash = (Get-FileHash -LiteralPath $asset -Algorithm SHA256).Hash.ToLowerInvariant()
    $name = [System.IO.Path]::GetFileName($asset)
    "$hash  $name"
}

$outputPathResolved = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputDirectory = Split-Path -Parent $outputPathResolved
if (-not [string]::IsNullOrWhiteSpace($outputDirectory)) {
    New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
}

Set-Content -LiteralPath $outputPathResolved -Value ($lines -join [Environment]::NewLine) -Encoding ascii
Write-Host "checksums_path=$outputPathResolved"
