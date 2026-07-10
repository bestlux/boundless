[CmdletBinding()]
param(
    [string]$RepoRoot = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
    $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
}
else {
    $RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
}

function Resolve-PowerShellExecutable {
    foreach ($name in @("pwsh", "powershell")) {
        $command = Get-Command $name -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $command) {
            return $command.Source
        }
    }

    throw "Could not find pwsh or powershell on PATH."
}

function Invoke-PackagingScript {
    param(
        [string]$ScriptPath,
        [string[]]$Arguments
    )

    Write-Host "[packaging-script-smoke] $([IO.Path]::GetFileName($ScriptPath)) $($Arguments -join ' ')"
    $global:LASTEXITCODE = 0
    $output = @(& $script:PowerShellExe -NoProfile -ExecutionPolicy Bypass -File $ScriptPath @Arguments 2>&1)
    $exitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
    foreach ($line in $output) {
        Write-Host $line
    }

    if ($exitCode -ne 0) {
        throw "$ScriptPath exited with $exitCode"
    }

    return [pscustomobject]@{
        exit_code = $exitCode
        output = @($output | ForEach-Object { $_.ToString() })
    }
}

$script:PowerShellExe = Resolve-PowerShellExecutable
$packagingRoot = Join-Path $RepoRoot "packaging\windows"
if (-not (Test-Path -LiteralPath $packagingRoot)) {
    throw "Packaging root was not found: $packagingRoot"
}

$packageWxs = Join-Path $packagingRoot "installer\Package.wxs"
if (-not (Test-Path -LiteralPath $packageWxs)) {
    throw "WiX package source was not found: $packageWxs"
}
$packageWxsText = Get-Content -LiteralPath $packageWxs -Raw
if ($packageWxsText -notmatch 'AllowSameVersionUpgrades="yes"') {
    throw "Package.wxs must allow same-version dogfood upgrades."
}
if ($packageWxsText -match 'Id="CloseBoundlessService"') {
    throw "Package.wxs must not use CloseApplication/TerminateProcess for BoundlessService; helper stop plus ServiceControl own that lifecycle."
}
$wixProject = Join-Path $packagingRoot "installer\Boundless.Installer.wixproj"
$wixProjectText = Get-Content -LiteralPath $wixProject -Raw
if ($wixProjectText -notmatch '<SuppressIces>[^<]*ICE61') {
    throw "The intentional same-version upgrade range must suppress ICE61 package noise."
}

$selfTestScripts = @(
    Get-ChildItem -LiteralPath $packagingRoot -Filter "*.ps1" -File |
        Where-Object {
            Select-String -LiteralPath $_.FullName -Pattern '\[switch\]\$SelfTest' -Quiet
        } |
        Sort-Object Name
)
if ($selfTestScripts.Count -eq 0) {
    throw "No packaging scripts with -SelfTest were found under $packagingRoot"
}

foreach ($scriptFile in $selfTestScripts) {
    Invoke-PackagingScript -ScriptPath $scriptFile.FullName -Arguments @("-SelfTest") | Out-Null
}

$installScript = Join-Path $packagingRoot "Boundless-Install.ps1"
if (-not (Test-Path -LiteralPath $installScript)) {
    throw "Boundless-Install.ps1 was not found under $packagingRoot"
}

$smokeSid = "S-1-5-21-1000-1000-1000-1001"
$installResult = Invoke-PackagingScript -ScriptPath $installScript -Arguments @(
    "-ResolveOnly",
    "-AllowedUserSid",
    $smokeSid
)
$summary = ($installResult.output -join "`n") | ConvertFrom-Json
if ($summary.status -ne "resolved") {
    throw "Boundless-Install.ps1 -ResolveOnly did not report status=resolved"
}
if ($summary.selected_user_sid -ne $smokeSid) {
    throw "Boundless-Install.ps1 -ResolveOnly resolved unexpected SID: $($summary.selected_user_sid)"
}

Write-Host "packaging_script_smoke=passed self_tests=$($selfTestScripts.Count) install_resolve_only=passed wix_upgrade_contract=passed"
