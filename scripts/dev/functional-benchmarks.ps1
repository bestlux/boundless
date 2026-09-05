[CmdletBinding()]
param(
    [ValidateSet('transport', 'logging', 'ui')][string[]]$Benchmark = @('transport', 'logging', 'ui'),
    [string]$OutputPath = '',
    [string]$TargetDirectory = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) { $PSNativeCommandUseErrorActionPreference = $false }
. (Join-Path $PSScriptRoot 'functional-evidence.ps1')
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $OutputPath) { $OutputPath = Join-Path $repo "artifacts/performance/functional-benchmarks/$(Get-Date -Format 'yyyyMMdd-HHmmss').json" }
$OutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $OutputPath) | Out-Null
if (-not $TargetDirectory) { $TargetDirectory = Join-Path $repo 'artifacts/targets/functional-benchmarks' }
$priorTarget = $env:CARGO_TARGET_DIR
$priorJobs = $env:CARGO_BUILD_JOBS
$measurements = New-Object 'System.Collections.Generic.List[object]'
Push-Location $repo
try {
    $env:CARGO_TARGET_DIR = $TargetDirectory
    $env:CARGO_BUILD_JOBS = '2'
    $executables = @{}
    $groups = @()
    if (@($Benchmark | Where-Object { $_ -in @('transport', 'logging') }).Count -gt 0) { $groups += @{ package = 'boundless-daemon'; target = 'boundless_daemon'; selector = @('--lib') } }
    if ($Benchmark -contains 'ui') { $groups += @{ package = 'boundless-tray'; target = 'boundlesstray'; selector = @('--bin', 'boundlesstray') } }
    foreach ($group in $groups) {
        Write-Host "[functional-benchmarks] compiling locked $($group.package) benchmark executable (2 build jobs)"
        $buildLog = [IO.Path]::ChangeExtension($OutputPath, ".$($group.package).build.log")
        $priorPreference = $ErrorActionPreference
        try {
            $ErrorActionPreference = 'Continue'
            $cargoArgs = @('test', '--locked', '-p', $group.package) + $group.selector + @('--no-run', '--message-format=json')
            $buildOutput = @(& cargo @cargoArgs 2> $buildLog)
            $buildExit = $LASTEXITCODE
        }
        finally { $ErrorActionPreference = $priorPreference }
        if ($buildExit -ne 0) { throw "Benchmark build failed; see $buildLog" }
        $found = @($buildOutput | ForEach-Object {
            try {
                $message = $_ | ConvertFrom-Json
                if ($message.reason -eq 'compiler-artifact' -and $message.profile.test -and $message.executable -and $message.target.name -eq $group.target) { $message.executable }
            }
            catch { }
        } | Sort-Object -Unique)
        if ($found.Count -ne 1) { throw "Cargo did not report exactly one $($group.target) test executable" }
        $executables[$group.target] = $found[0]
    }
    $specs = @{
        transport = @{ target = 'boundless_daemon'; test = 'network::tests::transport_safety_benchmark'; marker = 'boundless_transport_benchmark='; scope = 'actual-runtime-with-injected-connections-and-in-memory-writers' }
        logging = @{ target = 'boundless_daemon'; test = 'logging::bounded::tests::disk_log_budget_benchmark'; marker = 'boundless_log_budget_benchmark='; scope = 'actual-bounded-log-writer-on-local-filesystem' }
        ui = @{ target = 'boundlesstray'; test = 'windows_app::dashboard_render_tests::dashboard_render_cpu_benchmark'; marker = 'BOUNDLESS_UI_BENCHMARK='; scope = 'actual-offscreen-egui-layout-and-tessellation-only' }
    }
    foreach ($name in ($Benchmark | Select-Object -Unique)) {
        $spec = $specs[$name]
        $executable = $executables[$spec.target]
        $binaryHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
        Write-Host "[functional-benchmarks] measuring $name"
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $executable
        $startInfo.Arguments = "$($spec.test) --exact --ignored --nocapture --test-threads=1"
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $uiDirectory = [IO.Path]::ChangeExtension($OutputPath, '.ui')
        if ($name -eq 'ui') { $startInfo.EnvironmentVariables['BOUNDLESS_UI_ARTIFACT_DIR'] = $uiDirectory }
        $process = New-Object System.Diagnostics.Process
        $process.StartInfo = $startInfo
        try {
            if (-not $process.Start()) { throw 'Could not start benchmark process' }
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
            if (-not $process.WaitForExit(120000)) {
                $process.Kill()
                $process.WaitForExit()
                throw "$name benchmark exceeded its 120-second process deadline"
            }
            $output = @(($stdout.GetAwaiter().GetResult() + "`n" + $stderr.GetAwaiter().GetResult()) -split "`r?`n")
            $runExit = $process.ExitCode
        }
        finally { $process.Dispose() }
        $output | Set-Content -LiteralPath ([IO.Path]::ChangeExtension($OutputPath, ".$name.log")) -Encoding utf8
        if ($runExit -ne 0 -or ($output -join "`n") -notmatch 'test result: ok\. 1 passed; 0 failed;') { throw "$name benchmark did not execute exactly one passing test" }
        $metrics = @($output | ForEach-Object {
            $index = $_.IndexOf($spec.marker, [StringComparison]::Ordinal)
            if ($index -ge 0) { $_.Substring($index + $spec.marker.Length) | ConvertFrom-Json }
        })
        $reportHash = $null
        if ($name -eq 'ui') {
            $uiReportPath = Join-Path $uiDirectory 'ui-frame-benchmark.json'
            if ($metrics.Count -ne 1 -or $metrics[0].schema_version -ne 'boundless.ui_frame_benchmark.v1') { throw 'UI benchmark marker is missing or invalid' }
            if ([IO.Path]::GetFullPath([string]$metrics[0].report_path) -ne [IO.Path]::GetFullPath($uiReportPath)) { throw 'UI benchmark reported an unexpected artifact path' }
            if ((Get-Item -LiteralPath $uiReportPath).Length -gt 2097152) { throw 'UI benchmark report exceeds the bounded six-case artifact size' }
            $report = Get-Content -LiteralPath $uiReportPath -Raw | ConvertFrom-Json
            if ([IO.Path]::GetFullPath([string]$report.provenance.test_binary_path) -ne [IO.Path]::GetFullPath($executable)) { throw 'UI report does not identify the executing benchmark binary' }
            if (($report.provenance | ConvertTo-Json -Depth 8 -Compress) -cne ($metrics[0].provenance | ConvertTo-Json -Depth 8 -Compress)) { throw 'UI marker and report provenance disagree' }
            if (@($metrics[0].cases).Count -ne 6) { throw 'UI marker has incomplete case summaries' }
            for ($caseIndex = 0; $caseIndex -lt 6; $caseIndex++) {
                foreach ($field in @('fixture', 'viewport_points', 'pixels_per_point', 'measured_frames', 'summary_ns')) {
                    if (($report.cases[$caseIndex].$field | ConvertTo-Json -Depth 8 -Compress) -cne ($metrics[0].cases[$caseIndex].$field | ConvertTo-Json -Depth 8 -Compress)) { throw "UI marker and report disagree on $field" }
                }
            }
            $metrics = @($report)
            $reportHash = (Get-FileHash -LiteralPath $uiReportPath -Algorithm SHA256).Hash.ToLowerInvariant()
        }
        Assert-FunctionalBenchmarkMetrics -Benchmark $name -Metrics $metrics
        if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash -ine $binaryHash) { throw 'Benchmark executable changed during measurement' }
        $measurements.Add([pscustomobject]@{ benchmark = $name; scope = $spec.scope; test = $spec.test; test_binary_sha256 = $binaryHash; report_sha256 = $reportHash; metrics = $metrics })
    }
    $packet = [pscustomobject]@{
        schema_version = 'boundless.performance.functional_benchmarks.v1'
        generated_at = [DateTimeOffset]::UtcNow.ToString('o')
        source_commit = (& git rev-parse HEAD | Out-String).Trim()
        source_dirty = -not [string]::IsNullOrWhiteSpace((& git status --porcelain | Out-String))
        rustc = (& rustc --version | Out-String).Trim()
        cargo = (& cargo --version | Out-String).Trim()
        platform = [Environment]::OSVersion.VersionString
        machine_label_sha256 = ([BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes([Environment]::MachineName)))).Replace('-', '').ToLowerInvariant()
        machine_identity_scope = 'hashed-hostname-not-physical-attestation'
        role = 'local_benchmark_host'
        process_bitness = if ([Environment]::Is64BitProcess) { 64 } else { 32 }
        build_jobs = 2
        locked_build = $true
        measurements = @($measurements.ToArray())
        physical_two_pc_acceptance = 'not_tested'
    }
    $packet | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $OutputPath -Encoding utf8
    Write-Host "[functional-benchmarks] passed; report=$OutputPath"
}
finally {
    if ($null -eq $priorTarget) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue } else { $env:CARGO_TARGET_DIR = $priorTarget }
    if ($null -eq $priorJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue } else { $env:CARGO_BUILD_JOBS = $priorJobs }
    Pop-Location
}
