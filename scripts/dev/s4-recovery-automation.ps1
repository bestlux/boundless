param(
    [string]$EndpointA = "http://127.0.0.1:50051",
    [string]$EndpointB = "http://192.0.2.10:50051",
    [string]$LabelA = "machine-a",
    [string]$LabelB = "machine-b",
    [string]$ResponderHost = "192.0.2.10",
    [int]$ResponderPairingPort = 15200,
    [int]$EventsLimit = 300,
    [string]$ScenarioPrefix = "s4_recovery",
    [int]$PendingWaitSeconds = 20,
    [int]$PostExpiryGraceSeconds = 2,
    [string]$SuccessCode = "",
    [ValidateSet("full", "success-only", "lockout-only", "success-and-lockout")]
    [string]$Mode = "full"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
Set-Location $repoRoot

$cliExe = Join-Path $repoRoot "target/debug/boundlessctl.exe"
$captureScript = Join-Path $repoRoot "scripts/dev/multi-display-capture.ps1"

if (-not (Test-Path $cliExe)) {
    throw "boundlessctl binary not found at $cliExe; run cargo build --locked -p boundlessctl first"
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

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & $cliExe "--endpoint" $Endpoint @CommandArgs 2>&1 | Out-String
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    return [pscustomobject]@{
        ExitCode = $exitCode
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

    $line = ($output -split "\r?\n" | Where-Object { $_ -like "*snapshot written to *" } | Select-Object -Last 1)
    if (-not [string]::IsNullOrWhiteSpace($line)) {
        return ($line -replace ".*snapshot written to ", "").Trim()
    }

    $match = [regex]::Match($output, "snapshot written to (?<path>.+)")
    if ($match.Success) {
        return $match.Groups["path"].Value.Trim()
    }
    return "(capture path not parsed)"
}

function Start-PairRequestCode {
    $output = $null
    $attempt = 0
    $maxAttempts = 8
    while ($attempt -lt $maxAttempts) {
        $attempt += 1
        $result = Invoke-Cli -Endpoint $EndpointA -CommandArgs @(
            "pair", "request", "target",
            "--host", $ResponderHost,
            "--port", "$ResponderPairingPort"
        )
        if ($result.ExitCode -eq 0) {
            $output = $result.Output
            break
        }

        if ($result.Output.ToLowerInvariant().Contains("rate limited")) {
            $delaySeconds = [Math]::Min(2 * $attempt, 8)
            Write-Host "[s4-recovery] request-code rate limited; retrying in ${delaySeconds}s (attempt ${attempt}/${maxAttempts})"
            Start-Sleep -Seconds $delaySeconds
            continue
        }

        throw "CLI failed endpoint=$EndpointA args='pair request target --host $ResponderHost --port $ResponderPairingPort' exit=$($result.ExitCode)`n$($result.Output)"
    }

    if ($null -eq $output) {
        throw "timed out retrying rate-limited request-code start after ${maxAttempts} attempts"
    }

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

function Read-SuccessCodeFromOperator {
    param([string]$RequestId)

    while ($true) {
        Write-Host "[s4-recovery] enter verification code shown on responder (request_id=$RequestId)"
        $value = Read-Host "verification_code"
        $value = $value.Trim()
        if ($value -match "^\d{6}$") {
            return $value
        }
        Write-Host "[s4-recovery] invalid code format; expected exactly 6 digits"
    }
}

function Start-LockoutScenario {
    Write-Host "[s4-recovery] lockout scenario"

    $first = Start-PairRequestCode
    Write-Host "[s4-recovery] lockout pass1 request_id=$($first.RequestId)"
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        $result = Submit-PairRequestCode -RequestId $first.RequestId -Nonce $first.Nonce -Code "000000"
        if ($result.ExitCode -eq 0) {
            throw "lockout scenario pass1 attempt $attempt unexpectedly succeeded"
        }
    }

    Start-Sleep -Seconds 3

    $second = Start-PairRequestCode
    Write-Host "[s4-recovery] lockout pass2 request_id=$($second.RequestId)"
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $result = Submit-PairRequestCode -RequestId $second.RequestId -Nonce $second.Nonce -Code "000000"
        if ($result.ExitCode -eq 0) {
            throw "lockout scenario pass2 attempt $attempt unexpectedly succeeded"
        }
    }

    $lockout = Submit-PairRequestCode -RequestId $second.RequestId -Nonce $second.Nonce -Code "000000"
    if ($lockout.ExitCode -eq 0) {
        throw "lockout scenario expected lockout failure but submission succeeded"
    }
    if (-not $lockout.Output.ToLowerInvariant().Contains("temporarily locked")) {
        throw "lockout scenario expected temporary lockout message`n$($lockout.Output)"
    }
}

Write-Host "[s4-recovery] validating endpoint reachability"
Invoke-CliChecked -Endpoint $EndpointA -CommandArgs @("daemon", "status") | Out-Host
Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @("daemon", "status") | Out-Host

$captures = [ordered]@{}

$runFull = $Mode -eq "full"
$runSuccess = $runFull -or $Mode -eq "success-only" -or $Mode -eq "success-and-lockout"
$runLockout = $Mode -eq "lockout-only" -or $Mode -eq "success-and-lockout"

Write-Host "[s4-recovery] capture before"
$captures["before"] = Invoke-Capture -Scenario "${ScenarioPrefix}_matrix" -Phase "before"

if ($runFull) {
    Write-Host "[s4-recovery] reject scenario"
    $reject = Start-PairRequestCode
    $null = Wait-ForPendingRequestOnResponder -RequestId $reject.RequestId -TimeoutSeconds $PendingWaitSeconds
    Write-Host "[s4-recovery] reject request_id=$($reject.RequestId)"
    Invoke-CliChecked -Endpoint $EndpointB -CommandArgs @("pair", "reject", $reject.RequestId) | Out-Host
    $rejectSubmit = Submit-PairRequestCode -RequestId $reject.RequestId -Nonce $reject.Nonce -Code "000000"
    if ($rejectSubmit.ExitCode -eq 0) {
        throw "reject scenario unexpectedly succeeded for request_id=$($reject.RequestId)`n$($rejectSubmit.Output)"
    }
    $captures["reject_failure"] = Invoke-Capture -Scenario "${ScenarioPrefix}_reject" -Phase "failure"

    Write-Host "[s4-recovery] timeout scenario"
    $timeout = Start-PairRequestCode
    $null = Wait-ForPendingRequestOnResponder -RequestId $timeout.RequestId -TimeoutSeconds $PendingWaitSeconds
    Write-Host "[s4-recovery] waiting for code expiry request_id=$($timeout.RequestId) expires_at=$($timeout.ExpiresAt)"
    Wait-UntilAfterExpiration -ExpiresAt $timeout.ExpiresAt -GraceSeconds $PostExpiryGraceSeconds
    $timeoutSubmit = Submit-PairRequestCode -RequestId $timeout.RequestId -Nonce $timeout.Nonce -Code "000000"
    if ($timeoutSubmit.ExitCode -eq 0) {
        throw "timeout scenario unexpectedly succeeded for request_id=$($timeout.RequestId)`n$($timeoutSubmit.Output)"
    }
    $captures["timeout_failure"] = Invoke-Capture -Scenario "${ScenarioPrefix}_timeout" -Phase "failure"
}

if ($runSuccess) {
    Write-Host "[s4-recovery] success recovery scenario"
    $success = Start-PairRequestCode
    $successPending = Wait-ForPendingRequestOnResponder -RequestId $success.RequestId -TimeoutSeconds $PendingWaitSeconds
    $successCodeToUse = $SuccessCode.Trim()
    if ([string]::IsNullOrWhiteSpace($successCodeToUse)) {
        if ($null -ne $successPending -and -not [string]::IsNullOrWhiteSpace($successPending.Code) -and $successPending.Code -ne "(hidden)") {
            $successCodeToUse = $successPending.Code
        }
    }
    if ([string]::IsNullOrWhiteSpace($successCodeToUse)) {
        $successCodeToUse = Read-SuccessCodeFromOperator -RequestId $success.RequestId
    }

    $successSubmit = Submit-PairRequestCode -RequestId $success.RequestId -Nonce $success.Nonce -Code $successCodeToUse
    if ($successSubmit.ExitCode -ne 0) {
        throw "success scenario failed for request_id=$($success.RequestId)`n$($successSubmit.Output)"
    }
    $captures["after"] = Invoke-Capture -Scenario "${ScenarioPrefix}_matrix" -Phase "after"
}

if ($runLockout) {
    Start-LockoutScenario
    $captures["lockout_failure"] = Invoke-Capture -Scenario "${ScenarioPrefix}_lockout" -Phase "failure"
}

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
foreach ($entry in $captures.GetEnumerator()) {
    Write-Host "captures.$($entry.Key)=$($entry.Value)"
}
Write-Host ($dumpA.Trim())
Write-Host ($dumpB.Trim())
