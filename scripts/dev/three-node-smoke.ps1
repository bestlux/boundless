param(
    [int]$TimeoutSeconds = 60,
    [switch]$KeepArtifacts
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$originalCargoIncremental = $null

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$daemonExe = Join-Path $repoRoot "target/debug/boundlessd.exe"
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"

$runRoot = Join-Path $env:TEMP ("boundless-3node-smoke-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
$node1Root = Join-Path $runRoot "node1"
$node2Root = Join-Path $runRoot "node2"
$node3Root = Join-Path $runRoot "node3"
$null = New-Item -ItemType Directory -Force -Path $node1Root, $node2Root, $node3Root

$node1Config = Join-Path $node1Root "config.json"
$node2Config = Join-Path $node2Root "config.json"
$node3Config = Join-Path $node3Root "config.json"
$node1Security = Join-Path $node1Root "security"
$node2Security = Join-Path $node2Root "security"
$node3Security = Join-Path $node3Root "security"
$node1Inbox = Join-Path $node1Root "inbox"
$node2Inbox = Join-Path $node2Root "inbox"
$node3Inbox = Join-Path $node3Root "inbox"

$node1Endpoint = "http://127.0.0.1:56051"
$node2Endpoint = "http://127.0.0.1:56052"
$node3Endpoint = "http://127.0.0.1:56053"
$node1Bind = "127.0.0.1:56051"
$node2Bind = "127.0.0.1:56052"
$node3Bind = "127.0.0.1:56053"
$node1Port = 56100
$node2Port = 56101
$node3Port = 56102

$bundle1 = Join-Path $runRoot "node1-bundle.json"
$bundle2 = Join-Path $runRoot "node2-bundle.json"
$bundle3 = Join-Path $runRoot "node3-bundle.json"
$node1Out = Join-Path $node1Root "daemon.stdout.log"
$node1Err = Join-Path $node1Root "daemon.stderr.log"
$node2Out = Join-Path $node2Root "daemon.stdout.log"
$node2Err = Join-Path $node2Root "daemon.stderr.log"
$node3Out = Join-Path $node3Root "daemon.stdout.log"
$node3Err = Join-Path $node3Root "daemon.stderr.log"

$node1 = $null
$node2 = $null
$node3 = $null

function Invoke-Cli {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $allArgs = @("--endpoint", $Endpoint) + $CommandArgs
    & $cliExe @allArgs 2>&1
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

function Wait-ForDaemon {
    param(
        [string]$Endpoint,
        [int]$Seconds,
        [System.Diagnostics.Process]$Process,
        [string]$StdErrPath
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("daemon", "status")
        if ($LASTEXITCODE -eq 0 -and $output -match "running=true") {
            return
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

function Get-PeerIdByName {
    param(
        [string]$Endpoint,
        [string]$Name
    )

    $pattern = "peer_id=([^\s]+)\s+name=$([regex]::Escape($Name))\s+address=.*\s+connected=(true|false)"
    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("peer", "list")
    $match = [regex]::Match($output, $pattern)
    if (-not $match.Success) {
        throw "Could not find peer named '$Name' at $Endpoint. output=$output"
    }

    return $match.Groups[1].Value
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
        [string]$StdOutPath,
        [string]$StdErrPath,
        [hashtable]$Environment
    )

    $startProcessCommand = Get-Command Start-Process
    if ($startProcessCommand.Parameters.ContainsKey("Environment")) {
        return Start-Process -FilePath $daemonExe -ArgumentList @("--bind", $Bind, "--api-transport", "tcp", "--network-port", "$NetworkPort") -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdOutPath -RedirectStandardError $StdErrPath -Environment $Environment
    }

    $setCommands = @()
    foreach ($entry in $Environment.GetEnumerator()) {
        $setCommands += "set `"$($entry.Key)=$($entry.Value)`""
    }

    $daemonCommand = "`"$daemonExe`" --bind $Bind --api-transport tcp --network-port $NetworkPort"
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
                Write-Warning "[3node-smoke] failed to remove artifacts at ${Path}: $($_.Exception.Message)"
                return $false
            }
            Start-Sleep -Milliseconds $DelayMs
        }
    }

    return $false
}

try {
    $originalCargoIncremental = $env:CARGO_INCREMENTAL
    $env:CARGO_INCREMENTAL = "0"

    Write-Host "[3node-smoke] building debug binaries"
    cargo build -p boundless-daemon -p boundless-cli | Out-Host
    $buildExitCode = $LASTEXITCODE
    if ($buildExitCode -ne 0) {
        throw "cargo build failed with exit code $buildExitCode"
    }
    if (-not (Test-Path $daemonExe) -or -not (Test-Path $cliExe)) {
        throw "Expected binaries were not built"
    }

    $node1Env = @{
        BOUNDLESS_CONFIG_PATH = $node1Config
        BOUNDLESS_SECURITY_ROOT = $node1Security
        BOUNDLESS_ADVERTISE_HOST = "127.0.0.1"
        BOUNDLESS_INBOX_ROOT = $node1Inbox
    }
    $node2Env = @{
        BOUNDLESS_CONFIG_PATH = $node2Config
        BOUNDLESS_SECURITY_ROOT = $node2Security
        BOUNDLESS_ADVERTISE_HOST = "127.0.0.1"
        BOUNDLESS_INBOX_ROOT = $node2Inbox
    }
    $node3Env = @{
        BOUNDLESS_CONFIG_PATH = $node3Config
        BOUNDLESS_SECURITY_ROOT = $node3Security
        BOUNDLESS_ADVERTISE_HOST = "127.0.0.1"
        BOUNDLESS_INBOX_ROOT = $node3Inbox
    }

    Write-Host "[3node-smoke] starting node1/node2/node3"
    $node1 = Start-DaemonProcess -Bind $node1Bind -NetworkPort $node1Port -StdOutPath $node1Out -StdErrPath $node1Err -Environment $node1Env
    $node2 = Start-DaemonProcess -Bind $node2Bind -NetworkPort $node2Port -StdOutPath $node2Out -StdErrPath $node2Err -Environment $node2Env
    $node3 = Start-DaemonProcess -Bind $node3Bind -NetworkPort $node3Port -StdOutPath $node3Out -StdErrPath $node3Err -Environment $node3Env

    Wait-ForDaemon -Endpoint $node1Endpoint -Seconds $TimeoutSeconds -Process $node1 -StdErrPath $node1Err
    Wait-ForDaemon -Endpoint $node2Endpoint -Seconds $TimeoutSeconds -Process $node2 -StdErrPath $node2Err
    Wait-ForDaemon -Endpoint $node3Endpoint -Seconds $TimeoutSeconds -Process $node3 -StdErrPath $node3Err

    Write-Host "[3node-smoke] exporting trust bundles"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle1) | Out-Host
    Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle2) | Out-Host
    Invoke-CliChecked -Endpoint $node3Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle3) | Out-Host

    Write-Host "[3node-smoke] importing trust bundles"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle2, "--alias", "node2") | Out-Host
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle3, "--alias", "node3") | Out-Host
    Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle1, "--alias", "node1") | Out-Host
    Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle3, "--alias", "node3") | Out-Host
    Invoke-CliChecked -Endpoint $node3Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle1, "--alias", "node1") | Out-Host
    Invoke-CliChecked -Endpoint $node3Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle2, "--alias", "node2") | Out-Host

    Wait-ForConnectedPeerCount -Endpoint $node1Endpoint -ExpectedCount 2 -Seconds $TimeoutSeconds
    Wait-ForConnectedPeerCount -Endpoint $node2Endpoint -ExpectedCount 2 -Seconds $TimeoutSeconds
    Wait-ForConnectedPeerCount -Endpoint $node3Endpoint -ExpectedCount 2 -Seconds $TimeoutSeconds

    $node1PeerNode2 = Get-PeerIdByName -Endpoint $node1Endpoint -Name "node2"
    $node1PeerNode3 = Get-PeerIdByName -Endpoint $node1Endpoint -Name "node3"
    $layout = "$node1PeerNode2,self,$node1PeerNode3"
    Write-Host "[3node-smoke] setting node1 layout: $layout"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("layout", "set", $layout) | Out-Host

    Write-Host "[3node-smoke] validating switch_all rotation order across two peers"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "capture-stop") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget "none" -Seconds $TimeoutSeconds
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerNode2 -Seconds $TimeoutSeconds
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerNode3 -Seconds $TimeoutSeconds
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerNode2 -Seconds $TimeoutSeconds

    Write-Host "[3node-smoke] validating disconnected peer is skipped in rotation"
    Stop-DaemonProcess -Process $node3
    Wait-ForPeerConnectionState -Endpoint $node1Endpoint -PeerId $node1PeerNode3 -Connected $false -Seconds $TimeoutSeconds
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerNode2 -Seconds $TimeoutSeconds

    Write-Host "[3node-smoke] restarting node3 and validating rotation recovery"
    $node3 = Start-DaemonProcess -Bind $node3Bind -NetworkPort $node3Port -StdOutPath $node3Out -StdErrPath $node3Err -Environment $node3Env
    Wait-ForDaemon -Endpoint $node3Endpoint -Seconds $TimeoutSeconds -Process $node3 -StdErrPath $node3Err
    Wait-ForConnectedPeerCount -Endpoint $node1Endpoint -ExpectedCount 2 -Seconds $TimeoutSeconds
    Wait-ForPeerConnectionState -Endpoint $node1Endpoint -PeerId $node1PeerNode3 -Connected $true -Seconds $TimeoutSeconds
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
    Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerNode3 -Seconds $TimeoutSeconds

    Write-Host "[3node-smoke] success: 3-node switch_all rotation validated"
}
finally {
    if ($null -eq $originalCargoIncremental) {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_INCREMENTAL = $originalCargoIncremental
    }

    foreach ($proc in @($node1, $node2, $node3)) {
        if ($null -ne $proc -and -not $proc.HasExited) {
            Stop-DaemonProcess -Process $proc
        }
    }

    if (-not $KeepArtifacts -and (Test-Path $runRoot)) {
        $null = Remove-PathWithRetry -Path $runRoot
    }
    elseif (Test-Path $runRoot) {
        Write-Host "[3node-smoke] artifacts kept at: $runRoot"
    }
}
