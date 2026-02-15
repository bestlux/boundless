param(
    [int]$TimeoutSeconds = 45,
    [switch]$KeepArtifacts
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$originalCargoIncremental = $null

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$daemonExe = Join-Path $repoRoot "target/debug/boundlessd.exe"
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"

$runRoot = Join-Path $env:TEMP ("boundless-smoke-" + (Get-Date -Format "yyyyMMdd-HHmmss"))
$node1Root = Join-Path $runRoot "node1"
$node2Root = Join-Path $runRoot "node2"

$null = New-Item -ItemType Directory -Force -Path $node1Root, $node2Root

$node1Config = Join-Path $node1Root "config.json"
$node2Config = Join-Path $node2Root "config.json"
$node1Security = Join-Path $node1Root "security"
$node2Security = Join-Path $node2Root "security"
$node1Inbox = Join-Path $node1Root "inbox"
$node2Inbox = Join-Path $node2Root "inbox"

$node1Endpoint = "http://127.0.0.1:55051"
$node2Endpoint = "http://127.0.0.1:55052"
$node1Bind = "127.0.0.1:55051"
$node2Bind = "127.0.0.1:55052"
$node1Port = 55100
$node2Port = 55101

$bundle1 = Join-Path $runRoot "node1-bundle.json"
$bundle2 = Join-Path $runRoot "node2-bundle.json"
$node1Out = Join-Path $node1Root "daemon.stdout.log"
$node1Err = Join-Path $node1Root "daemon.stderr.log"
$node2Out = Join-Path $node2Root "daemon.stdout.log"
$node2Err = Join-Path $node2Root "daemon.stderr.log"

$node1 = $null
$node2 = $null

function Invoke-Cli {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $allArgs = @("--endpoint", $Endpoint) + $CommandArgs
    & $cliExe @allArgs 2>&1
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
                Write-Host "[smoke] daemon probe $Endpoint attempt=$attempt code=$LASTEXITCODE output=$output"
            }
        }
        catch {
            if ($attempt -le 5) {
                Write-Host "[smoke] daemon probe $Endpoint attempt=$attempt threw: $($_.Exception.Message)"
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

function Wait-ForConnectedPeer {
    param(
        [string]$Endpoint,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("peer", "list")
        if ($LASTEXITCODE -eq 0 -and $output -match "connected=true") {
            return
        }

        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for connected peer at $Endpoint"
}

function Get-FirstPeerId {
    param(
        [string]$Endpoint
    )

    $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("peer", "list")
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to list peers on ${Endpoint}: $output"
    }

    $match = [regex]::Match($output, "peer_id=([^\s]+)")
    if (-not $match.Success) {
        throw "Could not parse peer_id from peer list on ${Endpoint}: $output"
    }

    return $match.Groups[1].Value
}

function Wait-ForTransportEvent {
    param(
        [string]$Endpoint,
        [string]$Pattern,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("transport", "events", "--limit", "200")
        if ($LASTEXITCODE -eq 0 -and $output -match $Pattern) {
            return
        }

        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for transport event '$Pattern' at $Endpoint"
}

function Wait-ForInputOwner {
    param(
        [string]$Endpoint,
        [string]$ExpectedOwner,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("input", "owner")
        if ($LASTEXITCODE -eq 0) {
            if ($ExpectedOwner -eq "none" -and $output -match "owner=none") {
                return
            }
            if ($ExpectedOwner -ne "none" -and $output -match "owner=$ExpectedOwner") {
                return
            }
        }
        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for input owner '$ExpectedOwner' at $Endpoint"
}

function Start-DaemonProcess {
    param(
        [string]$Bind,
        [int]$NetworkPort,
        [string]$ApiTransport,
        [string]$StdOutPath,
        [string]$StdErrPath,
        [hashtable]$Environment
    )

    $startProcessCommand = Get-Command Start-Process
    if ($startProcessCommand.Parameters.ContainsKey("Environment")) {
        return Start-Process -FilePath $daemonExe -ArgumentList @("--bind", $Bind, "--api-transport", $ApiTransport, "--network-port", "$NetworkPort") -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdOutPath -RedirectStandardError $StdErrPath -Environment $Environment
    }

    $setCommands = @()
    foreach ($entry in $Environment.GetEnumerator()) {
        $setCommands += "set `"$($entry.Key)=$($entry.Value)`""
    }

    $daemonCommand = "`"$daemonExe`" --bind $Bind --api-transport $ApiTransport --network-port $NetworkPort"
    $commandLine = ($setCommands + $daemonCommand) -join " && "

    return Start-Process -FilePath "cmd.exe" -ArgumentList @("/d", "/s", "/c", $commandLine) -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdOutPath -RedirectStandardError $StdErrPath
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
                Write-Warning "[smoke] failed to remove artifacts at ${Path}: $($_.Exception.Message)"
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

    Write-Host "[smoke] building debug binaries"
    cargo build -p boundless-daemon -p boundless-cli | Out-Host

    if (-not (Test-Path $daemonExe) -or -not (Test-Path $cliExe)) {
        throw "Expected binaries were not built"
    }

    Write-Host "[smoke] starting node1"
    $node1 = Start-DaemonProcess -Bind $node1Bind -ApiTransport "tcp" -NetworkPort $node1Port -StdOutPath $node1Out -StdErrPath $node1Err -Environment @{
        BOUNDLESS_CONFIG_PATH = $node1Config
        BOUNDLESS_SECURITY_ROOT = $node1Security
        BOUNDLESS_ADVERTISE_HOST = "127.0.0.1"
        BOUNDLESS_INBOX_ROOT = $node1Inbox
    }

    Write-Host "[smoke] starting node2"
    $node2 = Start-DaemonProcess -Bind $node2Bind -ApiTransport "tcp" -NetworkPort $node2Port -StdOutPath $node2Out -StdErrPath $node2Err -Environment @{
        BOUNDLESS_CONFIG_PATH = $node2Config
        BOUNDLESS_SECURITY_ROOT = $node2Security
        BOUNDLESS_ADVERTISE_HOST = "127.0.0.1"
        BOUNDLESS_INBOX_ROOT = $node2Inbox
    }

    Start-Sleep -Milliseconds 500
    if ($node1.HasExited) {
        throw "node1 exited early. stderr: $(Get-Content $node1Err -Raw)"
    }
    if ($node2.HasExited) {
        throw "node2 exited early. stderr: $(Get-Content $node2Err -Raw)"
    }

    Wait-ForDaemon -Endpoint $node1Endpoint -Seconds $TimeoutSeconds -Process $node1 -StdErrPath $node1Err
    Wait-ForDaemon -Endpoint $node2Endpoint -Seconds $TimeoutSeconds -Process $node2 -StdErrPath $node2Err

    Write-Host "[smoke] exporting trust bundles"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle1) | Out-Host
    Invoke-Cli -Endpoint $node2Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle2) | Out-Host

    Write-Host "[smoke] importing trust bundles"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle2, "--alias", "node2") | Out-Host
    Invoke-Cli -Endpoint $node2Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle1, "--alias", "node1") | Out-Host

    Wait-ForConnectedPeer -Endpoint $node1Endpoint -Seconds $TimeoutSeconds
    Wait-ForConnectedPeer -Endpoint $node2Endpoint -Seconds $TimeoutSeconds

    $node1PeerId = Get-FirstPeerId -Endpoint $node1Endpoint
    $node2PeerId = Get-FirstPeerId -Endpoint $node2Endpoint

    Write-Host "[smoke] validating input owner control plane"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("input", "claim", $node1PeerId) | Out-Host
    Wait-ForInputOwner -Endpoint $node1Endpoint -ExpectedOwner $node1PeerId -Seconds $TimeoutSeconds
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("input", "release", $node1PeerId) | Out-Host
    Wait-ForInputOwner -Endpoint $node1Endpoint -ExpectedOwner "none" -Seconds $TimeoutSeconds

    Write-Host "[smoke] sending synthetic input frame from node1 to node2"
    Invoke-Cli -Endpoint $node2Endpoint -CommandArgs @("input", "claim", $node2PeerId) | Out-Host
    Wait-ForInputOwner -Endpoint $node2Endpoint -ExpectedOwner $node2PeerId -Seconds $TimeoutSeconds
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("input", "send-move", $node1PeerId, "3", "2") | Out-Host
    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=input_frame peer_id=$node1PeerId" -Seconds $TimeoutSeconds
    Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=input_frame peer_id=$node2PeerId" -Seconds $TimeoutSeconds
    Invoke-Cli -Endpoint $node2Endpoint -CommandArgs @("input", "release", $node2PeerId) | Out-Host
    Wait-ForInputOwner -Endpoint $node2Endpoint -ExpectedOwner "none" -Seconds $TimeoutSeconds

    $clipboardText = "smoke-clipboard-" + (Get-Date -Format "HHmmss")
    Write-Host "[smoke] sending clipboard payload from node1 to node2"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("transport", "send-text", $node1PeerId, $clipboardText) | Out-Host

    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=clipboard_text peer_id=$node1PeerId" -Seconds $TimeoutSeconds
    Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=clipboard_text peer_id=$node2PeerId" -Seconds $TimeoutSeconds

    $sampleImage = Join-Path $runRoot "sample-clipboard.bmp"
    [byte[]]$bmpBytes = @(
        0x42,0x4D,0x3A,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x36,0x00,0x00,0x00,
        0x28,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x01,0x00,0x18,0x00,
        0x00,0x00,0x00,0x00,0x04,0x00,0x00,0x00,0x13,0x0B,0x00,0x00,0x13,0x0B,0x00,0x00,
        0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
        0x00,0x00,0xFF,0x00
    )
    [System.IO.File]::WriteAllBytes($sampleImage, $bmpBytes)

    Write-Host "[smoke] sending clipboard image payload from node1 to node2"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("transport", "send-image", $node1PeerId, $sampleImage) | Out-Host

    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=clipboard_image peer_id=$node1PeerId" -Seconds $TimeoutSeconds
    Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=clipboard_image peer_id=$node2PeerId" -Seconds $TimeoutSeconds

    $sampleFile = Join-Path $runRoot "sample-transfer.txt"
    Set-Content -Path $sampleFile -Value "smoke-file-payload" -NoNewline

    Write-Host "[smoke] sending file payload from node1 to node2"
    Invoke-Cli -Endpoint $node1Endpoint -CommandArgs @("transport", "send-file", $node1PeerId, $sampleFile) | Out-Host

    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=file peer_id=$node1PeerId" -Seconds $TimeoutSeconds
    Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=file peer_id=$node2PeerId" -Seconds $TimeoutSeconds

    $receivedPath = Join-Path $node2Inbox (Join-Path $node2PeerId "sample-transfer.txt")
    if (-not (Test-Path $receivedPath)) {
        throw "Expected incoming file was not materialized at $receivedPath"
    }

    Write-Host "[smoke] success: peer connectivity and payload transfer validated"
}
finally {
    if ($null -eq $originalCargoIncremental) {
        Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
    }
    else {
        $env:CARGO_INCREMENTAL = $originalCargoIncremental
    }

    foreach ($proc in @($node1, $node2)) {
        if ($null -ne $proc -and -not $proc.HasExited) {
            Stop-Process -Id $proc.Id -Force
        }
    }

    if (-not $KeepArtifacts -and (Test-Path $runRoot)) {
        $null = Remove-PathWithRetry -Path $runRoot
    }
    elseif (Test-Path $runRoot) {
        Write-Host "[smoke] artifacts kept at: $runRoot"
    }
}
