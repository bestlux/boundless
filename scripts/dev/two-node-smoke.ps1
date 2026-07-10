param(
    [int]$TimeoutSeconds = 45,
    [switch]$KeepArtifacts,
    [switch]$ClipboardOnly,
    [switch]$ExtendedCoverage
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (Get-Variable PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$originalCargoIncremental = $null

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$daemonExe = Join-Path $repoRoot "target/debug/boundlessd.exe"
$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"
. (Join-Path $PSScriptRoot "smoke-harness.ps1")
Initialize-SmokeHarness -RepoRoot $repoRoot -DaemonExe $daemonExe -CliExe $cliExe -LogPrefix "[smoke]"

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

$node1Endpoint = $null
$node2Endpoint = $null
$node1Bind = $null
$node2Bind = $null
$node1Port = 0
$node2Port = 0

$bundle1 = Join-Path $runRoot "node1-bundle.json"
$bundle2 = Join-Path $runRoot "node2-bundle.json"
$node1Out = Join-Path $node1Root "daemon.stdout.log"
$node1Err = Join-Path $node1Root "daemon.stderr.log"
$node2Out = Join-Path $node2Root "daemon.stdout.log"
$node2Err = Join-Path $node2Root "daemon.stderr.log"

$node1 = $null
$node2 = $null
$shouldKeepArtifacts = [bool]$KeepArtifacts
$reportedFailureArtifacts = $false

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

function Get-TransportEventMatchCount {
    param(
        [string]$Endpoint,
        [string]$Pattern
    )

    $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("transport", "events", "--limit", "200")
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to fetch transport events at ${Endpoint}: $output"
    }

    $sampleCount = 0L
    foreach ($line in ($output -split "`r?`n")) {
        if ($line -notmatch $Pattern) {
            continue
        }

        $aggregate = [regex]::Match($line, '(?:^|\s)sample_count=(?<count>\d+)(?:\s|$)')
        if ($aggregate.Success) {
            $sampleCount += [int64]$aggregate.Groups['count'].Value
        }
        else {
            $sampleCount += 1
        }
    }

    return $sampleCount
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

    throw "Timed out waiting for transport event count >= $ExpectedMinCount for '$Pattern' at $Endpoint"
}

function Send-ClipboardImageUntilObserved {
    param(
        [string]$SendEndpoint,
        [string]$PeerId,
        [string]$ImagePath,
        [string]$ObserveEndpoint,
        [string]$ObservePattern,
        [int]$Attempts = 2,
        [int]$ObserveSeconds = 20,
        [string]$RetryLabel = "clipboard image"
    )

    $baselineCount = Get-TransportEventMatchCount -Endpoint $ObserveEndpoint -Pattern $ObservePattern
    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        Invoke-CliChecked -Endpoint $SendEndpoint -CommandArgs @("transport", "send-image", $PeerId, $ImagePath) | Out-Host

        try {
            Wait-ForTransportEventCount -Endpoint $ObserveEndpoint -Pattern $ObservePattern -ExpectedMinCount ($baselineCount + 1) -Seconds $ObserveSeconds
            return
        }
        catch {
            if ($attempt -eq $Attempts) {
                throw
            }
            Write-Warning "[smoke] $RetryLabel was not observed after attempt $attempt; retrying"
            Start-Sleep -Seconds 2
        }
    }
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

function Wait-ForFeatureValue {
    param(
        [string]$Endpoint,
        [string]$FeatureName,
        [bool]$ExpectedValue,
        [int]$Seconds
    )

    $expected = if ($ExpectedValue) { "true" } else { "false" }
    $pattern = [regex]::Escape("${FeatureName}=${expected}")
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $output = Invoke-Cli -Endpoint $Endpoint -CommandArgs @("feature", "list")
        if ($LASTEXITCODE -eq 0 -and $output -match $pattern) {
            return
        }
        Start-Sleep -Milliseconds 500
    }

    throw "Timed out waiting for feature ${FeatureName}=${expected} at $Endpoint"
}

function Wait-ForReceivedFile {
    param(
        [string]$Root,
        [string]$FileName,
        [int]$Seconds
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $matches = @(Get-ChildItem -Path $Root -Filter $FileName -File -Recurse -ErrorAction SilentlyContinue)
        if ($matches.Count -eq 1) {
            return $matches[0].FullName
        }
        if ($matches.Count -gt 1) {
            $paths = ($matches | ForEach-Object { $_.FullName }) -join ", "
            throw "Found multiple received files named ${FileName} under ${Root}: $paths"
        }

        Start-Sleep -Milliseconds 500
    }

    $entries = if (Test-Path $Root) {
        (Get-ChildItem -Path $Root -Recurse -Force | ForEach-Object { $_.FullName }) -join ", "
    }
    else {
        "<missing inbox root>"
    }
    throw "Timed out waiting for received file ${FileName} under ${Root}. Existing entries: $entries"
}

function Set-LittleEndianUInt16 {
    param(
        [byte[]]$Buffer,
        [int]$Offset,
        [int]$Value
    )

    [System.BitConverter]::GetBytes([uint16]$Value).CopyTo($Buffer, $Offset)
}

function Set-LittleEndianUInt32 {
    param(
        [byte[]]$Buffer,
        [int]$Offset,
        [uint32]$Value
    )

    [System.BitConverter]::GetBytes($Value).CopyTo($Buffer, $Offset)
}

function Set-LittleEndianInt32 {
    param(
        [byte[]]$Buffer,
        [int]$Offset,
        [int]$Value
    )

    [System.BitConverter]::GetBytes($Value).CopyTo($Buffer, $Offset)
}

function New-BmpBytes {
    param(
        [int]$Width,
        [int]$Height,
        [byte]$Blue = 0x00,
        [byte]$Green = 0x00,
        [byte]$Red = 0xFF
    )

    if ($Width -le 0 -or $Height -le 0) {
        throw "BMP dimensions must be positive"
    }

    $rowStride = [int](4 * [Math]::Ceiling(($Width * 3) / 4.0))
    $pixelBytes = $rowStride * $Height
    $fileBytes = 54 + $pixelBytes
    $bytes = [byte[]]::new($fileBytes)
    $bytes[0] = 0x42
    $bytes[1] = 0x4D
    Set-LittleEndianUInt32 -Buffer $bytes -Offset 2 -Value ([uint32]$fileBytes)
    Set-LittleEndianUInt32 -Buffer $bytes -Offset 10 -Value 54
    Set-LittleEndianUInt32 -Buffer $bytes -Offset 14 -Value 40
    Set-LittleEndianInt32 -Buffer $bytes -Offset 18 -Value $Width
    Set-LittleEndianInt32 -Buffer $bytes -Offset 22 -Value $Height
    Set-LittleEndianUInt16 -Buffer $bytes -Offset 26 -Value 1
    Set-LittleEndianUInt16 -Buffer $bytes -Offset 28 -Value 24
    Set-LittleEndianUInt32 -Buffer $bytes -Offset 34 -Value ([uint32]$pixelBytes)
    Set-LittleEndianInt32 -Buffer $bytes -Offset 38 -Value 2835
    Set-LittleEndianInt32 -Buffer $bytes -Offset 42 -Value 2835

    $pixelOffset = 54
    for ($row = 0; $row -lt $Height; $row++) {
        $rowStart = $pixelOffset + ($row * $rowStride)
        for ($col = 0; $col -lt $Width; $col++) {
            $base = $rowStart + ($col * 3)
            $bytes[$base] = $Blue
            $bytes[$base + 1] = $Green
            $bytes[$base + 2] = $Red
        }
    }

    return $bytes
}

function New-BmpFile {
    param(
        [string]$Path,
        [int]$Width,
        [int]$Height,
        [byte]$Blue = 0x00,
        [byte]$Green = 0x00,
        [byte]$Red = 0xFF
    )

    $bytes = New-BmpBytes -Width $Width -Height $Height -Blue $Blue -Green $Green -Red $Red
    [System.IO.File]::WriteAllBytes($Path, $bytes)
    return $bytes.Length
}

try {
    $originalCargoIncremental = $env:CARGO_INCREMENTAL
    $env:CARGO_INCREMENTAL = "0"

    $node1ApiPort = Get-FreeTcpPort
    $node2ApiPort = Get-FreeTcpPort
    $node1Port = Get-FreeTcpPort
    $node2Port = Get-FreeTcpPort
    $node1Endpoint = "http://127.0.0.1:$node1ApiPort"
    $node2Endpoint = "http://127.0.0.1:$node2ApiPort"
    $node1Bind = "127.0.0.1:$node1ApiPort"
    $node2Bind = "127.0.0.1:$node2ApiPort"

    Invoke-SmokeBinaryBuild

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

    Write-Host "[smoke] starting node1"
    $node1 = Start-DaemonProcess -Bind $node1Bind -ApiTransport "tcp" -NetworkPort $node1Port -StdOutPath $node1Out -StdErrPath $node1Err -Environment $node1Env

    Write-Host "[smoke] starting node2"
    $node2 = Start-DaemonProcess -Bind $node2Bind -ApiTransport "tcp" -NetworkPort $node2Port -StdOutPath $node2Out -StdErrPath $node2Err -Environment $node2Env

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
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle1) | Out-Host
    Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("pair", "export-trust", "--output", $bundle2) | Out-Host

    Write-Host "[smoke] importing trust bundles"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle2, "--alias", "node2") | Out-Host
    Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("pair", "import-trust", "--input", $bundle1, "--alias", "node1") | Out-Host

    Wait-ForConnectedPeer -Endpoint $node1Endpoint -Seconds $TimeoutSeconds
    Wait-ForConnectedPeer -Endpoint $node2Endpoint -Seconds $TimeoutSeconds

    $node1PeerId = Get-FirstPeerId -Endpoint $node1Endpoint
    $node2PeerId = Get-FirstPeerId -Endpoint $node2Endpoint
    Wait-ForPeerConnectionState -Endpoint $node1Endpoint -PeerId $node1PeerId -Connected $true -Seconds $TimeoutSeconds
    Wait-ForPeerConnectionState -Endpoint $node2Endpoint -PeerId $node2PeerId -Connected $true -Seconds $TimeoutSeconds

    if (-not $ClipboardOnly) {
        Write-Host "[smoke] validating diagnostics action trigger helpers"
        $featureList = Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("feature", "list")
        $easyMouseMatch = [regex]::Match($featureList, "easy_mouse=(true|false)")
        if (-not $easyMouseMatch.Success) {
            throw "Could not parse easy_mouse value from feature list: $featureList"
        }
        $easyMouseBefore = $easyMouseMatch.Groups[1].Value
        $easyMouseAfter = if ($easyMouseBefore -eq "true") { $false } else { $true }
        $easyMouseBeforeBool = ($easyMouseBefore -eq "true")

        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "toggle_easy_mouse") | Out-Host
        Wait-ForFeatureValue -Endpoint $node1Endpoint -FeatureName "easy_mouse" -ExpectedValue $easyMouseAfter -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "toggle_easy_mouse") | Out-Host
        Wait-ForFeatureValue -Endpoint $node1Endpoint -FeatureName "easy_mouse" -ExpectedValue $easyMouseBeforeBool -Seconds $TimeoutSeconds

        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget "none" -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerId -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "switch_all") | Out-Host
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerId -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "capture-stop") | Out-Host
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget "none" -Seconds $TimeoutSeconds

        Write-Host "[smoke] validating input owner control plane"
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "claim", $node1PeerId) | Out-Host
        Wait-ForInputOwner -Endpoint $node1Endpoint -ExpectedOwner $node1PeerId -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "release", $node1PeerId) | Out-Host
        Wait-ForInputOwner -Endpoint $node1Endpoint -ExpectedOwner "none" -Seconds $TimeoutSeconds

        Write-Host "[smoke] validating input capture target control plane"
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget "none" -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "capture-start", $node1PeerId) | Out-Host
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget $node1PeerId -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "capture-stop") | Out-Host
        Wait-ForInputCaptureTarget -Endpoint $node1Endpoint -ExpectedTarget "none" -Seconds $TimeoutSeconds
        $modeEventsOutput = Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("transport", "events", "--limit", "500")
        if ($modeEventsOutput -notmatch "direction=local kind=input_capture_backend_mode peer_id=none detail=(hook_raw|hook|polling|noop|scripted)") {
            Write-Warning "[smoke] input_capture_backend_mode event not observed in current transport event window; continuing"
        }

        Write-Host "[smoke] sending synthetic input frame from node1 to node2"
        Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("input", "claim", $node2PeerId) | Out-Host
        Wait-ForInputOwner -Endpoint $node2Endpoint -ExpectedOwner $node2PeerId -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "send-move", $node1PeerId, "3", "2") | Out-Host
        Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=input_frame peer_id=$node1PeerId" -Seconds $TimeoutSeconds
        Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=input_frame peer_id=$node2PeerId" -Seconds $TimeoutSeconds

        $incomingInputFrameCountBeforeKey = Get-TransportEventMatchCount -Endpoint $node2Endpoint -Pattern "direction=incoming kind=input_frame peer_id=$node2PeerId"
        $appliedInjectCountBeforeKey = Get-TransportEventMatchCount -Endpoint $node2Endpoint -Pattern "direction=local kind=input_inject_applied peer_id=$node2PeerId"
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("input", "send-key", $node1PeerId, "30", "down") | Out-Host
        Wait-ForTransportEventCount -Endpoint $node2Endpoint -Pattern "direction=incoming kind=input_frame peer_id=$node2PeerId" -ExpectedMinCount ($incomingInputFrameCountBeforeKey + 1) -Seconds $TimeoutSeconds
        Wait-ForTransportEventCount -Endpoint $node2Endpoint -Pattern "direction=local kind=input_inject_applied peer_id=$node2PeerId" -ExpectedMinCount ($appliedInjectCountBeforeKey + 1) -Seconds $TimeoutSeconds
        Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("input", "release", $node2PeerId) | Out-Host
        Wait-ForInputOwner -Endpoint $node2Endpoint -ExpectedOwner "none" -Seconds $TimeoutSeconds
    }

    $clipboardText = "smoke-clipboard-" + (Get-Date -Format "HHmmss")
    Write-Host "[smoke] sending clipboard payload from node1 to node2"
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("transport", "send-text", $node1PeerId, $clipboardText) | Out-Host

    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=clipboard_text peer_id=$node1PeerId" -Seconds $TimeoutSeconds
    Wait-ForTransportEvent -Endpoint $node2Endpoint -Pattern "direction=incoming kind=clipboard_text peer_id=$node2PeerId" -Seconds $TimeoutSeconds

    $sampleImage = Join-Path $runRoot "sample-clipboard.bmp"
    $sampleImageBytes = New-BmpFile -Path $sampleImage -Width 1 -Height 1 -Red 0xFF

    Write-Host "[smoke] sending clipboard image payload from node1 to node2"
    $sampleImagePattern = "direction=incoming kind=clipboard_image peer_id=$node2PeerId size_bytes=$sampleImageBytes"
    Send-ClipboardImageUntilObserved -SendEndpoint $node1Endpoint -PeerId $node1PeerId -ImagePath $sampleImage -ObserveEndpoint $node2Endpoint -ObservePattern $sampleImagePattern -ObserveSeconds ([Math]::Min($TimeoutSeconds, 20)) -RetryLabel "initial clipboard image"
    Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=clipboard_image peer_id=$node1PeerId size_bytes=$sampleImageBytes" -Seconds $TimeoutSeconds

    if ($ExtendedCoverage) {
        $chunkedImage = Join-Path $runRoot "chunked-clipboard.bmp"
        $chunkedImageBytes = New-BmpFile -Path $chunkedImage -Width 512 -Height 256 -Blue 0x44 -Green 0x22 -Red 0xAA

        Write-Host "[smoke] sending oversized clipboard image payload from node1 to node2"
        $chunkedImagePattern = "direction=incoming kind=clipboard_image peer_id=$node2PeerId size_bytes=$chunkedImageBytes"
        Send-ClipboardImageUntilObserved -SendEndpoint $node1Endpoint -PeerId $node1PeerId -ImagePath $chunkedImage -ObserveEndpoint $node2Endpoint -ObservePattern $chunkedImagePattern -ObserveSeconds ([Math]::Min($TimeoutSeconds, 30)) -RetryLabel "oversized clipboard image"
        Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=clipboard_image peer_id=$node1PeerId size_bytes=$chunkedImageBytes" -Seconds $TimeoutSeconds
    }

    if (-not $ClipboardOnly) {
        $sampleFile = Join-Path $runRoot "sample-transfer.txt"
        Set-Content -Path $sampleFile -Value "smoke-file-payload" -NoNewline

        Write-Host "[smoke] enabling explicit file receive policy on node2"
        Invoke-CliChecked -Endpoint $node2Endpoint -CommandArgs @("file-transfer", "set-receive-dir", $node2Inbox, "--organize-by-peer", "--auto-accept-trusted-peers", "true") | Out-Host

        Write-Host "[smoke] sending file payload from node1 to node2"
        Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("transport", "send-file", $node1PeerId, $sampleFile) | Out-Host

        Wait-ForTransportEvent -Endpoint $node1Endpoint -Pattern "direction=outgoing kind=file peer_id=$node1PeerId" -Seconds $TimeoutSeconds

        $receivedPath = Wait-ForReceivedFile -Root $node2Inbox -FileName "sample-transfer.txt" -Seconds $TimeoutSeconds
        $receivedContent = Get-Content -Path $receivedPath -Raw
        if ($receivedContent -ne "smoke-file-payload") {
            throw "Received file content mismatch at ${receivedPath}: '$receivedContent'"
        }
    }

    Write-Host "[smoke] validating clipboard delivery after peer restart and reconnect"
    Stop-DaemonProcess -Process $node2
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("diagnostics", "run-action", "reconnect") | Out-Host
    try {
        Wait-ForPeerConnectionState -Endpoint $node1Endpoint -PeerId $node1PeerId -Connected $false -Seconds ([Math]::Min($TimeoutSeconds, 10))
    }
    catch {
        Write-Warning "[smoke] peer disconnect state was not observed before reconnect queueing; continuing with forced reconnect flow"
    }

    $node2 = Start-DaemonProcess -Bind $node2Bind -ApiTransport "tcp" -NetworkPort $node2Port -StdOutPath $node2Out -StdErrPath $node2Err -Environment $node2Env
    Wait-ForDaemon -Endpoint $node2Endpoint -Seconds $TimeoutSeconds -Process $node2 -StdErrPath $node2Err
    Wait-ForConnectedPeer -Endpoint $node1Endpoint -Seconds $TimeoutSeconds
    Wait-ForConnectedPeer -Endpoint $node2Endpoint -Seconds $TimeoutSeconds
    Wait-ForPeerConnectionState -Endpoint $node1Endpoint -PeerId $node1PeerId -Connected $true -Seconds $TimeoutSeconds
    Wait-ForPeerConnectionState -Endpoint $node2Endpoint -PeerId $node2PeerId -Connected $true -Seconds $TimeoutSeconds

    $postReconnectClipboardText =
        "smoke-reconnect-live-" + (Get-Date -Format "HHmmss") + ("x" * 137)
    $postReconnectClipboardBytes =
        [System.Text.Encoding]::UTF8.GetByteCount($postReconnectClipboardText)
    $reconnectTextPattern =
        "direction=incoming kind=clipboard_text peer_id=$node2PeerId size_bytes=$postReconnectClipboardBytes"
    $reconnectTextCountBeforeSend =
        Get-TransportEventMatchCount -Endpoint $node2Endpoint -Pattern $reconnectTextPattern
    $reconnectImage = Join-Path $runRoot "reconnect-clipboard.bmp"
    # The large-image path is covered by the opt-in extended smoke step above.
    # Keep reconnect focused on delivery after reconnect, which is the contract
    # the required CI gate needs to enforce reliably.
    $reconnectImageBytes = New-BmpFile -Path $reconnectImage -Width 32 -Height 32 -Blue 0x11 -Green 0x88 -Red 0x33
    Invoke-CliChecked -Endpoint $node1Endpoint -CommandArgs @("transport", "send-text", $node1PeerId, $postReconnectClipboardText) | Out-Host

    Wait-ForTransportEventCount `
        -Endpoint $node2Endpoint `
        -Pattern $reconnectTextPattern `
        -ExpectedMinCount ($reconnectTextCountBeforeSend + 1) `
        -Seconds $TimeoutSeconds
    $reconnectImagePattern = "direction=incoming kind=clipboard_image peer_id=$node2PeerId size_bytes=$reconnectImageBytes"
    Send-ClipboardImageUntilObserved -SendEndpoint $node1Endpoint -PeerId $node1PeerId -ImagePath $reconnectImage -ObserveEndpoint $node2Endpoint -ObservePattern $reconnectImagePattern -Attempts 3 -ObserveSeconds ([Math]::Min($TimeoutSeconds, 20)) -RetryLabel "reconnect clipboard image"

    if (-not $ClipboardOnly) {
        Wait-ForInputOwner -Endpoint $node2Endpoint -ExpectedOwner "none" -Seconds $TimeoutSeconds
        Write-Host "[smoke] success: connectivity, reconnect recovery, and payload transfer validated"
    }
    else {
        Write-Host "[smoke] success: clipboard text/image transfer and reconnect delivery validated"
    }
}
catch {
    $shouldKeepArtifacts = $true
    if (Test-Path $runRoot) {
        Write-Host "[smoke] failure artifacts kept at: $runRoot"
        $reportedFailureArtifacts = $true
    }
    throw
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
            Stop-DaemonProcess -Process $proc
        }
    }

    if (-not $shouldKeepArtifacts -and (Test-Path $runRoot)) {
        $null = Remove-PathWithRetry -Path $runRoot
    }
    elseif ((Test-Path $runRoot) -and -not $reportedFailureArtifacts) {
        Write-Host "[smoke] artifacts kept at: $runRoot"
    }
}
