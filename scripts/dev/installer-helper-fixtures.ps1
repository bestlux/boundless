[CmdletBinding()]
param(
    [string]$HelperPath = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($HelperPath)) {
    $HelperPath = Join-Path $repoRoot "packaging\windows\Boundless-Install.ps1"
}
$HelperPath = (Resolve-Path -LiteralPath $HelperPath).Path

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Invoke-ResolveFixture {
    param(
        [string]$Name,
        [string[]]$Arguments,
        [switch]$ShouldFail
    )

    try {
        $output = & powershell -NoProfile -ExecutionPolicy Bypass -File $HelperPath @Arguments -ResolveOnly 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Helper exited with $LASTEXITCODE. Output: $output"
        }

        if ($ShouldFail) {
            throw "Fixture '$Name' was expected to fail but resolved successfully: $output"
        }

        return ($output | ConvertFrom-Json)
    }
    catch {
        if ($ShouldFail) {
            return [pscustomobject]@{
                status = "failed_as_expected"
                error = $_.Exception.Message
            }
        }

        throw "Fixture '$Name' failed unexpectedly. $($_.Exception.Message)"
    }
}

$validSid = "S-1-5-21-1-2-3-1001"
$explicitSid = Invoke-ResolveFixture -Name "explicit_sid" -Arguments @("-AllowedUserSid", $validSid)
if ($explicitSid.selected_user_sid -ne $validSid -or $explicitSid.selected_user_source -ne "explicit_sid") {
    throw "explicit_sid fixture selected the wrong SID or source."
}

$currentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$currentAccount = Invoke-ResolveFixture -Name "explicit_account" -Arguments @("-AllowedUserName", $currentIdentity.Name)
if ($currentAccount.selected_user_sid -ne $currentIdentity.User.Value -or $currentAccount.selected_user_source -ne "explicit_account") {
    throw "explicit_account fixture did not resolve the current account SID."
}

$invalidSid = Invoke-ResolveFixture -Name "invalid_sid" -Arguments @("-AllowedUserSid", "S-1-5-21-not-a-sid") -ShouldFail
if ($invalidSid.status -ne "failed_as_expected") {
    throw "invalid_sid fixture did not fail as expected."
}

$isElevated = Test-IsAdministrator
$currentUserFixture = Invoke-ResolveFixture -Name "current_user_default" -Arguments @() -ShouldFail:$isElevated
if ($isElevated) {
    if ($currentUserFixture.status -ne "failed_as_expected") {
        throw "current_user_default should fail closed in an elevated shell."
    }
}
elseif (
    $currentUserFixture.selected_user_sid -ne $currentIdentity.User.Value -or
    $currentUserFixture.selected_user_source -ne "current_unelevated_user"
) {
    throw "current_user_default should resolve the unelevated current user."
}

if ($isElevated) {
    $explicitElevated = Invoke-ResolveFixture -Name "current_elevated_explicit" -Arguments @("-UseCurrentUserWhenElevated")
    if (
        $explicitElevated.selected_user_sid -ne $currentIdentity.User.Value -or
        $explicitElevated.selected_user_source -ne "current_elevated_user_explicitly_allowed"
    ) {
        throw "current_elevated_explicit did not resolve the current elevated user after explicit opt-in."
    }
}

$summary = [ordered]@{
    helper_path = $HelperPath
    elevated_process = $isElevated
    explicit_sid_fixture = "passed"
    explicit_account_fixture = "passed"
    invalid_sid_fixture = "passed"
    current_user_default_fixture = "passed"
    status = "passed"
}

$summary | ConvertTo-Json -Depth 4
