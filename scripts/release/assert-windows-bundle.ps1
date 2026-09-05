[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BundlePath,
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [Parameter(Mandatory = $true)]
    [string]$HelperPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$installerName = "Boundless-$Version-windows-x64.msi"
$helperName = "Boundless-$Version-windows-x64-install.ps1"
if ([IO.Path]::GetFileName($BundlePath) -cne "Boundless-$Version-windows-x64.zip") {
    throw 'Bundle filename does not match the release version.'
}
$expected = @($installerName, $helperName, 'Install.cmd', 'README.txt', 'SHA256SUMS.txt')
$archive = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath $BundlePath).Path)
$sha = [Security.Cryptography.SHA256]::Create()
try {
    $names = @($archive.Entries | ForEach-Object { $_.FullName })
    if ($names.Count -ne $expected.Count -or @($names | Select-Object -Unique).Count -ne $expected.Count) {
        throw 'Bundle must contain exactly the five distinct release payloads.'
    }
    foreach ($name in $names) {
        if ($expected -cnotcontains $name) { throw "Unexpected bundle entry: $name" }
    }
    $checksumReader = [IO.StreamReader]::new($archive.GetEntry('SHA256SUMS.txt').Open())
    try { $lines = @($checksumReader.ReadToEnd().TrimEnd() -split '\r?\n') }
    finally { $checksumReader.Dispose() }
    if ($lines.Count -ne 4) { throw 'Expected four bundled checksums.' }
    $checksums = @{}
    foreach ($line in $lines) {
        if ($line -cnotmatch '^([0-9a-f]{64})  (.+)$' -or $checksums.ContainsKey($Matches[2])) {
            throw 'Invalid or duplicate bundled checksum.'
        }
        $checksums[$Matches[2]] = $Matches[1]
    }
    foreach ($name in $expected | Where-Object { $_ -ne 'SHA256SUMS.txt' }) {
        $entryStream = $archive.GetEntry($name).Open()
        try { $hash = [BitConverter]::ToString($sha.ComputeHash($entryStream)).Replace('-', '').ToLowerInvariant() }
        finally { $entryStream.Dispose() }
        if (-not $checksums.ContainsKey($name) -or $checksums[$name] -cne $hash) {
            throw "Bundle checksum mismatch: $name"
        }
        $external = if ($name -eq $installerName) { $InstallerPath } elseif ($name -eq $helperName) { $HelperPath } else { $null }
        if ($null -ne $external -and (Get-FileHash -LiteralPath $external -Algorithm SHA256).Hash.ToLowerInvariant() -cne $hash) {
            throw "Bundle differs from the matching release artifact: $name"
        }
    }
}
finally {
    $sha.Dispose()
    $archive.Dispose()
}
Write-Host 'windows_bundle_validation=passed'
