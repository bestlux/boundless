[CmdletBinding()]
param(
    [long[]]$SizeBytes = @(2MB, 8MB),

    [ValidateSet("noop", "direct-outbound", "local-outbound", "inbound-chunked")]
    [string[]]$Scenario = @("noop", "direct-outbound", "local-outbound", "inbound-chunked"),

    [string]$OutputPath = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutputPath = Join-Path $repoRoot "artifacts/performance/clipboard-image-memory/clipboard-image-memory-$stamp.json"
}
$OutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
$outputDir = Split-Path -Parent $OutputPath
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

$buildLog = [System.IO.Path]::ChangeExtension($OutputPath, ".build.log")
$cargoArgs = @("test", "-p", "boundless-daemon", "--lib", "--no-run", "--message-format", "json")
$previousErrorActionPreference = $ErrorActionPreference
try {
    $ErrorActionPreference = "Continue"
    $cargoOutput = & cargo @cargoArgs 2> $buildLog
    $cargoExitCode = if ($null -eq $global:LASTEXITCODE) { 0 } else { $global:LASTEXITCODE }
}
finally {
    $ErrorActionPreference = $previousErrorActionPreference
}
if ($cargoExitCode -ne 0) {
    throw "cargo test --no-run failed with exit code $cargoExitCode; see $buildLog"
}

$testExecutable = ""
foreach ($line in $cargoOutput) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        continue
    }
    try {
        $message = $line | ConvertFrom-Json
    }
    catch {
        continue
    }

    if ($message.reason -ne "compiler-artifact" -or [string]::IsNullOrWhiteSpace($message.executable)) {
        continue
    }
    if ($message.target.kind -contains "lib" -and $message.package_id -match "boundless-daemon") {
        $testExecutable = $message.executable
    }
}

if ([string]::IsNullOrWhiteSpace($testExecutable) -or -not (Test-Path -LiteralPath $testExecutable)) {
    throw "Unable to locate boundless-daemon lib test executable from cargo JSON output"
}

function Invoke-ProfileRun {
    param(
        [string]$RunScenario,
        [long]$RunSizeBytes
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $testExecutable
    $startInfo.Arguments = "--ignored --nocapture clipboard_image_memory_profile_workload"
    $startInfo.WorkingDirectory = $repoRoot
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.EnvironmentVariables["BOUNDLESS_CLIPBOARD_IMAGE_PROFILE_SCENARIO"] = $RunScenario
    $startInfo.EnvironmentVariables["BOUNDLESS_CLIPBOARD_IMAGE_PROFILE_SIZE_BYTES"] = [string]$RunSizeBytes

    $process = [System.Diagnostics.Process]::Start($startInfo)
    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $peakWorkingSetBytes = 0L
    while (-not $process.HasExited) {
        try {
            $process.Refresh()
            if ($process.WorkingSet64 -gt $peakWorkingSetBytes) {
                $peakWorkingSetBytes = $process.WorkingSet64
            }
        }
        catch {
        }
        Start-Sleep -Milliseconds 5
    }
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    try {
        $process.Refresh()
        if ($process.WorkingSet64 -gt $peakWorkingSetBytes) {
            $peakWorkingSetBytes = $process.WorkingSet64
        }
    }
    catch {
    }
    $stopwatch.Stop()

    return [pscustomobject]@{
        scenario = $RunScenario
        size_bytes = $RunSizeBytes
        peak_working_set_bytes = $peakWorkingSetBytes
        peak_working_set_mib = [Math]::Round($peakWorkingSetBytes / 1MB, 2)
        duration_ms = $stopwatch.ElapsedMilliseconds
        exit_code = $process.ExitCode
        stdout = $stdout.Trim()
        stderr = $stderr.Trim()
    }
}

$results = New-Object System.Collections.Generic.List[object]
foreach ($item in $Scenario) {
    $runSizes = if ($item -eq "noop") { @(0) } else { $SizeBytes }
    foreach ($size in $runSizes) {
        $result = Invoke-ProfileRun -RunScenario $item -RunSizeBytes $size
        $results.Add($result)
        if ($result.exit_code -ne 0) {
            $summary = [pscustomobject]@{
                generated_at = (Get-Date).ToString("o")
                repo_head = (& git rev-parse HEAD).Trim()
                test_executable = $testExecutable
                build_log = $buildLog
                results = $results
            }
            $summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding utf8
            throw "profile run failed for scenario=$item size_bytes=$size; see $OutputPath"
        }
    }
}

$summary = [pscustomobject]@{
    generated_at = (Get-Date).ToString("o")
    repo_head = (& git rev-parse HEAD).Trim()
    command = "scripts/dev/profile-clipboard-image-memory.ps1"
    size_bytes = $SizeBytes
    scenarios = $Scenario
    test_executable = $testExecutable
    build_log = $buildLog
    results = $results
}

$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding utf8
$summary | ConvertTo-Json -Depth 8
