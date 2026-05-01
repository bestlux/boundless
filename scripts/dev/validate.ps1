param(
    [switch]$SkipSmoke,
    [switch]$IncludeSmoke,
    [switch]$IncludeThreeNodeSmoke,
    [switch]$KeepArtifacts,
    [int]$TimeoutSeconds = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$explicitSmokeProfiles = @($IncludeSmoke.IsPresent, $IncludeThreeNodeSmoke.IsPresent) | Where-Object { $_ }
if ($SkipSmoke -and $explicitSmokeProfiles.Count -gt 0) {
    throw "-SkipSmoke cannot be combined with -IncludeSmoke or -IncludeThreeNodeSmoke"
}

$profile = "quick"
if ($IncludeThreeNodeSmoke) {
    $profile = "full"
}
elseif ($IncludeSmoke) {
    $profile = "smoke"
}

$suiteParams = @{
    Profile = $profile
    TimeoutSeconds = $TimeoutSeconds
}
if ($KeepArtifacts) {
    $suiteParams.KeepArtifacts = $true
}

Write-Host "[validate] forwarding to scripts/dev/test-suite.ps1 profile=$profile"
& (Join-Path $repoRoot "scripts/dev/test-suite.ps1") @suiteParams | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "test-suite failed with exit code $LASTEXITCODE"
}
