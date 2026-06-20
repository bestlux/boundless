Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Warning "scripts/dev/v5-readiness.ps1 is deprecated; use scripts/dev/release-readiness.ps1."
& (Join-Path $PSScriptRoot "release-readiness.ps1") @args
exit $LASTEXITCODE
