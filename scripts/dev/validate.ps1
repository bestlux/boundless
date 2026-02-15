param(
    [switch]$SkipSmoke
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

Write-Host "[validate] cargo fmt --all -- --check"
cargo fmt --all -- --check | Out-Host

Write-Host "[validate] cargo test --workspace"
cargo test --workspace | Out-Host

Write-Host "[validate] cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings | Out-Host

if (-not $SkipSmoke) {
    Write-Host "[validate] scripts/dev/two-node-smoke.ps1"
    & (Join-Path $repoRoot "scripts/dev/two-node-smoke.ps1") -TimeoutSeconds 60 | Out-Host
}

Write-Host "[validate] complete"
