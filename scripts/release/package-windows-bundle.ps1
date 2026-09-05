[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [Parameter(Mandatory = $true)]
    [string]$HelperPath,
    [Parameter(Mandatory = $true)]
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$installerName = "Boundless-$Version-windows-x64.msi"
$helperName = "Boundless-$Version-windows-x64-install.ps1"
$outputName = "Boundless-$Version-windows-x64.zip"
$installer = (Get-Item -LiteralPath $InstallerPath -ErrorAction Stop).FullName
$helper = (Get-Item -LiteralPath $HelperPath -ErrorAction Stop).FullName
$output = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
foreach ($pair in @(@($installer, $installerName), @($helper, $helperName), @($output, $outputName))) {
    if ([IO.Path]::GetFileName($pair[0]) -cne $pair[1]) {
        throw "Expected file name '$($pair[1])', got '$($pair[0])'."
    }
}

# Keep the desktop user's identity: the matching helper owns the UAC handoff.
# Literal, version-validated names also avoid wildcard selection of another MSI.
$launcher = @'
@echo off
setlocal DisableDelayedExpansion
if not exist "%~dp0__HELPER__" goto missing
if not exist "%~dp0__INSTALLER__" goto missing
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File "%~dp0__HELPER__" -InstallerPath "%~dp0__INSTALLER__"
set "BOUNDLESS_INSTALL_EXIT=%ERRORLEVEL%"
echo.
if "%BOUNDLESS_INSTALL_EXIT%"=="0" (echo Boundless installation completed.) else (echo Boundless installation returned code %BOUNDLESS_INSTALL_EXIT%. See the details above.)
goto finish
:missing
echo Extract the entire Boundless ZIP into a folder before running Install.cmd.
set "BOUNDLESS_INSTALL_EXIT=1"
:finish
if not defined BOUNDLESS_INSTALL_NO_PAUSE pause
exit /b %BOUNDLESS_INSTALL_EXIT%
'@
$launcher = $launcher.Replace('__HELPER__', $helperName).Replace('__INSTALLER__', $installerName)
$readme = @"
Boundless $Version - Windows x64 qualification preview
====================================================

1. Copy this ZIP to each Windows PC and choose Extract All.
2. Sign in as the desktop user who will use Boundless.
3. Double-click Install.cmd in the extracted folder and approve its UAC prompt.
   Keep the window open until the helper finishes its service/API/tray checks.
4. Open Boundless and pair the PCs. Install this version on both PCs.

The complete MSI is included; installation does not download another package.
Run Install.cmd normally. The matching helper captures the desktop user's
identity before elevation. Running the raw MSI skips this required setup.
Unsigned preview builds may show Unknown publisher in Windows prompts.

Existing MSI installations are upgraded by Windows Installer. Recognized old
per-user script installs are retired with a recovery copy. Unrecognized or
still-running legacy installs stop the upgrade with an explanation. Old user
configuration and trust data are preserved; obsolete supported config schemas
are migrated with a byte-for-byte backup. Moving from a per-user daemon to the
machine service requires pairing again because the service owns its identity.

The transport now defaults to TCP 16100 and nearby pairing to TCP 16200.
Older configs using the former default 15100 migrate once; custom ports stay.
TCP 15100/15101 belong to Mouse Without Borders on some machines. This bundle
does not stop or reconfigure Mouse Without Borders. Disable its input sharing
while qualifying Boundless to avoid two apps controlling the same desktops.

This is a qualification preview. Physical two-PC input, clipboard, recovery,
sleep/resume, and endurance still need testing on your actual machines.
For install troubleshooting, keep the helper output and the log path it prints.
SHA256SUMS.txt records the hashes of every other file in this bundle.
"@

$payloads = [ordered]@{}
$payloads[$installerName] = [IO.File]::ReadAllBytes($installer)
$payloads[$helperName] = [IO.File]::ReadAllBytes($helper)
$utf8 = [Text.UTF8Encoding]::new($false)
$payloads['Install.cmd'] = $utf8.GetBytes(($launcher -replace '\r?\n', "`r`n") + "`r`n")
$payloads['README.txt'] = $utf8.GetBytes(($readme -replace '\r?\n', "`r`n") + "`r`n")
$sha = [Security.Cryptography.SHA256]::Create()
try {
    $checksums = foreach ($name in $payloads.Keys) {
        $hash = [BitConverter]::ToString($sha.ComputeHash($payloads[$name])).Replace('-', '').ToLowerInvariant()
        "$hash  $name"
    }
}
finally { $sha.Dispose() }
$payloads['SHA256SUMS.txt'] = $utf8.GetBytes(($checksums -join "`n") + "`n")
New-Item -ItemType Directory -Path (Split-Path -Parent $output) -Force | Out-Null
# CreateNew prevents an accidental overwrite of a previously validated bundle.
$stream = [IO.File]::Open($output, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
try {
    $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
    try {
        foreach ($name in $payloads.Keys) {
            $entry = $archive.CreateEntry($name, [IO.Compression.CompressionLevel]::Optimal)
            $entryStream = $entry.Open()
            try { $entryStream.Write($payloads[$name], 0, $payloads[$name].Length) }
            finally { $entryStream.Dispose() }
        }
    }
    finally { $archive.Dispose() }
}
finally { $stream.Dispose() }

& (Join-Path $PSScriptRoot 'assert-windows-bundle.ps1') -BundlePath $output -Version $Version -InstallerPath $installer -HelperPath $helper
Write-Host "bundle_path=$output"
