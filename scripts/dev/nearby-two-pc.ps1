param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("requester", "responder")]
    [string]$Role,

    [Parameter(Mandatory = $true)]
    [ValidateSet("start-daemon", "stop-daemon", "status", "create-code", "pending", "approve", "join", "peers", "show-session", "clipboard-test", "input-test", "edge-test", "latency-report")]
    [string]$Action,

    [string]$RootPath,
    [int]$ApiPort,
    [int]$NetworkPort,
    [string]$Code,
    [string]$ResponderHost,
    [int]$ResponderPairingPort,
    [int]$TimeoutSeconds = 120,
    [int]$WaitSeconds = 45,
    [ValidateSet("left", "right")]
    [string]$PeerSide = "left",
    [string]$RequestId,
    [string]$Message,
    [switch]$Build,
    [switch]$Clean
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$daemonExe = Join-Path $repoRoot "target/debug/boundlessd.exe"
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"
$scriptPath = Join-Path $repoRoot "scripts/dev/nearby-two-pc.ps1"

if (-not $RootPath) {
    $RootPath = Join-Path $env:TEMP "boundless-nearby-two-pc-$Role"
}

if (-not $PSBoundParameters.ContainsKey("ApiPort")) {
    $ApiPort = if ($Role -eq "responder") { 59052 } else { 59051 }
}

if (-not $PSBoundParameters.ContainsKey("NetworkPort")) {
    # Responder default intentionally exercises overflow fallback:
    # pairing listener should bind to 65336 when network_port is 65436.
    $NetworkPort = if ($Role -eq "responder") { 65436 } else { 58100 }
}

$sessionPath = Join-Path $RootPath "session.json"
$stdoutPath = Join-Path $RootPath "daemon.stdout.log"
$stderrPath = Join-Path $RootPath "daemon.stderr.log"
$configPath = Join-Path $RootPath "config.json"
$securityRoot = Join-Path $RootPath "security"
$inboxRoot = Join-Path $RootPath "inbox"

function Get-PairingPort {
    param([int]$TransportPort)

    if ($TransportPort -le (65535 - 100)) {
        return ($TransportPort + 100)
    }

    $fallback = $TransportPort - 100
    if ($fallback -lt 1) {
        return 1
    }

    return $fallback
}

function Load-Session {
    if (-not (Test-Path $sessionPath)) {
        throw "Missing session file: $sessionPath. Run -Action start-daemon first."
    }

    return (Get-Content $sessionPath -Raw | ConvertFrom-Json)
}

function Save-Session {
    param([hashtable]$Session)

    $Session | ConvertTo-Json -Depth 6 | Set-Content -Path $sessionPath -Encoding UTF8
}

function Invoke-Cli {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    & $cliExe "--endpoint" $Endpoint @CommandArgs 2>&1
}

function Invoke-CliChecked {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs $CommandArgs
    if ($LASTEXITCODE -ne 0) {
        throw "CLI failed: endpoint=$Endpoint args='$($CommandArgs -join " ")' exit=$LASTEXITCODE output=$output"
    }

    return $output
}

function Start-DaemonProcess {
    param(
        [string]$Bind,
        [int]$TransportPort
    )

    $envMap = @{
        BOUNDLESS_CONFIG_PATH = $configPath
        BOUNDLESS_SECURITY_ROOT = $securityRoot
        BOUNDLESS_INBOX_ROOT = $inboxRoot
        RUST_LOG = "info"
    }

    $startProcessCommand = Get-Command Start-Process
    if ($startProcessCommand.Parameters.ContainsKey("Environment")) {
        return Start-Process -FilePath $daemonExe `
            -ArgumentList @("--bind", $Bind, "--api-transport", "tcp", "--network-port", "$TransportPort") `
            -PassThru `
            -WindowStyle Hidden `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -Environment $envMap
    }

    $setCommands = @()
    foreach ($entry in $envMap.GetEnumerator()) {
        $setCommands += "set `"$($entry.Key)=$($entry.Value)`""
    }
    $daemonCommand = "`"$daemonExe`" --bind $Bind --api-transport tcp --network-port $TransportPort"
    $commandLine = ($setCommands + $daemonCommand) -join " && "

    return Start-Process -FilePath "cmd.exe" `
        -ArgumentList @("/d", "/s", "/c", $commandLine) `
        -PassThru `
        -WindowStyle Hidden `
        -RedirectStandardOutput $stdoutPath `
        -RedirectStandardError $stderrPath
}

function Wait-ForDaemon {
    param(
        [string]$Endpoint,
        [System.Diagnostics.Process]$Process,
        [int]$Seconds = 30
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("daemon", "status")
        if ($LASTEXITCODE -eq 0 -and $output -match "running=true") {
            return
        }

        if ($Process.HasExited) {
            $stderr = if (Test-Path $stderrPath) { Get-Content $stderrPath -Raw } else { "" }
            throw "Daemon exited before becoming healthy. stderr=$stderr"
        }

        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for daemon at $Endpoint"
}

function Stop-ManagedDaemon {
    $session = Load-Session
    if (-not $session.pid) {
        Write-Host "No pid recorded in session."
        return
    }

    $proc = Get-Process -Id ([int]$session.pid) -ErrorAction SilentlyContinue
    if ($null -eq $proc) {
        Write-Host "Process $($session.pid) is not running."
        return
    }

    Stop-Process -Id $proc.Id -Force
    $proc.WaitForExit(5000) | Out-Null
    Write-Host "Stopped daemon pid=$($proc.Id)"
}

function Get-FirstPeerId {
    param([string]$Endpoint)

    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("peer", "list")
    if ($output -match "no peers configured") {
        throw "No peers configured at endpoint=$Endpoint"
    }

    $match = [regex]::Match($output, "peer_id=([^\s]+)")
    if (-not $match.Success) {
        throw "Unable to parse peer_id from peer list output: $output"
    }

    return $match.Groups[1].Value
}

function Get-TransportEventMatchCount {
    param(
        [string]$Endpoint,
        [string]$Pattern
    )

    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("transport", "events", "--limit", "200")
    return ([regex]::Matches($output, $Pattern)).Count
}

function Wait-ForTransportEventCount {
    param(
        [string]$Endpoint,
        [string]$Pattern,
        [int]$ExpectedMinCount,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $count = Get-TransportEventMatchCount -Endpoint $Endpoint -Pattern $Pattern
        if ($count -ge $ExpectedMinCount) {
            return
        }
        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for transport event count >= $ExpectedMinCount for pattern '$Pattern' at endpoint=$Endpoint"
}

function Wait-ForTransportEventPattern {
    param(
        [string]$Endpoint,
        [string]$Pattern,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("transport", "events", "--limit", "200")
        if ($output -match $Pattern) {
            return
        }
        Start-Sleep -Milliseconds 700
    }

    throw "Timed out waiting for transport event pattern '$Pattern' at endpoint=$Endpoint"
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
        if ($LASTEXITCODE -eq 0 -and $output -match "owner=$ExpectedOwner") {
            return
        }
        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for input owner '$ExpectedOwner' at endpoint=$Endpoint"
}

function Get-PeerRecords {
    param([string]$Endpoint)

    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("peer", "list")
    $text = ($output | Out-String)
    if ($text -match "no peers configured") {
        return @()
    }

    $records = @()
    $lines = $text -split "\r?\n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    foreach ($line in $lines) {
        if ($line -match "^peer_id=(\S+)\s+name=(.+?)\s+address=(\S+)\s+connected=(true|false)$") {
            $records += [pscustomobject]@{
                peer_id = $matches[1]
                name = $matches[2]
                address = $matches[3]
                connected = ($matches[4] -eq "true")
            }
        }
    }

    return $records
}

function Get-ConnectedPeerIds {
    param([string]$Endpoint)

    return @(
        (Get-PeerRecords -Endpoint $Endpoint) |
            Where-Object { $_.connected } |
            ForEach-Object { $_.peer_id }
    )
}

function Get-CaptureTarget {
    param([string]$Endpoint)

    $output = Invoke-CliChecked -Endpoint $Endpoint -CommandArgs @("input", "capture-target")
    $text = ($output | Out-String)
    $match = [regex]::Match($text, "target=([^\s]+)")
    if (-not $match.Success) {
        throw "Unable to parse capture target from output: $text"
    }

    return $match.Groups[1].Value
}

function Wait-ForCaptureTarget {
    param(
        [string]$Endpoint,
        [string]$ExpectedTarget,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $target = Get-CaptureTarget -Endpoint $Endpoint
        if ($target -eq $ExpectedTarget) {
            return
        }
        Start-Sleep -Milliseconds 500
    }

    $finalTarget = Get-CaptureTarget -Endpoint $Endpoint
    throw "Timed out waiting for capture target '$ExpectedTarget' at endpoint=$Endpoint (current='$finalTarget')"
}

function Get-MetricFromDetail {
    param(
        [string]$Detail,
        [string]$MetricName
    )

    $match = [regex]::Match($Detail, "(?:^|\s)$([regex]::Escape($MetricName))=(-?\d+)")
    if (-not $match.Success) {
        return $null
    }

    return [int]$match.Groups[1].Value
}

function Show-LatencyMetricSummary {
    param(
        [string]$Label,
        [System.Collections.IEnumerable]$Values
    )

    $items = @($Values)
    if (-not $items -or $items.Count -eq 0) {
        Write-Host "${Label}: no samples"
        return
    }

    $sorted = @($items | Sort-Object)
    $count = $sorted.Count
    $min = $sorted[0]
    $max = $sorted[$count - 1]
    $avg = [math]::Round((($sorted | Measure-Object -Average).Average), 2)
    $p50 = $sorted[[int][math]::Floor(($count - 1) * 0.50)]
    $p95 = $sorted[[int][math]::Floor(($count - 1) * 0.95)]

    Write-Host "${Label}: n=$count min=$min p50=$p50 p95=$p95 max=$max avg=$avg"
}

switch ($Action) {
    "start-daemon" {
        if ($Build -or -not (Test-Path $daemonExe) -or -not (Test-Path $cliExe)) {
            Write-Host "[nearby-two-pc] building debug binaries"
            cargo build --locked -p boundless-daemon -p boundless-cli | Out-Host
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build --locked failed with exit code $LASTEXITCODE"
            }
        }

        if ($Clean -and (Test-Path $RootPath)) {
            Remove-Item -Path $RootPath -Recurse -Force
        }
        $null = New-Item -ItemType Directory -Force -Path $RootPath, $securityRoot, $inboxRoot

        if (Test-Path $sessionPath) {
            $existing = Load-Session
            if ($existing.pid) {
                $existingProc = Get-Process -Id ([int]$existing.pid) -ErrorAction SilentlyContinue
                if ($existingProc) {
                    throw "Daemon already running with pid=$($existing.pid). Run -Action stop-daemon first."
                }
            }
        }

        $bind = "127.0.0.1:$ApiPort"
        $endpoint = "http://127.0.0.1:$ApiPort"
        $pairingPort = Get-PairingPort -TransportPort $NetworkPort

        $proc = Start-DaemonProcess -Bind $bind -TransportPort $NetworkPort
        Wait-ForDaemon -Endpoint $endpoint -Process $proc -Seconds 30

        Save-Session @{
            role = $Role
            root = $RootPath
            endpoint = $endpoint
            bind = $bind
            network_port = $NetworkPort
            pairing_port = $pairingPort
            pid = $proc.Id
            config_path = $configPath
            security_root = $securityRoot
            inbox_root = $inboxRoot
            stdout_log = $stdoutPath
            stderr_log = $stderrPath
            started_at = (Get-Date).ToString("o")
        }

        Write-Host "Daemon running: role=$Role endpoint=$endpoint pid=$($proc.Id) network_port=$NetworkPort pairing_port=$pairingPort"
        Write-Host "Session file: $sessionPath"
        Write-Host "Stdout log: $stdoutPath"
        Write-Host "Stderr log: $stderrPath"

        if ($Role -eq "responder") {
            Write-Host "Next: & `"$scriptPath`" -Role responder -Action create-code"
        } else {
            Write-Host "Next: run join once you have a code from responder."
        }
        break
    }
    "stop-daemon" {
        Stop-ManagedDaemon
        break
    }
    "status" {
        $session = Load-Session
        Write-Host "Session: $(Get-Content $sessionPath -Raw)"
        $output = Invoke-Cli -Endpoint $session.endpoint -CommandArgs @("daemon", "status")
        $output | Out-Host
        if ($LASTEXITCODE -ne 0) {
            throw "daemon status failed with exit code $LASTEXITCODE"
        }
        break
    }
    "show-session" {
        Write-Host (Get-Content $sessionPath -Raw)
        break
    }
    "create-code" {
        if ($Role -ne "responder") {
            throw "-Action create-code is only valid for responder role."
        }

        $session = Load-Session
        $output = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("pair", "create-code", "--ttl", "120")
        $output | Out-Host
        $match = [regex]::Match($output, "\b\d{6}\b")
        if ($match.Success) {
            Write-Host "Pairing code: $($match.Value)"
        } else {
            Write-Host "Unable to parse code from output."
        }
        break
    }
    "pending" {
        if ($Role -ne "responder") {
            throw "-Action pending is only valid for responder role."
        }

        $session = Load-Session
        $output = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("pair", "pending")
        $output | Out-Host
        break
    }
    "approve" {
        if ($Role -ne "responder") {
            throw "-Action approve is only valid for responder role."
        }

        $session = Load-Session

        if (-not $RequestId) {
            $pending = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("pair", "pending")
            $match = [regex]::Match($pending, "request_id=([^\s]+)")
            if (-not $match.Success) {
                throw "No pending request_id found. pair pending output:`n$pending"
            }
            $RequestId = $match.Groups[1].Value
            Write-Host "Using request_id=$RequestId"
        }

        $output = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("pair", "approve", $RequestId)
        $output | Out-Host
        break
    }
    "join" {
        if ($Role -ne "requester") {
            throw "-Action join is only valid for requester role."
        }
        if (-not $Code) {
            throw "-Code is required for join."
        }
        if (-not $ResponderHost) {
            throw "-ResponderHost is required for join (hostname or IP of responder machine)."
        }

        $session = Load-Session
        $joinPort = if ($PSBoundParameters.ContainsKey("ResponderPairingPort")) {
            $ResponderPairingPort
        } else {
            65336
        }

        Write-Host "Joining responder at ${ResponderHost}:$joinPort with code=$Code"
        $output = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @(
            "pair", "nearby-join", $Code,
            "--host", $ResponderHost,
            "--port", "$joinPort",
            "--timeout-seconds", "$TimeoutSeconds"
        )
        $output | Out-Host
        break
    }
    "peers" {
        $session = Load-Session
        $output = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("peer", "list")
        $output | Out-Host
        break
    }
    "clipboard-test" {
        $session = Load-Session
        $peerId = Get-FirstPeerId -Endpoint $session.endpoint

        if ($Role -eq "requester") {
            $text = if ($Message) {
                $Message
            } else {
                "nearby-clipboard-$((Get-Date).ToString('yyyyMMdd-HHmmss'))"
            }

            $pattern = "direction=outgoing kind=clipboard_text peer_id=$([regex]::Escape($peerId))"
            $before = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $pattern

            Write-Host "Sending clipboard text to peer_id=$peerId"
            Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("transport", "send-text", $peerId, $text) | Out-Host
            Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $pattern -ExpectedMinCount ($before + 1) -Seconds $WaitSeconds
            Write-Host "Clipboard send event observed. message=$text"
            Write-Host "On responder, run:"
            Write-Host "& `"$scriptPath`" -Role responder -Action clipboard-test -Message `"$text`""
            break
        }

        $incomingPattern = "direction=incoming kind=clipboard_text peer_id=$([regex]::Escape($peerId))"
        if ($Message) {
            $escapedMessage = [regex]::Escape($Message)
            $incomingPattern = "$incomingPattern .*detail=$escapedMessage"
            Write-Host "Waiting for clipboard text with message='$Message' from peer_id=$peerId"
            Wait-ForTransportEventPattern -Endpoint $session.endpoint -Pattern $incomingPattern -Seconds $WaitSeconds
        } else {
            $before = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $incomingPattern
            Write-Host "Waiting for next incoming clipboard text from peer_id=$peerId"
            Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $incomingPattern -ExpectedMinCount ($before + 1) -Seconds $WaitSeconds
        }

        Write-Host "Clipboard receive event observed."
        break
    }
    "input-test" {
        $session = Load-Session
        $peerId = Get-FirstPeerId -Endpoint $session.endpoint

        if ($Role -eq "responder") {
            $incomingPattern = "direction=incoming kind=input_frame peer_id=$([regex]::Escape($peerId))"
            $appliedPattern = "direction=local kind=input_inject_applied peer_id=$([regex]::Escape($peerId))"

            $incomingBefore = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $incomingPattern
            $appliedBefore = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $appliedPattern

            Write-Host "Claiming input owner for peer_id=$peerId"
            Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "claim", $peerId) | Out-Host
            Wait-ForInputOwner -Endpoint $session.endpoint -ExpectedOwner $peerId -Seconds 10

            try {
                Write-Host "Waiting up to $WaitSeconds seconds for input frames from requester"
                Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $incomingPattern -ExpectedMinCount ($incomingBefore + 1) -Seconds $WaitSeconds
                Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $appliedPattern -ExpectedMinCount ($appliedBefore + 1) -Seconds $WaitSeconds
                Write-Host "Input receive/apply events observed."
            } finally {
                Write-Host "Releasing input owner for peer_id=$peerId"
                $releaseOutput = Invoke-Cli -Endpoint $session.endpoint -CommandArgs @("input", "release", $peerId)
                if ($LASTEXITCODE -eq 0) {
                    $releaseOutput | Out-Host
                } else {
                    Write-Warning "Failed to release input owner cleanly: $releaseOutput"
                }
            }
            break
        }

        $outgoingPattern = "direction=outgoing kind=input_frame peer_id=$([regex]::Escape($peerId))"
        $before = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $outgoingPattern

        Write-Host "Sending input frames to peer_id=$peerId"
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "send-move", $peerId, "30", "0") | Out-Host
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "send-key", $peerId, "30", "down") | Out-Host
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "send-key", $peerId, "30", "up") | Out-Host

        Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $outgoingPattern -ExpectedMinCount ($before + 1) -Seconds $WaitSeconds
        Write-Host "Outgoing input event observed."
        Write-Host "On responder, run:"
        Write-Host "& `"$scriptPath`" -Role responder -Action input-test"
        break
    }
    "edge-test" {
        if ($Role -ne "requester") {
            throw "-Action edge-test is only valid for requester role."
        }

        $session = Load-Session
        $connectedPeers = @(Get-ConnectedPeerIds -Endpoint $session.endpoint)
        if ($connectedPeers.Count -eq 0) {
            throw "No connected peers available for edge test. Verify pairing and connectivity first."
        }

        Write-Host "Enabling edge-switch related features on requester"
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("feature", "set", "share_input", "on") | Out-Host
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("feature", "set", "easy_mouse", "on") | Out-Host
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("feature", "set", "wrap_mouse", "on") | Out-Host

        if ($connectedPeers.Count -eq 1) {
            $peerId = $connectedPeers[0]
            $layout = if ($PeerSide -eq "left") {
                "$peerId,self"
            } else {
                "self,$peerId"
            }
            $edgeHint = if ($PeerSide -eq "left") { "LEFT" } else { "RIGHT" }

            Write-Host "Single-peer mode: configuring layout '$layout' (peer side: $PeerSide)"
            Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("layout", "set", $layout) | Out-Host

            Write-Host "Starting capture on peer_id=$peerId"
            Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "capture-start", $peerId) | Out-Host
            Wait-ForCaptureTarget -Endpoint $session.endpoint -ExpectedTarget $peerId -Seconds 10

            $outgoingPattern = "direction=outgoing kind=input_frame peer_id=$([regex]::Escape($peerId))"
            $before = Get-TransportEventMatchCount -Endpoint $session.endpoint -Pattern $outgoingPattern

            Write-Host "Move the local mouse aggressively into the $edgeHint screen edge for up to $WaitSeconds seconds."
            Write-Host "This validates edge sampling + capture loop in 2-node mode (target handoff change requires 2+ connected peers)."

            Wait-ForTransportEventCount -Endpoint $session.endpoint -Pattern $outgoingPattern -ExpectedMinCount ($before + 1) -Seconds $WaitSeconds
            Write-Host "Edge capture smoke passed (outgoing input frames observed)."
            break
        }

        $leftPeer = $connectedPeers[0]
        $rightPeer = $connectedPeers[1]
        $layout = "$leftPeer,self,$rightPeer"
        Write-Host "Multi-peer mode: configuring layout '$layout'"
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("layout", "set", $layout) | Out-Host

        Write-Host "Phase 1/2: start capture at left peer and hand off to right peer"
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "capture-start", $leftPeer) | Out-Host
        Wait-ForCaptureTarget -Endpoint $session.endpoint -ExpectedTarget $leftPeer -Seconds 10
        Write-Host "Move cursor to RIGHT screen edge now. Waiting up to $WaitSeconds seconds for target switch..."
        Wait-ForCaptureTarget -Endpoint $session.endpoint -ExpectedTarget $rightPeer -Seconds $WaitSeconds
        Write-Host "Right-edge handoff observed."

        Write-Host "Phase 2/2: start capture at right peer and hand off to left peer"
        Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("input", "capture-start", $rightPeer) | Out-Host
        Wait-ForCaptureTarget -Endpoint $session.endpoint -ExpectedTarget $rightPeer -Seconds 10
        Write-Host "Move cursor to LEFT screen edge now. Waiting up to $WaitSeconds seconds for target switch..."
        Wait-ForCaptureTarget -Endpoint $session.endpoint -ExpectedTarget $leftPeer -Seconds $WaitSeconds
        Write-Host "Left-edge handoff observed."
        Write-Host "Edge handoff test passed."
        break
    }
    "latency-report" {
        $session = Load-Session
        $raw = Invoke-CliChecked -Endpoint $session.endpoint -CommandArgs @("transport", "events", "--limit", "400")
        $text = ($raw | Out-String)
        $lines = $text -split "\r?\n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }

        $captureToSend = New-Object System.Collections.Generic.List[int]
        $captureToReceive = New-Object System.Collections.Generic.List[int]
        $captureToApply = New-Object System.Collections.Generic.List[int]
        $receiveToApply = New-Object System.Collections.Generic.List[int]
        $queueWait = New-Object System.Collections.Generic.List[int]

        foreach ($line in $lines) {
            $eventMatch = [regex]::Match($line, "direction=(\S+)\s+kind=(\S+)\s+peer_id=(\S+)\s+size_bytes=\S+\s+detail=(.*)$")
            if (-not $eventMatch.Success) {
                continue
            }

            $direction = $eventMatch.Groups[1].Value
            $kind = $eventMatch.Groups[2].Value
            $detail = $eventMatch.Groups[4].Value

            if ($direction -eq "outgoing" -and $kind -eq "input_frame") {
                $value = Get-MetricFromDetail -Detail $detail -MetricName "capture_to_send_ms"
                if ($null -ne $value) { [void]$captureToSend.Add($value) }
            }
            if ($direction -eq "incoming" -and $kind -eq "input_frame") {
                $value = Get-MetricFromDetail -Detail $detail -MetricName "capture_to_receive_ms"
                if ($null -ne $value) { [void]$captureToReceive.Add($value) }
            }
            if ($direction -eq "local" -and $kind -eq "input_inject_applied") {
                $v1 = Get-MetricFromDetail -Detail $detail -MetricName "capture_to_apply_ms"
                $v2 = Get-MetricFromDetail -Detail $detail -MetricName "receive_to_apply_ms"
                $v3 = Get-MetricFromDetail -Detail $detail -MetricName "queue_wait_ms"
                if ($null -ne $v1) { [void]$captureToApply.Add($v1) }
                if ($null -ne $v2) { [void]$receiveToApply.Add($v2) }
                if ($null -ne $v3) { [void]$queueWait.Add($v3) }
            }
        }

        Write-Host "Latency summary for endpoint=$($session.endpoint)"
        Show-LatencyMetricSummary -Label "capture_to_send_ms" -Values $captureToSend
        Show-LatencyMetricSummary -Label "capture_to_receive_ms" -Values $captureToReceive
        Show-LatencyMetricSummary -Label "capture_to_apply_ms (end-to-end)" -Values $captureToApply
        Show-LatencyMetricSummary -Label "receive_to_apply_ms (remote host only)" -Values $receiveToApply
        Show-LatencyMetricSummary -Label "queue_wait_ms (remote host only)" -Values $queueWait
        break
    }
}
