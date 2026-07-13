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
        [string[]]$Arguments,
        [string]$PowerShellExe = $script:PowerShellExe
    )

    Write-Host "[packaging-script-smoke] $([IO.Path]::GetFileName($PowerShellExe)) $([IO.Path]::GetFileName($ScriptPath)) $($Arguments -join ' ')"
    $global:LASTEXITCODE = 0
    $output = @(& $PowerShellExe -NoProfile -ExecutionPolicy Bypass -File $ScriptPath @Arguments 2>&1)
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
if (
    $packageWxsText -notmatch 'Id="CloseBoundlessInputInjector"' -or
    $packageWxsText -notmatch 'Target="boundless-input-injector\.exe"' -or
    $packageWxsText -notmatch 'Id="CloseBoundlessInputInjector"(?s:.*?)TerminateProcess="1"'
) {
    throw "Package.wxs must own bounded close/termination fallback for the elevated input injector."
}
if (
    $packageWxsText -notmatch 'Id="BoundlessInputInjectorPayloadComponent"' -or
    $packageWxsText -notmatch 'Id="InputInjectorBinaryFile"' -or
    $packageWxsText -notmatch 'Source="\$\(var\.PayloadDir\)\\boundless-input-injector\.exe"'
) {
    throw "Package.wxs must install the elevated input injector as an MSI-owned Program Files payload."
}

$packageManifestPath = Join-Path $packagingRoot "package-manifest.json"
$packageManifest = Get-Content -LiteralPath $packageManifestPath -Raw | ConvertFrom-Json
if ($packageManifest.executables.input_injector -ne "boundless-input-injector.exe") {
    throw "package-manifest.json must declare the installed elevated input injector executable."
}

$packageScriptPath = Join-Path $RepoRoot "scripts\release\package-windows.ps1"
$packageScriptText = Get-Content -LiteralPath $packageScriptPath -Raw
if (
    $packageScriptText -notmatch '\[Parameter\(Mandatory = \$true\)\]\s*\[string\]\$InputInjectorPath' -or
    $packageScriptText -notmatch 'Resolve-RequiredPath -Path \$InputInjectorPath -Label "Input injector binary"' -or
    $packageScriptText -notmatch 'Copy-Item -LiteralPath \$inputInjectorBinary -Destination \(Join-Path \$stageRoot "boundless-input-injector\.exe"\)'
) {
    throw "package-windows.ps1 must require, validate, and stage the elevated input injector binary."
}

$releaseWorkflowPath = Join-Path $RepoRoot ".github\workflows\release-please.yml"
$releaseWorkflowText = Get-Content -LiteralPath $releaseWorkflowPath -Raw
if (
    $releaseWorkflowText -notmatch 'cargo build --release[^\r\n]*-p boundless-input-injector' -or
    $releaseWorkflowText -notmatch '"target/release/boundless-input-injector\.exe"' -or
    $releaseWorkflowText -notmatch '-InputInjectorPath "source/target/release/boundless-input-injector\.exe"'
) {
    throw "The Windows release workflow must build, sign, and package the elevated input injector."
}

$releasePleaseConfigPath = Join-Path $RepoRoot "release-please-config.json"
$releasePleaseConfig = Get-Content -LiteralPath $releasePleaseConfigPath -Raw | ConvertFrom-Json
$releasePleaseExtraFiles = @($releasePleaseConfig.packages."."."extra-files" | ForEach-Object { $_.path })
if ($releasePleaseExtraFiles -notcontains "crates/input-injector/Cargo.toml") {
    throw "release-please-config.json must propagate the release version into the input injector crate."
}

$inputInjectorCrateRoot = Join-Path $RepoRoot "crates\input-injector"
if (Test-Path -LiteralPath $inputInjectorCrateRoot) {
    $inputInjectorManifestPath = Join-Path $inputInjectorCrateRoot "assets\input-injector.manifest"
    if (-not (Test-Path -LiteralPath $inputInjectorManifestPath)) {
        throw "The input injector crate must keep its execution-level contract in assets/input-injector.manifest."
    }
    $inputInjectorManifestText = Get-Content -LiteralPath $inputInjectorManifestPath -Raw
    if (
        $inputInjectorManifestText -notmatch 'requestedExecutionLevel\s+level="requireAdministrator"\s+uiAccess="false"' -or
        @([regex]::Matches($inputInjectorManifestText, 'requestedExecutionLevel')).Count -ne 1
    ) {
        throw "The input injector source manifest must declare exactly one requireAdministrator, uiAccess=false execution level."
    }
}
else {
    Write-Host "input_injector_source_manifest_check=deferred_missing_crate"
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
        'kernel_object_acl_negative_probe',
        'blocking_service_stop_fixture',
        'start_pending_service_recovery_fixture',
        'failed_msi_service_recovery_fixture',
        'hard_cancel_before_msi_recovery_fixture',
        'uncertain_transaction_guardian_fixture',
        'stalled_monitor_heartbeat_fixture',
        'stalled_monitor_takeover_fixture',
        'hard_kill_parent_service_recovery_fixture',
        'hard_kill_recovery_failure_fixture',
        'bounded_recovery_elevation_launch_fixture',
        'msi_started_deferred_recovery_fixture',
        'deferred_recovery_idle_race_fixture',
        'recovery_action_fence_fixture',
        'recovery_authority_drain_failure_fixture',
        'native_type_upgrade_compatibility_fixture',
        'account_administrator_mutex_dacl_fixture',
        'Start-BoundlessTrayQuiescenceSentinelOwner',
        'ElevatedInstallCoordinatorProcessId',
        'Get-BoundlessServiceStatusBounded',
        'ElevatedBootstrapRecoveryActionFence',
        'recovery_action_settled',
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
$mutexSecuritySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Test-BoundlessTrayOwnerMutexSecurity'
if (
    $mutexSecuritySource -notmatch 'GetAccessRules' -or
    $mutexSecuritySource -notmatch 'AreAccessRulesProtected' -or
    $mutexSecuritySource -match 'GetSecurityDescriptorSddlForm'
) {
    throw "Tray owner mutex validation must compare protected semantic access rules, not serialized SDDL aliases."
}
$kernelObjectAclSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-BoundlessKernelObjectAclFixture'
$kernelObjectSecuritySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Test-BoundlessProtectedKernelObjectSecurity'
if (
    $kernelObjectAclSource -notmatch 'Test-BoundlessProtectedKernelObjectSecurity' -or
    $kernelObjectAclSource -notmatch 'currentTokenCanUsePrivilegedAcl' -or
    $kernelObjectAclSource -notmatch 'negativeMutationProbeRequired = -not' -or
    $kernelObjectAclSource -notmatch 'ChangePermissions' -or
    $kernelObjectAclSource -notmatch 'Synchronize' -or
    $kernelObjectAclSource -notmatch 'inherited Everyone full control' -or
    $kernelObjectSecuritySource -notmatch '(?s)GetAccessRules\(\s*\$true,\s*\$true,' -or
    $kernelObjectSecuritySource -notmatch 'IsInherited'
) {
    throw "Kernel-object ACL fixtures must always verify semantic rules and retain a real non-admin mutation probe."
}
$stopTraySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Stop-BoundlessTrayForUpgrade'
if ($stopTraySource -match 'Invoke-BoundedProcess' -or $stopTraySource -match '--quit') {
    throw "Boundless-Install.ps1 must not execute a tray image discovered from a user process."
}
$stopwatchIndex = $stopTraySource.IndexOf('[Diagnostics.Stopwatch]::StartNew')
$targetDiscoveryIndex = $stopTraySource.IndexOf('Get-BoundlessTrayProcessesForCurrentSession')
if (
    $stopwatchIndex -lt 0 -or
    $targetDiscoveryIndex -lt 0 -or
    $stopwatchIndex -gt $targetDiscoveryIndex
) {
    throw "Tray shutdown timeout must include target discovery and owner verification."
}
$ownerLookupSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Get-ProcessOwnerSid'
$nativeInstallSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Initialize-BoundlessInstallNativeMethods'
if (
    $ownerLookupSource -match 'Get-CimInstance|Invoke-CimMethod' -or
    $ownerLookupSource -notmatch 'BoundlessInstallNativeMethodsV2' -or
    $nativeInstallSource -notmatch 'GetTokenInformation' -or
    $nativeInstallSource -notmatch 'ConvertSidToStringSidW'
) {
    throw "Tray owner verification must use direct process-token SID lookup without CIM/WMI."
}
if (
    $nativeInstallSource -notmatch 'BoundlessInstallNativeMethodsV2' -or
    $nativeInstallSource -match 'public static class BoundlessInstallNativeMethods\s'
) {
    throw "Installer native methods must use a versioned type that cannot reuse the v5.0.13 class."
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
    $invokeMsiSource -notmatch 'TreeClosureState' -or
    $invokeMsiSource -notmatch 'HardKillRecoveryAction' -or
    $invokeMsiSource -notmatch 'Restore-BoundlessServiceAfterHardKilledElevatedInstall'
) {
    throw "Installer parent must supervise elevation/tree completion, hard-kill recovery, and completed staged-log handoff."
}
$supervisedInstallSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Wait-BoundlessElevatedInstallSupervised'
if (
    $supervisedInstallSource -match 'while\s*\(\$true\)' -or
    $supervisedInstallSource -notmatch 'Parent service recovery after the hard kill also failed'
) {
    throw "Parent hard-kill recovery must make one bounded attempt and preserve its failure with the original supervision error."
}
$parentRecoverySource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Restore-BoundlessServiceAfterHardKilledElevatedInstall'
$recoveryLauncherSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-BoundlessRecoveryLauncherBounded'
$recoveryServiceStartSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Start-BoundlessServiceAfterFailedInstall'
$recoveryRevokeSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Revoke-BoundlessRecoveryAuthorityAndSynchronizeAction'
$normalReleaseSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Test-BoundlessNormalQuiescenceReleaseAllowed'
if (
    $parentRecoverySource -notmatch 'ElevatedBootstrapMsiIdleServiceRecovery' -or
    $parentRecoverySource -notmatch 'msi_definitive_completion_event' -or
    $parentRecoverySource -notmatch 'msi_idle_proven_event' -or
    $parentRecoverySource -notmatch 'ElevatedBootstrapRecoveryActionFence' -or
    $recoveryLauncherSource -notmatch 'WaitForExit\(\$TimeoutMilliseconds\)' -or
    $recoveryLauncherSource -notmatch 'Stop-BoundlessProcessBoundary' -or
    $recoveryServiceStartSource -notmatch 'action_fence\.WaitOne' -or
    $recoveryServiceStartSource -notmatch 'action_committed_event\.Set' -or
    $recoveryServiceStartSource -notmatch 'Wait-BoundlessServiceTransition' -or
    $recoveryRevokeSource -notmatch 'SettlementTimeoutMilliseconds = 35000' -or
    $recoveryRevokeSource -notmatch 'did not settle' -or
    $normalReleaseSource -notmatch 'RecoveryAuthorityDrained' -or
    $normalReleaseSource -notmatch 'RecoveryActionSettled'
) {
    throw "Parent recovery must bound elevation, fence SCM mutation through settlement, and retain fail-closed authority evidence."
}
$elevatedCommandSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'New-BoundlessElevatedInstallCommand'
if (
    $elevatedCommandSource -notmatch 'TerminateJobObject' -or
    $elevatedCommandSource -notmatch 'StartOwned' -or
    $elevatedCommandSource -notmatch 'StartGate' -or
    $elevatedCommandSource -notmatch 'Get-CancellationReason' -or
    $elevatedCommandSource -notmatch 'service_initial_running_event' -or
    $elevatedCommandSource -notmatch 'msi_may_have_started_event' -or
    $elevatedCommandSource -notmatch 'Restore-BootstrapServiceBeforeMsiFailure'
) {
    throw "Elevated installer helper must own gated process, phase-evidence, and pre-MSI recovery boundaries."
}
$msiElevatedSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Invoke-BoundlessMsiElevated'
if (
    $msiElevatedSource -notmatch 'MsiMayHaveStartedEvent\.Reset\(\)' -or
    $msiElevatedSource -notmatch 'cleanup could not be proven' -or
    $msiElevatedSource -notmatch '\$exitCode -notin @\(0, 3010\)'
) {
    throw "Returned non-start and definitive MSI failures must publish bootstrap recovery eligibility."
}
$monitorCommandSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'New-BoundlessTrayQuiescenceMonitorCommand'
if (
    $monitorCommandSource -match 'Get-CimInstance|Invoke-CimMethod' -or
    $monitorCommandSource -notmatch 'GetTokenInformation' -or
    $monitorCommandSource -notmatch 'heartbeat_event_name' -or
    $monitorCommandSource -notmatch 'sentinelReleaseAuthorized' -or
    $monitorCommandSource -notmatch 'Hold-QuiescenceAfterGuardianFailure' -or
    $monitorCommandSource -notmatch '_MSIExecute'
) {
    throw "Tray quiescence monitoring must use direct token identity, heartbeat, and authoritative MSI-idle evidence."
}
$serviceStopSource = Get-PowerShellFunctionSource `
    -Path $installScript `
    -Name 'Stop-BoundlessServiceForUpgrade'
if (
    $serviceStopSource -match '\$service\.Stop\(\)' -or
    $serviceStopSource -notmatch 'Wait-BoundlessServiceTransition' -or
    $serviceStopSource -notmatch 'The MSI was not started' -or
    $serviceStopSource -notmatch 'BoundlessServiceStopInitialStatus' -or
    $serviceStopSource -notmatch '"StartPending"'
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
    $serviceRecoverySource -notmatch 'initial_status -in @\("Running", "StartPending"\)' -or
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

$fixtureHosts = @()
foreach ($hostName in @("powershell.exe", "pwsh.exe")) {
    $hostCommand = Get-Command $hostName -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $hostCommand) { continue }
    $fixtureHosts += $hostName
    if (-not $hostCommand.Source.Equals($script:PowerShellExe, [StringComparison]::OrdinalIgnoreCase)) {
        Invoke-PackagingScript `
            -ScriptPath $installScript `
            -Arguments @("-SelfTest") `
            -PowerShellExe $hostCommand.Source | Out-Null
    }
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

Write-Host "packaging_script_smoke=passed self_tests=$($selfTestScripts.Count) install_fixture_hosts=$($fixtureHosts -join ',') install_resolve_only=passed wix_upgrade_contract=passed helper_upgrade_contract=passed"
