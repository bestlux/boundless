Set-StrictMode -Version Latest

$script:SmokeHarnessRepoRoot = ""
$script:SmokeHarnessDaemonExe = ""
$script:SmokeHarnessCliExe = ""
$script:SmokeHarnessLogPrefix = "[smoke]"

function Initialize-SmokeHarness {
    param(
        [string]$RepoRoot,
        [string]$DaemonExe,
        [string]$CliExe,
        [string]$LogPrefix = "[smoke]"
    )

    $script:SmokeHarnessRepoRoot = $RepoRoot
    $script:SmokeHarnessDaemonExe = $DaemonExe
    $script:SmokeHarnessCliExe = $CliExe
    $script:SmokeHarnessLogPrefix = $LogPrefix
}

function Assert-SmokeHarnessInitialized {
    if ([string]::IsNullOrWhiteSpace($script:SmokeHarnessDaemonExe) -or [string]::IsNullOrWhiteSpace($script:SmokeHarnessCliExe)) {
        throw "Smoke harness was not initialized"
    }
}

function Invoke-Cli {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    Assert-SmokeHarnessInitialized
    $allArgs = @("--endpoint", $Endpoint) + $CommandArgs
    & $script:SmokeHarnessCliExe @allArgs 2>&1
}

function Invoke-CliChecked {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs $CommandArgs
    if ($LASTEXITCODE -ne 0) {
        throw "CLI command failed at ${Endpoint}: args='$($CommandArgs -join " ")' exit=$LASTEXITCODE output=$output"
    }

    return $output
}

function Invoke-SmokeBinaryBuild {
    Assert-SmokeHarnessInitialized

    Write-Host "$script:SmokeHarnessLogPrefix building debug binaries"
    cargo build --locked -p boundless-daemon -p boundless-cli | Out-Host
    $buildExitCode = $LASTEXITCODE
    if ($buildExitCode -ne 0) {
        throw "cargo build --locked failed with exit code $buildExitCode"
    }

    if (-not (Test-Path $script:SmokeHarnessDaemonExe) -or -not (Test-Path $script:SmokeHarnessCliExe)) {
        throw "Expected binaries were not built"
    }
}

function Wait-ForDaemon {
    param(
        [string]$Endpoint,
        [int]$Seconds,
        [System.Diagnostics.Process]$Process,
        [string]$StdErrPath
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    $attempt = 0
    while ((Get-Date) -lt $deadline) {
        $attempt++
        try {
            $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("daemon", "status")
            if ($LASTEXITCODE -eq 0 -and $output -match "running=true") {
                return
            }
            if ($attempt -le 5) {
                Write-Host "$script:SmokeHarnessLogPrefix daemon probe $Endpoint attempt=$attempt code=$LASTEXITCODE output=$output"
            }
        }
        catch {
            if ($attempt -le 5) {
                Write-Host "$script:SmokeHarnessLogPrefix daemon probe $Endpoint attempt=$attempt threw: $($_.Exception.Message)"
            }
        }

        Start-Sleep -Milliseconds 500

        if ($Process.HasExited) {
            $stderr = if (Test-Path $StdErrPath) { Get-Content $StdErrPath -Raw } else { "" }
            throw "Daemon at $Endpoint exited early. stderr: $stderr"
        }
    }

    throw "Timed out waiting for daemon at $Endpoint"
}

function Wait-ForConnectedPeerCount {
    param(
        [string]$Endpoint,
        [int]$ExpectedCount,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("peer", "list")
        if ($LASTEXITCODE -eq 0) {
            $count = ([regex]::Matches($output, "connected=true")).Count
            if ($count -ge $ExpectedCount) {
                return
            }
        }

        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for connected peer count >= $ExpectedCount at $Endpoint"
}

function Wait-ForConnectedPeer {
    param(
        [string]$Endpoint,
        [int]$Seconds
    )

    Wait-ForConnectedPeerCount -Endpoint $Endpoint -ExpectedCount 1 -Seconds $Seconds
}

function Wait-ForPeerConnectionState {
    param(
        [string]$Endpoint,
        [string]$PeerId,
        [bool]$Connected,
        [int]$Seconds
    )

    $expected = if ($Connected) { "true" } else { "false" }
    $peerPattern = [regex]::Escape($PeerId)
    $pattern = "peer_id=$peerPattern\s+name=.*\s+address=.*\s+connected=$expected"
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("peer", "list")
        if ($LASTEXITCODE -eq 0 -and $output -match $pattern) {
            return
        }

        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for peer connection state connected=$expected for peer_id=$PeerId at $Endpoint"
}

function Wait-ForInputCaptureTarget {
    param(
        [string]$Endpoint,
        [string]$ExpectedTarget,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("input", "capture-target")
        if ($LASTEXITCODE -eq 0) {
            if ($ExpectedTarget -eq "none" -and $output -match "target=none") {
                return
            }
            if ($ExpectedTarget -ne "none" -and $output -match "target=$ExpectedTarget") {
                return
            }
        }
        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for input capture target '$ExpectedTarget' at $Endpoint"
}

function Start-DaemonProcess {
    param(
        [string]$Bind,
        [int]$NetworkPort,
        [string]$ApiTransport = "tcp",
        [string]$StdOutPath,
        [string]$StdErrPath,
        [hashtable]$Environment
    )

    Assert-SmokeHarnessInitialized
    $startProcessCommand = Get-Command Start-Process
    if ($startProcessCommand.Parameters.ContainsKey("Environment")) {
        return Start-Process -FilePath $script:SmokeHarnessDaemonExe -ArgumentList @("--bind", $Bind, "--api-transport", $ApiTransport, "--network-port", "$NetworkPort") -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdOutPath -RedirectStandardError $StdErrPath -Environment $Environment
    }

    $quote = [char]34
    $setCommands = @()
    foreach ($entry in $Environment.GetEnumerator()) {
        $setCommands += "set $quote$($entry.Key)=$($entry.Value)$quote"
    }

    $daemonCommand = "$quote$script:SmokeHarnessDaemonExe$quote --bind $Bind --api-transport $ApiTransport --network-port $NetworkPort"
    $commandLine = ($setCommands + $daemonCommand) -join " && "

    return Start-Process -FilePath "cmd.exe" -ArgumentList @("/d", "/s", "/c", $commandLine) -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdOutPath -RedirectStandardError $StdErrPath
}

function Stop-DaemonProcess {
    param(
        [System.Diagnostics.Process]$Process,
        [int]$WaitMs = 5000
    )

    if ($null -eq $Process -or $Process.HasExited) {
        return
    }

    Stop-Process -Id $Process.Id -Force
    if (-not $Process.WaitForExit($WaitMs)) {
        throw "Timed out waiting for daemon process $($Process.Id) to stop"
    }
}

function Remove-PathWithRetry {
    param(
        [string]$Path,
        [int]$Attempts = 10,
        [int]$DelayMs = 200
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        try {
            if (Test-Path $Path) {
                Remove-Item -Path $Path -Recurse -Force -ErrorAction Stop
            }
            return $true
        }
        catch {
            if ($attempt -eq $Attempts) {
                Write-Warning "$script:SmokeHarnessLogPrefix failed to remove artifacts at ${Path}: $($_.Exception.Message)"
                return $false
            }
            Start-Sleep -Milliseconds $DelayMs
        }
    }

    return $false
}

function Get-FreeTcpPort {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try {
        return $listener.LocalEndpoint.Port
    }
    finally {
        $listener.Stop()
    }
}
