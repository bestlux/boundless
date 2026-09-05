[CmdletBinding()]
param([string]$RepoRoot = '')

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path }
$root = Join-Path ([IO.Path]::GetTempPath()) ('BoundlessBundleFixtures-' + [guid]::NewGuid().ToString('N'))
$source = Join-Path $root 'source files'
$extracted = Join-Path $root 'extracted bundle & spaces'
$version = '1.2.3'
$installerName = "Boundless-$version-windows-x64.msi"
$helperName = "Boundless-$version-windows-x64-install.ps1"
$bundleName = "Boundless-$version-windows-x64.zip"
$builder = Join-Path $RepoRoot 'scripts/release/package-windows-bundle.ps1'
$validator = Join-Path $RepoRoot 'scripts/release/assert-windows-bundle.ps1'
$savedNoPause = $env:BOUNDLESS_INSTALL_NO_PAUSE
$savedFixtureExit = $env:BOUNDLESS_BUNDLE_FIXTURE_EXIT
try {
    New-Item -ItemType Directory -Path $source -Force | Out-Null
    $installer = Join-Path $source $installerName
    $helper = Join-Path $source $helperName
    $bundle = Join-Path $source $bundleName
    [IO.File]::WriteAllBytes($installer, [byte[]](0, 1, 2, 128, 255))
    # Exercise the real extracted CMD launcher with a benign helper: no MSI/UAC.
    $stub = @'
param([string]$InstallerPath)
$ErrorActionPreference = 'Stop'
$expected = Join-Path $PSScriptRoot 'Boundless-1.2.3-windows-x64.msi'
if ($InstallerPath -cne $expected) { throw "Unexpected installer argument: $InstallerPath" }
if (-not (Test-Path -LiteralPath $InstallerPath)) { throw 'Installer missing' }
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'launch-evidence.txt'), $InstallerPath)
exit ([int]$env:BOUNDLESS_BUNDLE_FIXTURE_EXIT)
'@
    [IO.File]::WriteAllText($helper, $stub, [Text.UTF8Encoding]::new($false))
    & $builder -Version $version -InstallerPath $installer -HelperPath $helper -OutputPath $bundle
    [IO.Compression.ZipFile]::ExtractToDirectory($bundle, $extracted)
    if ((Get-FileHash -LiteralPath (Join-Path $extracted $installerName)).Hash -ne (Get-FileHash -LiteralPath $installer).Hash) {
        throw 'Extracted MSI does not match the original.'
    }
    $env:BOUNDLESS_INSTALL_NO_PAUSE = '1'
    foreach ($expectedExit in @(0, 37)) {
        $env:BOUNDLESS_BUNDLE_FIXTURE_EXIT = [string]$expectedExit
        & (Join-Path $extracted 'Install.cmd') | Out-Host
        if ($LASTEXITCODE -ne $expectedExit) { throw "Launcher lost exit code $expectedExit (got $LASTEXITCODE)." }
        if (-not (Test-Path -LiteralPath (Join-Path $extracted 'launch-evidence.txt'))) { throw 'Launcher did not run helper.' }
    }
    Remove-Item -LiteralPath (Join-Path $extracted $installerName) -Force
    & (Join-Path $extracted 'Install.cmd') | Out-Host
    if ($LASTEXITCODE -eq 0) { throw 'Launcher accepted an incompletely extracted bundle.' }

    $archive = [IO.Compression.ZipFile]::Open($bundle, [IO.Compression.ZipArchiveMode]::Update)
    try {
        $archive.GetEntry('README.txt').Delete()
        $writer = [IO.StreamWriter]::new($archive.CreateEntry('README.txt').Open())
        try { $writer.Write('corrupt content') } finally { $writer.Dispose() }
    }
    finally { $archive.Dispose() }
    $rejected = $false
    try { & $validator -Version $version -BundlePath $bundle -InstallerPath $installer -HelperPath $helper }
    catch {
        if ($_.Exception.Message -notlike '*checksum mismatch*') { throw }
        $rejected = $true
    }
    if (-not $rejected) { throw 'Validator accepted a corrupted bundle.' }
    Write-Host 'windows_bundle_fixtures=passed archive_hashes=passed cmd_spaces=passed cmd_failure_exit=passed missing_payload=passed corrupt_payload_rejected=passed'
}
finally {
    $env:BOUNDLESS_INSTALL_NO_PAUSE = $savedNoPause
    $env:BOUNDLESS_BUNDLE_FIXTURE_EXIT = $savedFixtureExit
    $fullRoot = [IO.Path]::GetFullPath($root).TrimEnd('\')
    $fullTemp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    if (-not $fullRoot.StartsWith("$fullTemp\", [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetFileName($fullRoot) -notmatch '^BoundlessBundleFixtures-[0-9a-f]{32}$') {
        throw "Refusing to clean an unsafe fixture path: $fullRoot"
    }
    if (Test-Path -LiteralPath $fullRoot) { Remove-Item -LiteralPath $fullRoot -Recurse -Force }
}
