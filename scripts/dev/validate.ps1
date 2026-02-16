param(
    [switch]$SkipSmoke,
    [switch]$IncludeThreeNodeSmoke
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$originalCargoIncremental = $env:CARGO_INCREMENTAL
$env:CARGO_INCREMENTAL = "0"

try {
    Write-Host "[validate] cargo fmt --all -- --check"
    cargo fmt --all -- --check | Out-Host

    Write-Host "[validate] cargo test --workspace"
    cargo test --workspace | Out-Host

    Write-Host "[validate] cargo clippy --workspace --all-targets -- -D warnings"
    cargo clippy --workspace --all-targets -- -D warnings | Out-Host

    if (-not $SkipSmoke) {
        Write-Host "[validate] scripts/dev/two-node-smoke.ps1"
        & (Join-Path $repoRoot "scripts/dev/two-node-smoke.ps1") -TimeoutSeconds 60 | Out-Host

        if ($IncludeThreeNodeSmoke) {
            Write-Host "[validate] scripts/dev/three-node-smoke.ps1"
            & (Join-Path $repoRoot "scripts/dev/three-node-smoke.ps1") -TimeoutSeconds 90 | Out-Host
        }
    }

    Write-Host "[validate] complete"
}
finally {
    if ($null -eq $originalCargoIncremental) {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_INCREMENTAL = $originalCargoIncremental
    }
}
