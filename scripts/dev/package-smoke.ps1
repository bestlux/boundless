[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallerPath = "",
    [string]$PreviousInstallerPath = "",
    [string]$OutputRoot = "",
    [switch]$RequireSignature,
    [switch]$KeepArtifacts
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$arguments = @()
if (-not [string]::IsNullOrWhiteSpace($Version)) {
    $arguments += @("-Version", $Version)
}
if (-not [string]::IsNullOrWhiteSpace($InstallerPath)) {
    $arguments += @("-InstallerPath", $InstallerPath)
}
if (-not [string]::IsNullOrWhiteSpace($PreviousInstallerPath)) {
    $arguments += @("-PreviousInstallerPath", $PreviousInstallerPath)
}
if (-not [string]::IsNullOrWhiteSpace($OutputRoot)) {
    $arguments += @("-OutputRoot", $OutputRoot)
}
if ($RequireSignature) {
    $arguments += "-RequireSignature"
}
if ($KeepArtifacts) {
    $arguments += "-KeepArtifacts"
}

& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "installer-smoke.ps1") @arguments
exit $LASTEXITCODE
