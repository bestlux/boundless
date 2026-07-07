param(
    [string]$CliPath = "",
    [string]$ServiceBinaryPath = "",
    [string]$ServiceInstallRoot = "",
    [string]$OutputRoot = "",
    [int]$StopThresholdSeconds = 10,
    [switch]$KeepServiceInstalled
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-IsWindowsPlatform {
    return [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )
}

function Invoke-Cli {
    param(
        [string]$Label,
        [string[]]$Arguments
    )

    Write-Host "[service-smoke] $Label"
    $output = & $CliPath @Arguments 2>&1 | Out-String
    $exitCode = $LASTEXITCODE
    $trimmed = $output.Trim()
    if (-not [string]::IsNullOrWhiteSpace($trimmed)) {
        Write-Host $trimmed
    }
    if ($exitCode -ne 0) {
        throw "$Label failed with exit code $exitCode :: $output"
    }
    return $trimmed
}

function Stop-BoundlessProcesses {
    Get-Process -Name "boundlesstray", "boundlessd", "boundless-service" -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 800
}

function Get-BoundlessServiceProcessId {
    $queryOutput = sc.exe queryex BoundlessService 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        return 0
    }
    if ($queryOutput -match 'PID\s+:\s+(\d+)') {
        return [int]$Matches[1]
    }
    return 0
}

function Remove-BoundlessServiceForCleanup {
    param([string]$Label)

    Write-Host "[service-smoke] $Label"
    $service = Get-Service -Name "BoundlessService" -ErrorAction SilentlyContinue
    if ($null -eq $service) {
        Write-Host "[service-smoke] $Label skipped: service not installed"
        return
    }

    if ($service.Status.ToString() -ne "Stopped") {
        try {
            Stop-Service -Name "BoundlessService" -Force -ErrorAction Stop
        }
        catch {
            Write-Host "[service-smoke] $Label stop request failed: $($_.Exception.Message)"
        }
    }

    try {
        Wait-ServiceState -Name "BoundlessService" -ExpectedStatus "Stopped" -TimeoutSeconds 5 | Out-Null
    }
    catch {
        $pid = Get-BoundlessServiceProcessId
        if ($pid -gt 0) {
            Write-Host "[service-smoke] $Label killing stuck service pid=$pid"
            Stop-Process -Id $pid -Force -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 1
        }
    }

    sc.exe delete BoundlessService | Out-String | ForEach-Object {
        $trimmed = $_.Trim()
        if (-not [string]::IsNullOrWhiteSpace($trimmed)) {
            Write-Host $trimmed
        }
    }
}

function Wait-ServiceState {
    param(
        [string]$Name,
        [string]$ExpectedStatus,
        [int]$TimeoutSeconds = 20
    )

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $service = Get-Service -Name $Name -ErrorAction SilentlyContinue
        if ($null -ne $service -and $service.Status.ToString() -eq $ExpectedStatus) {
            return $service
        }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $deadline)

    if ($null -eq $service) {
        throw "service $Name was not found while waiting for $ExpectedStatus."
    }
    throw "service $Name did not reach $ExpectedStatus within ${TimeoutSeconds}s; current=$($service.Status)."
}

if (-not (Test-IsWindowsPlatform)) {
    throw "service-smoke.ps1 is only supported on Windows."
}
if (-not (Test-IsAdministrator)) {
    throw "service-smoke.ps1 must run from an elevated PowerShell session."
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot "artifacts/service-smoke"
}
$OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null

if ([string]::IsNullOrWhiteSpace($CliPath)) {
    $CliPath = Join-Path $repoRoot "target/release/boundlessctl.exe"
}
if ([string]::IsNullOrWhiteSpace($ServiceBinaryPath)) {
    $ServiceBinaryPath = Join-Path $repoRoot "target/release/boundless-service.exe"
}
if ([string]::IsNullOrWhiteSpace($ServiceInstallRoot)) {
    $ServiceInstallRoot = Join-Path $env:ProgramFiles "Boundless"
}

$CliPath = (Resolve-Path -LiteralPath $CliPath).Path
$ServiceBinaryPath = (Resolve-Path -LiteralPath $ServiceBinaryPath).Path
$ServiceInstallRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ServiceInstallRoot)
$installedServiceBinary = Join-Path $ServiceInstallRoot "boundless-service.exe"

$summaryPath = Join-Path $OutputRoot "service-smoke.json"
$installed = $false
$started = $false

try {
    Stop-BoundlessProcesses
    New-Item -ItemType Directory -Force -Path $ServiceInstallRoot | Out-Null
    Copy-Item -LiteralPath $ServiceBinaryPath -Destination $installedServiceBinary -Force

    Remove-BoundlessServiceForCleanup -Label "service pre-clean"

    $installOutput = Invoke-Cli -Label "service install" -Arguments @(
        "service",
        "install",
        "--binary",
        $installedServiceBinary
    )
    $installed = $true
    if ($installOutput -notmatch "control_pipe_acl=system,administrators,installing_user") {
        throw "service install output did not report reviewed control pipe ACL."
    }

    $startOutput = Invoke-Cli -Label "service start" -Arguments @("service", "start")
    $started = $true
    Wait-ServiceState -Name "BoundlessService" -ExpectedStatus "Running" | Out-Null
    $statusOutput = Invoke-Cli -Label "service status" -Arguments @("service", "status")
    if ($statusOutput -notmatch "installed=true" -or $statusOutput -notmatch "Running") {
        throw "service status did not report Running: $statusOutput"
    }

    $daemonStatusOutput = Invoke-Cli -Label "daemon status through service pipe" -Arguments @("daemon", "status")
    if ($daemonStatusOutput -notmatch "running=true" -or $daemonStatusOutput -notmatch "api_transport=named_pipe") {
        throw "daemon status did not report named-pipe service health: $daemonStatusOutput"
    }

    $stopWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $stopOutput = Invoke-Cli -Label "service stop" -Arguments @("service", "stop")
    Wait-ServiceState -Name "BoundlessService" -ExpectedStatus "Stopped" | Out-Null
    $stopWatch.Stop()
    $stopDurationMs = [int][Math]::Round($stopWatch.Elapsed.TotalMilliseconds)
    Write-Host "[service-smoke] service stop duration ${stopDurationMs}ms threshold=${StopThresholdSeconds}s"
    if ($stopWatch.Elapsed.TotalSeconds -gt $StopThresholdSeconds) {
        throw "service stop exceeded ${StopThresholdSeconds}s threshold: ${stopDurationMs}ms"
    }
    $started = $false
    $stoppedStatusOutput = Invoke-Cli -Label "service status stopped" -Arguments @("service", "status")
    if ($stoppedStatusOutput -notmatch "Stopped") {
        throw "service status did not report Stopped after stop: $stoppedStatusOutput"
    }

    if (-not $KeepServiceInstalled) {
        $uninstallOutput = Invoke-Cli -Label "service uninstall" -Arguments @("service", "uninstall")
        $installed = $false
    }
    else {
        $uninstallOutput = "kept"
    }

    $summary = [ordered]@{
        cli_path = $CliPath
        source_service_binary = $ServiceBinaryPath
        installed_service_binary = $installedServiceBinary
        install_output = $installOutput
        start_output = $startOutput
        running_status_output = $statusOutput
        daemon_status_output = $daemonStatusOutput
        stop_output = $stopOutput
        stop_duration_ms = $stopDurationMs
        stop_threshold_seconds = $StopThresholdSeconds
        stopped_status_output = $stoppedStatusOutput
        uninstall_output = $uninstallOutput
        kept_service_installed = $KeepServiceInstalled.IsPresent
        status = "passed"
    }
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $summaryPath -Encoding utf8
    Write-Host "[service-smoke] passed summary=$summaryPath"
}
finally {
    if (-not $KeepServiceInstalled) {
        if ($installed) {
            Remove-BoundlessServiceForCleanup -Label "cleanup service uninstall"
        }
    }
}
