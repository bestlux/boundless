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

function Get-PowerShellFunctionSource {
    param(
        [string]$Path,
        [string]$Name
    )

    $tokens = $null
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile(
        $Path,
        [ref]$tokens,
        [ref]$errors
    )
    if ($errors.Count -ne 0) {
        throw "$Path did not parse while inspecting function $Name`: $($errors[0].Message)"
    }
    $functionAst = $ast.FindAll(
        {
            param($node)
            $node -is [Management.Automation.Language.FunctionDefinitionAst] -and
                $node.Name -eq $Name
        },
        $true
    ) | Select-Object -First 1
    if ($null -eq $functionAst) {
        throw "$Path did not define required function $Name."
    }
    return $functionAst.Extent.Text.Replace("`r`n", "`n")
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

$installScript = Join-Path $packagingRoot "Boundless-Install.ps1"
if (-not (Test-Path -LiteralPath $installScript)) {
    throw "Boundless-Install.ps1 was not found under $packagingRoot"
}
$installScriptText = Get-Content -LiteralPath $installScript -Raw
foreach ($requiredInstallContract in @(
        'BoundlessInstaller-',
        'boundless_install_tray_quiescence_acquired',
        'ExpectedInstallerSha256',
        'CommonApplicationData',
        'New-BoundlessSecuredDirectoryAtomic',
        'staging_child_process_probe_hosts',
        'BoundlessHelperStartupAnchor',
        'New-BoundlessInstallerAnchor',
        'Request-BoundlessTrayShutdownSignal',
        'UpgradeQuiescence.v1',
        'Start-BoundlessTrayQuiescenceMonitor',
        'Get-WindowsCommandExecutablePath',
        'Wait-BoundlessElevatedInstallSupervised',
        'Boundless-install.log',
        'Copy-BoundlessInstallerLogHandoff',
        'ElevatedInstallCancelEvent',
        'caller_privilege_log_handoff_fixture',
        'BoundlessOwnedProcessBoundary',
        'BoundlessElevatedJob',
        'coordinator_death_cancellation_fixture',
        'failed_drain_quiescence_fixture',
        'kernel_object_acl_fixture',
        'blocking_service_stop_fixture',
        'failed_msi_service_recovery_fixture',
        'Start-BoundlessTrayQuiescenceSentinelOwner',
        'ElevatedInstallCoordinatorProcessId',
        'Get-BoundlessServiceStatusBounded',
        'Boundless-install-result.txt',
        '(A;;RC;;;OW)'
    )) {
    if ($installScriptText -notmatch [regex]::Escape($requiredInstallContract)) {
        throw "Boundless-Install.ps1 is missing the upgrade safety contract: $requiredInstallContract"
    }
}
if ($installScriptText -notmatch 'ExpectedOwnerSid\s*=\s*\$selection\.sid') {
    throw "Boundless-Install.ps1 must key tray quiescence to the selected desktop SID."
}
$stopTraySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Stop-BoundlessTrayForUpgrade'
if ($stopTraySource -match 'Invoke-BoundedProcess' -or $stopTraySource -match '--quit') {
    throw "Boundless-Install.ps1 must not execute a tray image discovered from a user process."
}
$postInstallSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-PostInstallVerification'
if (
    $postInstallSource -match 'Get-MsiProperty' -or
    $postInstallSource -match 'ResolvedInstallerPath'
) {
    throw "Post-install verification must use the immutable pre-UAC MSI metadata anchor."
}
$elevatedCommandSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'New-BoundlessElevatedInstallCommand'
if (
    $elevatedCommandSource -match 'payload\.log_path' -or
    $elevatedCommandSource -match '(?m)^\s*log_path\s*='
) {
    throw "Elevated installer payload must not carry the caller-selected log destination."
}
$invokeMsiSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-BoundlessMsi'
if (
    $invokeMsiSource -notmatch 'Wait-BoundlessElevatedInstallSupervised' -or
    $invokeMsiSource -notmatch 'Copy-BoundlessInstallerLogHandoff' -or
    $invokeMsiSource -notmatch 'CompletionEvent' -or
    $invokeMsiSource -notmatch 'TreeJobName' -or
    $invokeMsiSource -notmatch 'TreeClosureState'
) {
    throw "Installer parent must supervise elevation/tree completion and copy the completed staged log itself."
}
$elevatedCommandSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'New-BoundlessElevatedInstallCommand'
if (
    $elevatedCommandSource -notmatch 'TerminateJobObject' -or
    $elevatedCommandSource -notmatch 'StartOwned' -or
    $elevatedCommandSource -notmatch 'StartGate' -or
    $elevatedCommandSource -notmatch 'Get-CancellationReason'
) {
    throw "Elevated installer helper must own a gated kill-on-close process tree and liveness boundary."
}
$serviceStopSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Stop-BoundlessServiceForUpgrade'
if (
    $serviceStopSource -match '\$service\.Stop\(\)' -or
    $serviceStopSource -notmatch 'Wait-BoundlessServiceTransition' -or
    $serviceStopSource -notmatch 'The MSI was not started' -or
    $serviceStopSource -notmatch 'BoundlessServiceStopInitialStatus'
) {
    throw "Upgrade service stop must supervise the blocking SCM request outside the installer process."
}
$preMsiRecoverySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Stop-BoundlessServiceBeforeMsi'
if (
    $preMsiRecoverySource -notmatch 'original_service_not_running_or_stop_not_requested' -or
    $preMsiRecoverySource -notmatch 'service_missing_or_uninstall_policy' -or
    $preMsiRecoverySource -notmatch 'Start-BoundlessServiceAfterFailedInstall' -or
    $preMsiRecoverySource -notmatch 'BoundlessServiceRecoveryError'
) {
    throw "A failed pre-MSI partial service stop must restore only a proven originally-running service."
}
$elevatedPhaseSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-ElevatedInstallPhase'
if ($elevatedPhaseSource -notmatch 'Stop-BoundlessServiceBeforeMsi') {
    throw "The elevated install phase must use the bounded pre-MSI service recovery boundary."
}
$serviceRecoverySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-BoundlessMsiWithServiceRecovery'
if (
    $serviceRecoverySource -notmatch 'definitive_failure' -or
    $serviceRecoverySource -notmatch 'initial_status -eq "Running"' -or
    $serviceRecoverySource -notmatch 'BoundlessServiceRecoveryError'
) {
    throw "Failed MSI recovery must restart only an originally running service after a definitive boundary."
}

$serviceRecovery = Join-Path $RepoRoot 'crates\tray\src\service_recovery.rs'
$serviceRecoveryText = Get-Content -LiteralPath $serviceRecovery -Raw
foreach ($requiredServiceStartContract in @(
        'ServiceStartOriginGuard',
        'service_start_origin_event_name',
        'service-start-origin-sid',
        'tray_upgrade_quiescence_sentinel_name',
        'held_upgrade_sentinel_refuses_before_scm_start_mutation'
    )) {
    if ($serviceRecoveryText -notmatch [regex]::Escape($requiredServiceStartContract)) {
        throw "Tray service-start recovery is missing privileged origin/quiescence contract: $requiredServiceStartContract"
    }
}

$packagingReadme = Join-Path $packagingRoot "README.txt"
$packagingReadmeText = Get-Content -LiteralPath $packagingReadme -Raw
if ($packagingReadmeText -match '(?mi)^\s*msiexec(?:\.exe)?\s+/i') {
    throw "README.txt must not recommend raw msiexec because it bypasses the upgrade lifecycle helper."
}
if (
    $packagingReadmeText -notmatch 'windows-x64-install\.ps1' -or
    $packagingReadmeText -notmatch '-AllowedUserSid'
) {
    throw "README.txt must route elevated fallback installs through the matching helper with an explicit SID."
}

$installerSmoke = Join-Path $RepoRoot "scripts\dev\installer-smoke.ps1"
if (-not (Test-Path -LiteralPath $installerSmoke)) {
    throw "Installer smoke script was not found: $installerSmoke"
}
$installerSmokeText = Get-Content -LiteralPath $installerSmoke -Raw
foreach ($requiredSmokeContract in @(
        'Invoke-BoundlessInstallHelper',
        'install_helper_upgrade_evidence',
        'boundless_install_tray_quiescence_acquired',
        'Get-WindowsCommandExecutablePath',
        'Assert-WindowsServiceExecutablePathFixtures'
    )) {
    if ($installerSmokeText -notmatch [regex]::Escape($requiredSmokeContract)) {
        throw "installer-smoke.ps1 is missing helper-driven upgrade evidence: $requiredSmokeContract"
    }
}
foreach ($sharedFunction in @(
        'Test-WindowsPathEqual',
        'Get-WindowsCommandExecutablePath',
        'Assert-WindowsServiceExecutablePathFixtures'
    )) {
    $helperFunction = Get-PowerShellFunctionSource -Path $installScript -Name $sharedFunction
    $smokeFunction = Get-PowerShellFunctionSource -Path $installerSmoke -Name $sharedFunction
    if (-not [string]::Equals($helperFunction, $smokeFunction, [StringComparison]::Ordinal)) {
        throw "Service executable validation function drifted between helper and smoke: $sharedFunction"
    }
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

Write-Host "packaging_script_smoke=passed self_tests=$($selfTestScripts.Count) install_resolve_only=passed wix_upgrade_contract=passed helper_upgrade_contract=passed"
