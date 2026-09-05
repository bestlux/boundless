[CmdletBinding()]
param(
    [string]$HelperPath = "",
    [switch]$SimulateUnelevatedAuthority
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($HelperPath)) {
    $HelperPath = Join-Path $PSScriptRoot "..\..\packaging\windows\Boundless-Install.ps1"
}
$HelperPath = (Resolve-Path -LiteralPath $HelperPath).Path
$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($HelperPath, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw $parseErrors[0].Message }
# Load exactly the production migration functions without the install entrypoint,
# elevation, service control, or input runtime. All mutations target this fixture.
foreach ($name in @(
    "Test-IsAdministrator", "Test-WindowsPathEqual", "New-BoundlessSecuredDirectoryAtomic",
    "Assert-BoundlessLegacyPlainPath", "Get-BoundlessLegacyRegistrySnapshot",
    "New-BoundlessLegacyInstallPlan", "Wait-BoundlessLegacyProcessesExited",
    "Restore-BoundlessLegacyInstall", "Start-BoundlessLegacyInstallMigration"
)) {
    $function = @($ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true))
    if ($function.Count -ne 1) { throw "Expected exactly one production function: $name" }
    . ([scriptblock]::Create($function[0].Extent.Text))
}
$actualElevated = Test-IsAdministrator
if ($actualElevated -and -not $SimulateUnelevatedAuthority) {
    throw "Run fixtures from a normal shell, or explicitly pass -SimulateUnelevatedAuthority for an elevated CI runner."
}
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$caseId = [guid]::NewGuid().ToString("N")
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) ("boundless-legacy-fixture-" + $caseId)
$registryRoot = "Software\BoundlessLegacyMigrationFixtures\$caseId"

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Expect-Failure {
    param([scriptblock]$Action, [string]$MessagePattern)
    $failed = $false
    try { & $Action | Out-Null }
    catch {
        if ($_.Exception.Message -notmatch $MessagePattern) { throw }
        $failed = $true
    }
    Assert-True $failed "Expected failure matching '$MessagePattern'."
}

function New-FixtureCase {
    param([string]$Name, [switch]$NoRegistration)
    $localRoot = Join-Path $fixtureRoot $Name
    $installRoot = Join-Path $localRoot "Programs\Boundless"
    [void][IO.Directory]::CreateDirectory($installRoot)
    if ($SimulateUnelevatedAuthority) {
        # Elevated CI normally creates administrator-owned directories. This
        # fixture alone models an existing desktop-user-owned legacy root.
        $acl = Get-Acl -LiteralPath $installRoot
        $acl.SetOwner([Security.Principal.SecurityIdentifier]::new($sid))
        Set-Acl -LiteralPath $installRoot -AclObject $acl
    }
    $shortcuts = @((Join-Path $localRoot "startup"), (Join-Path $localRoot "desktop"))
    foreach ($path in $shortcuts) { [void][IO.Directory]::CreateDirectory($path) }
    foreach ($payloadName in @("Boundless-Install.ps1", "Boundless-Uninstall.ps1", "boundlessd.exe", "boundlessctl.exe", "boundlesstray.exe")) {
        [IO.File]::WriteAllText((Join-Path $installRoot $payloadName), "fixture file must never execute")
    }
    [IO.File]::WriteAllText((Join-Path $installRoot "package-manifest.json"), '{"app_id":"boundless","publisher":"Boundless","version":"4.3.1"}')
    [IO.File]::WriteAllText((Join-Path $installRoot "unknown-user-file.txt"), "preserve me")
    $subKey = "$registryRoot\$Name"
    if (-not $NoRegistration) {
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($subKey)
        try {
            $key.SetValue("DisplayName", "Boundless")
            $key.SetValue("Publisher", "Boundless")
            $key.SetValue("DisplayVersion", "4.3.1")
            $key.SetValue("InstallLocation", $installRoot)
            $key.SetValue("UninstallString", ('powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}"' -f (Join-Path $installRoot "Boundless-Uninstall.ps1")))
            $key.SetValue("NoModify", 1, [Microsoft.Win32.RegistryValueKind]::DWord)
        }
        finally { $key.Dispose() }
    }
    return [pscustomobject]@{ local = $localRoot; install = $installRoot; shortcuts = $shortcuts; key = $subKey }
}

function Get-FixturePlan {
    param([object]$Case)
    New-BoundlessLegacyInstallPlan -LocalAppDataPath $Case.local -ShortcutDirectories $Case.shortcuts -UninstallSubKey $Case.key -ExpectedUserSid $sid
}

try {
    [void][IO.Directory]::CreateDirectory($fixtureRoot)
    $authorityCase = New-FixtureCase "authority" -NoRegistration
    if ($actualElevated) {
        Expect-Failure { Get-FixturePlan $authorityCase } 'normal, non-elevated shell'
    }
    else {
        Expect-Failure {
            New-BoundlessLegacyInstallPlan -LocalAppDataPath $authorityCase.local -ShortcutDirectories $authorityCase.shortcuts -UninstallSubKey $authorityCase.key -ExpectedUserSid "S-1-5-18"
        } 'normal, non-elevated shell'
    }
    if ($SimulateUnelevatedAuthority) {
        # No production switch bypasses this check. Only these extracted
        # functions operating on the random fixture roots see the substitute.
        function Test-IsAdministrator { return $false }
    }
    $case = New-FixtureCase "supported"
    $state = Join-Path $case.local "Boundless\security"
    [void][IO.Directory]::CreateDirectory($state)
    $stateFile = Join-Path $state "synthetic-identity.txt"
    [IO.File]::WriteAllText($stateFile, "synthetic identity must stay unchanged")
    $stateHash = (Get-FileHash -LiteralPath $stateFile).Hash
    $logPath = Join-Path $case.install "large-old.log"
    $log = [IO.File]::Create($logPath)
    try { $log.SetLength(16MB) } finally { $log.Dispose() }
    $logHash = (Get-FileHash -LiteralPath $logPath).Hash
    $shell = New-Object -ComObject WScript.Shell
    try {
        $ownedShortcut = Join-Path $case.shortcuts[0] "Boundless.lnk"
        $shortcut = $shell.CreateShortcut($ownedShortcut)
        $shortcut.TargetPath = Join-Path $case.install "boundlesstray.exe"
        $shortcut.Save()
        $otherShortcut = Join-Path $case.shortcuts[1] "Boundless.lnk"
        $shortcut = $shell.CreateShortcut($otherShortcut)
        $shortcut.TargetPath = Join-Path $case.local "different-app.exe"
        $shortcut.Save()
    }
    finally { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($shell) }
    $otherHash = (Get-FileHash -LiteralPath $otherShortcut).Hash
    $ownedHash = (Get-FileHash -LiteralPath $ownedShortcut).Hash
    $registration = Get-BoundlessLegacyRegistrySnapshot -SubKey $case.key | ConvertTo-Json -Depth 8 -Compress
    $plan = Get-FixturePlan $case
    Assert-True ($plan.shortcuts.Count -eq 1) "Only the matching legacy shortcut should be retired."
    $migration = Start-BoundlessLegacyInstallMigration -Plan $plan
    Assert-True (-not (Test-Path -LiteralPath $case.install)) "Legacy executable root remained active."
    Assert-True (-not (Test-Path -LiteralPath $ownedShortcut)) "Legacy autostart remained active."
    Assert-True ($null -eq (Get-BoundlessLegacyRegistrySnapshot -SubKey $case.key)) "Legacy uninstall entry remained active."
    Assert-True ((Get-FileHash -LiteralPath $stateFile).Hash -eq $stateHash) "Current user identity changed."
    Assert-True ((Get-FileHash -LiteralPath $otherShortcut).Hash -eq $otherHash) "Unrelated shortcut changed."
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $migration.backup_root "install\large-old.log")).Hash -eq $logHash) "Archived log bytes changed."
    Assert-True ([IO.File]::ReadAllText((Join-Path $migration.backup_root "install\unknown-user-file.txt")) -eq "preserve me") "Unknown payload file was lost."
    $acl = Get-Acl -LiteralPath $migration.backup_root
    Assert-True $acl.AreAccessRulesProtected "Backup ACL inherited access."
    $allowedSids = @($sid, "S-1-5-18", "S-1-5-32-544")
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        Assert-True ($rule.IdentityReference.Value -in $allowedSids) "Backup grants access to an unexpected principal."
    }
    Assert-True ($null -eq (Get-FixturePlan $case)) "A repeated run should ignore archived payload."
    Restore-BoundlessLegacyInstall -Migration $migration
    Assert-True (Test-Path -LiteralPath $case.install) "Cancelled pre-MSI migration did not restore the install."
    Assert-True ((Get-FileHash -LiteralPath $ownedShortcut).Hash -eq $ownedHash) "Rollback changed the autostart shortcut."
    Assert-True ((Get-BoundlessLegacyRegistrySnapshot -SubKey $case.key | ConvertTo-Json -Depth 8 -Compress) -eq $registration) "Rollback did not restore exact typed registry values."
    Assert-True ((Get-FileHash -LiteralPath $stateFile).Hash -eq $stateHash) "Rollback changed current state."

    $changedShortcutPlan = Get-FixturePlan $case
    [IO.File]::WriteAllText($ownedShortcut, "a concurrent replacement must stay unchanged")
    Expect-Failure { Start-BoundlessLegacyInstallMigration -Plan $changedShortcutPlan } 'shortcut changed'
    Assert-True ([IO.File]::ReadAllText($ownedShortcut) -eq "a concurrent replacement must stay unchanged") "Migration consumed a changed shortcut."
    Assert-True (Test-Path -LiteralPath $case.install) "Changed-shortcut failure retired the install."

    $unrecognized = New-FixtureCase "unrecognized"
    [IO.File]::WriteAllText((Join-Path $unrecognized.install "package-manifest.json"), '{"app_id":"unrelated","publisher":"Boundless","version":"4.3.1"}')
    Expect-Failure { Get-FixturePlan $unrecognized } 'supported Boundless package'
    Assert-True (Test-Path -LiteralPath $unrecognized.install) "Unrecognized install was changed."

    $mismatch = New-FixtureCase "mismatch"
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($mismatch.key, $true)
    try { $key.SetValue("InstallLocation", (Join-Path $fixtureRoot "unrelated")) } finally { $key.Dispose() }
    Expect-Failure { Get-FixturePlan $mismatch } 'does not match'

    $incomplete = New-FixtureCase "incomplete"
    [IO.File]::Delete((Join-Path $incomplete.install "Boundless-Uninstall.ps1"))
    Expect-Failure { Get-FixturePlan $incomplete } 'does not exist|Cannot find path'

    $changed = New-FixtureCase "changed"
    $changedPlan = Get-FixturePlan $changed
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($changed.key, $true)
    try { $key.SetValue("DisplayVersion", "9.9.9") } finally { $key.Dispose() }
    Expect-Failure { Start-BoundlessLegacyInstallMigration -Plan $changedPlan } 'changed during migration'
    Assert-True (Test-Path -LiteralPath $changed.install) "Mid-migration error failed to restore old payload."
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($changed.key)
    try { Assert-True ($key.GetValue("DisplayVersion") -eq "9.9.9") "Rollback overwrote a concurrent registration change." } finally { $key.Dispose() }

    $conflict = New-FixtureCase "rollback-conflict" -NoRegistration
    $conflictMigration = Start-BoundlessLegacyInstallMigration -Plan (Get-FixturePlan $conflict)
    [void][IO.Directory]::CreateDirectory($conflict.install)
    [IO.File]::WriteAllText((Join-Path $conflict.install "new-file.txt"), "new install")
    Expect-Failure { Restore-BoundlessLegacyInstall -Migration $conflictMigration } 'will not overwrite'
    Assert-True ([IO.File]::ReadAllText((Join-Path $conflict.install "new-file.txt")) -eq "new install") "Rollback overwrote replacement files."
    Assert-True (Test-Path -LiteralPath (Join-Path $conflictMigration.backup_root "install")) "Conflict discarded the backup."

    $running = New-FixtureCase "running" -NoRegistration
    $runningExecutable = Join-Path $running.install "boundlessd.exe"
    $systemCmd = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::System)) "cmd.exe"
    [IO.File]::Copy($systemCmd, $runningExecutable, $true)
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $runningExecutable
    $start.Arguments = "/d /q"
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $runningProcess = [Diagnostics.Process]::Start($start)
    try {
        Expect-Failure { Start-BoundlessLegacyInstallMigration -Plan (Get-FixturePlan $running) } 'processes remain active'
        Assert-True (-not $runningProcess.HasExited) "Migration terminated a surviving legacy process."
        Assert-True (Test-Path -LiteralPath $running.install) "Migration moved a running installation."
    }
    finally {
        if (-not $runningProcess.HasExited) {
            $runningProcess.StandardInput.WriteLine("exit")
            if (-not $runningProcess.WaitForExit(1000)) { $runningProcess.Kill(); $runningProcess.WaitForExit() }
        }
        $runningProcess.Dispose()
    }

    $linkCase = New-FixtureCase "junction" -NoRegistration
    $linkTarget = Join-Path $fixtureRoot "junction-target"
    [void][IO.Directory]::CreateDirectory($linkTarget)
    $linkPath = Join-Path $linkCase.local "redirected"
    [void](New-Item -ItemType Junction -Path $linkPath -Target $linkTarget)
    try { Expect-Failure { Assert-BoundlessLegacyPlainPath -Path (Join-Path $linkPath "child") } 'reparse point' }
    finally { [IO.Directory]::Delete($linkPath) }

    [pscustomobject]@{
        status = "passed"
        supported_retirement_and_cancel_restore = "passed"
        current_state_and_unknown_files_preserved = "passed"
        owned_shortcut_and_registration_only = "passed"
        backup_acl_and_idempotence = "passed"
        incomplete_unrecognized_and_custom_registration_rejected = "passed"
        changed_registration_rollback_and_no_overwrite = "passed"
        changed_shortcut_preserved = "passed"
        surviving_legacy_process_not_killed_or_moved = "passed"
        reparse_ancestry_rejected = "passed"
        elevation_or_installed_runtime_exercised = $false
        production_authority_rejection = "passed"
        authority_simulated_for_fixtures = [bool]$SimulateUnelevatedAuthority
        actual_runner_elevated = $actualElevated
        powershell_version = $PSVersionTable.PSVersion.ToString()
    } | ConvertTo-Json
}
finally {
    # Exact randomized fixture roots only; never delete an installed/user root.
    $expected = [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) ("boundless-legacy-fixture-" + $caseId)))
    if ([IO.Path]::GetFullPath($fixtureRoot) -ne $expected -or $registryRoot -ne "Software\BoundlessLegacyMigrationFixtures\$caseId") {
        throw "Fixture cleanup root mismatch."
    }
    if (Test-Path -LiteralPath $fixtureRoot) {
        [void](Assert-BoundlessLegacyPlainPath -Path $fixtureRoot)
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
    }
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($registryRoot, $false)
}
