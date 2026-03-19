[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$crateManifests = Get-ChildItem -LiteralPath (Join-Path $repoRoot 'crates') -Filter Cargo.toml -Recurse -File
$invalidManifests = @()

foreach ($manifest in $crateManifests) {
    $match = Select-String -Path $manifest.FullName -Pattern '^\s*version\.workspace\s*=\s*true\s*$'
    if ($match) {
        $invalidManifests += $manifest.FullName
    }
}

if ($invalidManifests.Count -gt 0) {
    $paths = $invalidManifests | ForEach-Object { " - $_" }
    throw @"
release-please's cargo-workspace plugin requires a literal [package].version in every crate manifest.
Replace version.workspace = true with an explicit version string in:
$($paths -join [Environment]::NewLine)
"@
}
