param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("requester", "responder")]
    [string]$Role,

    [Parameter(Mandatory = $true)]
    [ValidateSet("start-daemon", "stop-daemon", "status", "create-code", "pending", "approve", "join", "peers", "show-session")]
    [string]$Action,

    [string]$RootPath,
    [int]$ApiPort,
    [int]$NetworkPort,
    [string]$Code,
    [string]$ResponderHost,
    [int]$ResponderPairingPort,
    [int]$TimeoutSeconds = 120,
    [string]$RequestId,
    [switch]$Build,
    [switch]$Clean
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$daemonExe = Join-Path $repoRoot "target/debug/boundlessd.exe"
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"

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

switch ($Action) {
    "start-daemon" {
        if ($Build -or -not (Test-Path $daemonExe) -or -not (Test-Path $cliExe)) {
            Write-Host "[nearby-two-pc] building debug binaries"
            cargo build -p boundless-daemon -p boundless-cli | Out-Host
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build failed with exit code $LASTEXITCODE"
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
            Write-Host "Next: .\\scripts\\dev\\nearby-two-pc.ps1 -Role responder -Action create-code"
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
}
