param(
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "http://10.10.0.149:50051",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [string]$ResponderHost = "10.10.0.149",
    [int]$ResponderPairingPort = 15200,
    [int]$EventsLimit = 300,
    [string]$ScenarioPrefix = "s4_recovery",
    [int]$PendingWaitSeconds = 20,
    [int]$PostExpiryGraceSeconds = 2
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"
$captureScript = Join-Path $repoRoot "scripts/dev/multi-display-capture.ps1"

if (-not (Test-Path $cliExe)) {
    throw "boundlessctl binary not found at $cliExe; run cargo build -p boundlessctl first"
}
if (-not (Test-Path $captureScript)) {
    throw "capture script not found at $captureScript"
}
if ($ResponderPairingPort -le 0 -or $ResponderPairingPort -gt 65535) {
    throw "ResponderPairingPort must be in 1..=65535"
}

function Invoke-Cli {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $output = & $cliExe "--endpoint" $Endpoint @CommandArgs 2>&1 | Out-String
    return [pscustomobject]@{
        ExitCode = $LASTEXITCODE
        Output = $output
    }
}

function Invoke-CliChecked {
    param(
        [string]$Endpoint,
        [string[]]$CommandArgs
    )

    $result = Invoke-Cli -Endpoint $Endpoint -CommandArgs $CommandArgs
    if ($result.ExitCode -ne 0) {
        throw "CLI failed endpoint=$Endpoint args='$($CommandArgs -join " ")' exit=$($result.ExitCode)`n$($result.Output)"
    }
    return $result.Output
}

function Invoke-Capture {
    param(
        [string]$Scenario,
        [ValidateSet("before", "after", "failure")]
        [string]$Phase
    )

    $output = & $captureScript `
        -Scenario $Scenario `
        -Phase $Phase `
        -EndpointA $EndpointA `
        -EndpointB $EndpointB `
        -LabelA $LabelA `
        -LabelB $LabelB `
        -EventsLimit $EventsLimit 2>&1 | Out-String

    $match = [regex]::Match($output, "snapshot written to (?<path>.+)")
    if ($match.Success) {
        return $match.Groups["path"].Value.Trim()
    }
    return "(capture path not parsed)"
}

function Start-PairRequestCode {
    $output = Invoke-CliChecked -Endpoint $EndpointA -CommandArgs @(
        "pair", "request", "target",
        "--host", $ResponderHost,
        "--port", "$ResponderPairingPort"
    )

    $line = ($output -split "\r?\n" | Where-Object { $_ -like "pair_request_code_started=true*" } | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($line)) {
        throw "failed to parse pair request start output:`n$output"
    }

    $match = [regex]::Match(
        $line,
        "request_id=(?<request_id>\S+)\s+verification_nonce=(?<nonce>\S+)\s+expires_at=(?<expires_at>\S+)"
    )
    if (-not $match.Success) {
        throw "failed to parse request id/nonce from line:`n$line"
    }

    return [pscustomobject]@{
        RequestId = $match.Groups["request_id"].Value
        Nonce = $match.Groups["nonce"].Value
        ExpiresAt = $match.Groups["expires_at"].Value
        Raw = $output
    }
}

function Get-PendingRequestOnResponder {
    param([string]$RequestId)

    $output = Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @("pair", "pending")
    if ($output -match "no pending nearby pairing requests") {
        return $null
    }

    $line = ($output -split "\r?\n" | Where-Object { $_ -match "request_id=$([regex]::Escape($RequestId))\b" } | Select-Object -First 1)
    if ([string]::IsNullOrWhiteSpace($line)) {
        return $null
    }

    $codeMatch = [regex]::Match($line, "verification_code=(?<code>\S+)")
    $expiresMatch = [regex]::Match($line, "verification_expires_at=(?<expires>\S+)")

    return [pscustomobject]@{
        RequestId = $RequestId
        Code = if ($codeMatch.Success) { $codeMatch.Groups["code"].Value } else { "" }
        ExpiresAt = if ($expiresMatch.Success) { $expiresMatch.Groups["expires"].Value } else { "" }
        RawLine = $line
    }
}

function Wait-ForPendingRequestOnResponder {
    param(
        [string]$RequestId,
        [int]$TimeoutSeconds
    )

    $deadline = (Get-Date).AddSeconds([Math]::Max(1, $TimeoutSeconds))
    while ((Get-Date) -lt $deadline) {
        $pending = Get-PendingRequestOnResponder -RequestId $RequestId
        if ($null -ne $pending) {
            return $pending
        }
        Start-Sleep -Milliseconds 300
    }

    throw "timed out waiting for pending request_id=$RequestId on responder endpoint=$EndpointB"
}

function Submit-PairRequestCode {
    param(
        [string]$RequestId,
        [string]$Nonce,
        [string]$Code
    )

    return Invoke-Cli -Endpoint $EndpointA -CommandArgs @(
        "pair", "request", "target",
        "--request-id", $RequestId,
        "--nonce", $Nonce,
        "--code", $Code,
        "--host", $ResponderHost,
        "--port", "$ResponderPairingPort"
    )
}

function Wait-UntilAfterExpiration {
    param(
        [string]$ExpiresAt,
        [int]$GraceSeconds
    )

    $expiry = [DateTimeOffset]::Parse($ExpiresAt)
    $now = [DateTimeOffset]::UtcNow
    $waitSeconds = [Math]::Ceiling(($expiry - $now).TotalSeconds) + [Math]::Max(0, $GraceSeconds)
    if ($waitSeconds -gt 0) {
        Start-Sleep -Seconds $waitSeconds
    }
}

Write-Host "[s4-recovery] validating endpoint reachability"
Invoke-CliChecked -Endpoint $EndpointA -CommandArgs @("daemon", "status") | Out-Host
Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @("daemon", "status") | Out-Host

$captures = [ordered]@{}

Write-Host "[s4-recovery] capture before"
$captures["before"] = Invoke-Capture -Scenario "${ScenarioPrefix}_matrix" -Phase "before"

Write-Host "[s4-recovery] reject scenario"
$reject = Start-PairRequestCode
$rejectPending = Wait-ForPendingRequestOnResponder -RequestId $reject.RequestId -TimeoutSeconds $PendingWaitSeconds
Write-Host "[s4-recovery] reject request_id=$($reject.RequestId)"
Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @("pair", "reject", $reject.RequestId) | Out-Host
$rejectSubmit = Submit-PairRequestCode -RequestId $reject.RequestId -Nonce $reject.Nonce -Code "000000"
if ($rejectSubmit.ExitCode -eq 0) {
    throw "reject scenario unexpectedly succeeded for request_id=$($reject.RequestId)`n$($rejectSubmit.Output)"
}
$captures["reject_failure"] = Invoke-Capture -Scenario "${ScenarioPrefix}_reject" -Phase "failure"

Write-Host "[s4-recovery] timeout scenario"
$timeout = Start-PairRequestCode
$timeoutPending = Wait-ForPendingRequestOnResponder -RequestId $timeout.RequestId -TimeoutSeconds $PendingWaitSeconds
if ([string]::IsNullOrWhiteSpace($timeoutPending.Code) -or $timeoutPending.Code -eq "(hidden)") {
    throw "timeout scenario could not read verification code for request_id=$($timeout.RequestId)`n$($timeoutPending.RawLine)"
}
if ([string]::IsNullOrWhiteSpace($timeoutPending.ExpiresAt) -or $timeoutPending.ExpiresAt -eq "(hidden)") {
    throw "timeout scenario could not read verification expiry for request_id=$($timeout.RequestId)`n$($timeoutPending.RawLine)"
}
Write-Host "[s4-recovery] waiting for code expiry request_id=$($timeout.RequestId) expires_at=$($timeoutPending.ExpiresAt)"
Wait-UntilAfterExpiration -ExpiresAt $timeoutPending.ExpiresAt -GraceSeconds $PostExpiryGraceSeconds
$timeoutSubmit = Submit-PairRequestCode -RequestId $timeout.RequestId -Nonce $timeout.Nonce -Code $timeoutPending.Code
if ($timeoutSubmit.ExitCode -eq 0) {
    throw "timeout scenario unexpectedly succeeded for request_id=$($timeout.RequestId)`n$($timeoutSubmit.Output)"
}
$captures["timeout_failure"] = Invoke-Capture -Scenario "${ScenarioPrefix}_timeout" -Phase "failure"

Write-Host "[s4-recovery] success recovery scenario"
$success = Start-PairRequestCode
$successPending = Wait-ForPendingRequestOnResponder -RequestId $success.RequestId -TimeoutSeconds $PendingWaitSeconds
if ([string]::IsNullOrWhiteSpace($successPending.Code) -or $successPending.Code -eq "(hidden)") {
    throw "success scenario could not read verification code for request_id=$($success.RequestId)`n$($successPending.RawLine)"
}
$successSubmit = Submit-PairRequestCode -RequestId $success.RequestId -Nonce $success.Nonce -Code $successPending.Code
if ($successSubmit.ExitCode -ne 0) {
    throw "success scenario failed for request_id=$($success.RequestId)`n$($successSubmit.Output)"
}
$captures["after"] = Invoke-Capture -Scenario "${ScenarioPrefix}_matrix" -Phase "after"

Write-Host "[s4-recovery] diagnostics dump"
$dumpA = Invoke-CliChecked -Endpoint $EndpointA -CommandArgs @(
    "diagnostics", "dump",
    "--output", ".\artifacts\pairing-recovery\${ScenarioPrefix}_a"
)
$dumpB = Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @(
    "diagnostics", "dump",
    "--output", ".\artifacts\pairing-recovery\${ScenarioPrefix}_b"
)

Write-Host "[s4-recovery] complete"
Write-Host "captures.before=$($captures["before"])"
Write-Host "captures.reject_failure=$($captures["reject_failure"])"
Write-Host "captures.timeout_failure=$($captures["timeout_failure"])"
Write-Host "captures.after=$($captures["after"])"
Write-Host ($dumpA.Trim())
Write-Host ($dumpB.Trim())
