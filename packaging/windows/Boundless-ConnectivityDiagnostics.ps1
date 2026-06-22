param(
    [string]$RemoteHost,

    [ValidateRange(1, 65535)]
    [int[]]$Ports = @(15100, 15101, 15200),

    [ValidateRange(1, 30)]
    [int]$TimeoutSeconds = 4,

    [switch]$Json,

    [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-ProcessSummary {
    param([int]$ProcessId)

    if ($ProcessId -le 0) {
        return [pscustomobject]@{
            pid = $ProcessId
            name = $null
            path = $null
            error = "no owning process"
        }
    }

    try {
        $process = Get-Process -Id $ProcessId -ErrorAction Stop
        return [pscustomobject]@{
            pid = $ProcessId
            name = $process.ProcessName
            path = $process.Path
            error = $null
        }
    } catch {
        return [pscustomobject]@{
            pid = $ProcessId
            name = $null
            path = $null
            error = $_.Exception.Message
        }
    }
}

function Get-LocalListenerReport {
    param([int]$Port)

    $listeners = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)
    $owners = @(
        foreach ($listener in $listeners) {
            $process = Get-ProcessSummary -ProcessId $listener.OwningProcess
            [pscustomobject]@{
                local_address = $listener.LocalAddress
                local_port = $listener.LocalPort
                address_family = if (($listener.LocalAddress -as [string]).Contains(":")) { "ipv6" } else { "ipv4" }
                bind_scope = Get-BindScope -LocalAddress $listener.LocalAddress
                owner_kind = Get-ListenerOwnerKind -Process $process
                owning_process = $process
                mitigation = Get-ListenerMitigation -Port $listener.LocalPort -OwnerKind (Get-ListenerOwnerKind -Process $process)
            }
        }
    )

    return [pscustomobject]@{
        port = $Port
        listening = $owners.Count -gt 0
        listeners = $owners
    }
}

function Get-BindScope {
    param([string]$LocalAddress)

    switch ($LocalAddress) {
        "0.0.0.0" { return "any" }
        "::" { return "any" }
        "127.0.0.1" { return "loopback" }
        "::1" { return "loopback" }
        "" { return "unknown" }
        default { return "specific" }
    }
}

function Get-ListenerOwnerKind {
    param([pscustomobject]$Process)

    $name = if ($null -ne $Process.name) { $Process.name.ToLowerInvariant() } else { "" }
    $path = if ($null -ne $Process.path) { $Process.path.ToLowerInvariant() } else { "" }
    $combined = "$name $path"

    if ($combined.Contains("mousewithoutborders") -or
        $combined.Contains("mouse without borders") -or
        $combined.Contains("powertoys.mousewithoutborders")) {
        return "mouse-without-borders"
    }
    if ($name -eq "boundlessd" -or
        $name -eq "boundless-service" -or
        $name -eq "boundless" -or
        $combined.Contains("boundless-service.exe") -or
        $combined.Contains("boundlessd.exe")) {
        return "boundless"
    }
    if ([string]::IsNullOrWhiteSpace($name) -and [string]::IsNullOrWhiteSpace($path)) {
        return "unknown"
    }
    return "other"
}

function Get-ListenerMitigation {
    param(
        [int]$Port,
        [string]$OwnerKind
    )

    switch ($OwnerKind) {
        "boundless" { return "TCP $Port is owned by Boundless; this is expected when the daemon is running." }
        "mouse-without-borders" { return "Mouse Without Borders or PowerToys is listening on TCP $Port; stop MWB during Boundless dogfood or move Boundless to an alternate network_port before pairing." }
        "other" { return "Another local process is listening on TCP $Port; identify the owner, stop it if appropriate, or move Boundless to an alternate network_port for side-by-side testing." }
        default { return "TCP $Port has a listener but the owning process could not be resolved; inspect the port owner before changing trust or firewall state." }
    }
}

function Get-SideBySideGuidance {
    param([object[]]$LocalListeners)

    $entries = @($LocalListeners | ForEach-Object { $_.listeners } | Where-Object { $null -ne $_ })
    $hasMwb = @($entries | Where-Object { $_.owner_kind -eq "mouse-without-borders" }).Count -gt 0
    $hasOther = @($entries | Where-Object { $_.owner_kind -eq "other" -or $_.owner_kind -eq "unknown" }).Count -gt 0
    $guidance = @()
    if ($hasMwb) {
        $guidance += "Mouse Without Borders/PowerToys listener ownership was detected on a Boundless-related port; stop MWB during Boundless dogfood or configure an alternate Boundless network_port on all participating machines."
    }
    if ($hasOther) {
        $guidance += "A non-Boundless or unresolved listener owns a Boundless-related port; resolve local port ownership before resetting trust or changing firewall policy."
    }
    if ($guidance.Count -gt 0) {
        $guidance += "This script is read-only and did not create firewall rules, elevate, or change network state."
    }
    return $guidance
}

function Test-RemotePort {
    param(
        [string]$ComputerName,
        [int]$Port,
        [int]$TimeoutSeconds
    )

    $client = [System.Net.Sockets.TcpClient]::new()
    try {
        $connect = $client.BeginConnect($ComputerName, $Port, $null, $null)
        $connected = $connect.AsyncWaitHandle.WaitOne([TimeSpan]::FromSeconds($TimeoutSeconds))
        if (-not $connected) {
            return [pscustomobject]@{
                port = $Port
                reachable = $false
                error = "timed out after ${TimeoutSeconds}s"
            }
        }

        $client.EndConnect($connect)
        return [pscustomobject]@{
            port = $Port
            reachable = $true
            error = $null
        }
    } catch {
        return [pscustomobject]@{
            port = $Port
            reachable = $false
            error = $_.Exception.Message
        }
    } finally {
        $client.Dispose()
    }
}

function Get-NetworkProfileReport {
    try {
        return @(Get-NetConnectionProfile -ErrorAction Stop | ForEach-Object {
            [pscustomobject]@{
                name = $_.Name
                interface_alias = $_.InterfaceAlias
                network_category = $_.NetworkCategory.ToString()
                ipv4_connectivity = $_.IPv4Connectivity.ToString()
                ipv6_connectivity = $_.IPv6Connectivity.ToString()
            }
        })
    } catch {
        return @([pscustomobject]@{
            name = $null
            interface_alias = $null
            network_category = $null
            ipv4_connectivity = $null
            ipv6_connectivity = $null
            error = $_.Exception.Message
        })
    }
}

function ConvertTo-StringList {
    param([object]$Value)

    if ($null -eq $Value) {
        return @()
    }
    if ($Value -is [array]) {
        return @($Value | ForEach-Object { "$_" })
    }
    return @("$Value")
}

function Test-ProfileIncludesPrivate {
    param([object]$Profile)

    $values = @(ConvertTo-StringList -Value $Profile)
    return @($values | Where-Object { $_ -eq "Private" }).Count -gt 0
}

function Test-ProfileIsBroadOrPublic {
    param([object]$Profile)

    $values = @(ConvertTo-StringList -Value $Profile)
    return @($values | Where-Object { $_ -eq "Any" -or $_ -eq "Public" }).Count -gt 0
}

function Test-RemoteAddressIsBroad {
    param([object]$RemoteAddress)

    $values = @(ConvertTo-StringList -Value $RemoteAddress)
    if ($values.Count -eq 0) {
        return $true
    }
    return @($values | Where-Object { $_ -in @("Any", "*", "0.0.0.0/0", "::/0", "Internet") }).Count -gt 0
}

function Test-RemoteAddressIsLocalSubnetOrNarrower {
    param([object]$RemoteAddress)

    $values = @(ConvertTo-StringList -Value $RemoteAddress)
    if ($values.Count -eq 0 -or (Test-RemoteAddressIsBroad -RemoteAddress $values)) {
        return $false
    }
    return @($values | Where-Object { $_ -eq "LocalSubnet" }).Count -gt 0
}

function Test-LocalPortIsBroad {
    param([object]$LocalPort)

    $values = @(ConvertTo-StringList -Value $LocalPort)
    if ($values.Count -eq 0) {
        return $true
    }
    return @($values | Where-Object { $_ -in @("Any", "*") }).Count -gt 0
}

function Test-ProtocolIsTcp {
    param([object]$Protocol)

    $values = @(ConvertTo-StringList -Value $Protocol)
    return @($values | Where-Object { $_ -eq "TCP" -or $_ -eq "6" }).Count -gt 0
}

function Test-ProtocolIsBroad {
    param([object]$Protocol)

    $values = @(ConvertTo-StringList -Value $Protocol)
    if ($values.Count -eq 0) {
        return $true
    }
    return @($values | Where-Object { $_ -in @("Any", "*") }).Count -gt 0
}

function Test-PortMatches {
    param(
        [object]$LocalPort,
        [int]$Port
    )

    foreach ($value in @(ConvertTo-StringList -Value $LocalPort)) {
        if ($value -match '^\d+$' -and [int]$value -eq $Port) {
            return $true
        }
        if ($value -match '^(\d+)-(\d+)$') {
            $start = [int]$Matches[1]
            $end = [int]$Matches[2]
            if ($Port -ge $start -and $Port -le $end) {
                return $true
            }
        }
    }
    return $false
}

function Test-ProgramMatches {
    param(
        [string]$Program,
        [string]$ExpectedProgram
    )

    if ([string]::IsNullOrWhiteSpace($Program)) {
        return $false
    }
    return $Program.Trim().Equals($ExpectedProgram, [System.StringComparison]::OrdinalIgnoreCase)
}

function New-FirewallRuleEvidence {
    param(
        [string]$Name,
        [string]$DisplayName,
        [object]$Enabled,
        [object]$Direction,
        [object]$Action,
        [object]$Profile,
        [string]$Program,
        [object]$Protocol,
        [object]$LocalPort,
        [object]$RemoteAddress,
        [string]$ExpectedProgram,
        [int[]]$RequiredPorts
    )

    $coveredPorts = @($RequiredPorts | Where-Object { Test-PortMatches -LocalPort $LocalPort -Port $_ })
    $programMatches = Test-ProgramMatches -Program $Program -ExpectedProgram $ExpectedProgram
    $privateProfile = Test-ProfileIncludesPrivate -Profile $Profile
    $broadProfile = Test-ProfileIsBroadOrPublic -Profile $Profile
    $broadPort = Test-LocalPortIsBroad -LocalPort $LocalPort
    $broadProtocol = Test-ProtocolIsBroad -Protocol $Protocol
    $broadRemote = Test-RemoteAddressIsBroad -RemoteAddress $RemoteAddress
    $tcpProtocol = Test-ProtocolIsTcp -Protocol $Protocol
    $localSubnetOrNarrower = Test-RemoteAddressIsLocalSubnetOrNarrower -RemoteAddress $RemoteAddress

    return [pscustomobject]@{
        name = $Name
        display_name = $DisplayName
        enabled = "$Enabled"
        direction = "$Direction"
        action = "$Action"
        profile = @(ConvertTo-StringList -Value $Profile)
        program = $Program
        program_matches_expected = $programMatches
        protocol = @(ConvertTo-StringList -Value $Protocol)
        local_port = @(ConvertTo-StringList -Value $LocalPort)
        remote_address = @(ConvertTo-StringList -Value $RemoteAddress)
        covers_required_ports = $coveredPorts
        private_profile = $privateProfile
        public_or_any_profile = $broadProfile
        tcp_protocol = $tcpProtocol
        any_or_unspecified_protocol = $broadProtocol
        local_subnet_or_narrower = $localSubnetOrNarrower
        broad_remote_address = $broadRemote
        broad_local_port = $broadPort
        expected_policy_match = (
            $programMatches -and
            "$Enabled" -eq "True" -and
            "$Direction" -eq "Inbound" -and
            "$Action" -eq "Allow" -and
            $privateProfile -and
            -not $broadProfile -and
            $tcpProtocol -and
            -not $broadProtocol -and
            $localSubnetOrNarrower -and
            -not $broadRemote -and
            -not $broadPort
        )
        broad_or_public_pattern = (
            $programMatches -and
            ($broadProfile -or $broadProtocol -or $broadPort -or $broadRemote)
        )
    }
}

function Get-FirewallRuleEvidence {
    param(
        [string]$ExpectedProgram,
        [int[]]$RequiredPorts
    )

    $rules = @(Get-NetFirewallRule -Direction Inbound -Action Allow -Enabled True -ErrorAction Stop)
    $evidence = @()
    foreach ($rule in $rules) {
        $applicationFilters = @($rule | Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue)
        $portFilters = @($rule | Get-NetFirewallPortFilter -ErrorAction SilentlyContinue)
        $addressFilters = @($rule | Get-NetFirewallAddressFilter -ErrorAction SilentlyContinue)
        if ($applicationFilters.Count -eq 0) {
            $applicationFilters = @([pscustomobject]@{ Program = $null })
        }
        if ($portFilters.Count -eq 0) {
            $portFilters = @([pscustomobject]@{ Protocol = $null; LocalPort = $null })
        }
        if ($addressFilters.Count -eq 0) {
            $addressFilters = @([pscustomobject]@{ RemoteAddress = $null })
        }

        foreach ($app in $applicationFilters) {
            foreach ($port in $portFilters) {
                foreach ($address in $addressFilters) {
                    if (-not (Test-ProgramMatches -Program $app.Program -ExpectedProgram $ExpectedProgram)) {
                        continue
                    }
                    $evidence += New-FirewallRuleEvidence -Name $rule.Name -DisplayName $rule.DisplayName -Enabled $rule.Enabled -Direction $rule.Direction -Action $rule.Action -Profile $rule.Profile -Program $app.Program -Protocol $port.Protocol -LocalPort $port.LocalPort -RemoteAddress $address.RemoteAddress -ExpectedProgram $ExpectedProgram -RequiredPorts $RequiredPorts
                }
            }
        }
    }
    return $evidence
}

function Get-FirewallPolicyReport {
    param(
        [string]$ServiceExe,
        [object[]]$RuleEvidence,
        [string]$ErrorMessage
    )

    $requiredFirewallPorts = @(15100, 15200)
    $portReports = @(
        foreach ($port in $requiredFirewallPorts) {
            $matches = @($RuleEvidence | Where-Object {
                $_.expected_policy_match -and
                @($_.covers_required_ports).Contains($port)
            })
            [pscustomobject]@{
                port = $port
                required = $true
                covered_by_private_local_subnet_program_rule = $matches.Count -gt 0
                matching_rules = $matches
            }
        }
    )
    $broadPatterns = @($RuleEvidence | Where-Object { $_.broad_or_public_pattern })

    return [pscustomobject]@{
        checked = [string]::IsNullOrWhiteSpace($ErrorMessage)
        read_only = $true
        service_exe = $ServiceExe
        expected_policy = [pscustomobject]@{
            program = $ServiceExe
            direction = "Inbound"
            action = "Allow"
            enabled = "True"
            profile = "Private"
            protocol = "TCP"
            remote_address = "LocalSubnet or narrower"
            required_ports = $requiredFirewallPorts
            diagnostics_only_ports = @(15101)
        }
        matching_private_inbound_program_rules = @($RuleEvidence | Where-Object { $_.expected_policy_match }).Count
        required_ports = $portReports
        required_ports_covered = @($portReports | Where-Object { -not $_.covered_by_private_local_subnet_program_rule }).Count -eq 0
        relevant_rules = $RuleEvidence
        broad_or_public_patterns = $broadPatterns
        broad_or_public_pattern_detected = $broadPatterns.Count -gt 0
        error = $ErrorMessage
        note = "This script is read-only; it does not create or edit firewall rules."
    }
}

function Get-FirewallRuleHint {
    $serviceExe = Join-Path $env:ProgramFiles "Boundless\boundless-service.exe"
    try {
        $evidence = @(Get-FirewallRuleEvidence -ExpectedProgram $serviceExe -RequiredPorts @(15100, 15200))
        return Get-FirewallPolicyReport -ServiceExe $serviceExe -RuleEvidence $evidence -ErrorMessage $null
    } catch {
        return Get-FirewallPolicyReport -ServiceExe $serviceExe -RuleEvidence @() -ErrorMessage $_.Exception.Message
    }
}

function Invoke-FirewallPolicySelfTest {
    $serviceExe = Join-Path $env:ProgramFiles "Boundless\boundless-service.exe"
    $requiredPorts = @(15100, 15200)
    $evidence = @(
        New-FirewallRuleEvidence -Name "good" -DisplayName "Boundless private" -Enabled "True" -Direction "Inbound" -Action "Allow" -Profile @("Private") -Program $serviceExe -Protocol "TCP" -LocalPort @("15100", "15200") -RemoteAddress "LocalSubnet" -ExpectedProgram $serviceExe -RequiredPorts $requiredPorts
        New-FirewallRuleEvidence -Name "broad" -DisplayName "Boundless broad" -Enabled "True" -Direction "Inbound" -Action "Allow" -Profile @("Any") -Program $serviceExe -Protocol "Any" -LocalPort "Any" -RemoteAddress "Any" -ExpectedProgram $serviceExe -RequiredPorts $requiredPorts
        New-FirewallRuleEvidence -Name "diagnostics-only" -DisplayName "Boundless 15101" -Enabled "True" -Direction "Inbound" -Action "Allow" -Profile @("Private") -Program $serviceExe -Protocol "TCP" -LocalPort "15101" -RemoteAddress "LocalSubnet" -ExpectedProgram $serviceExe -RequiredPorts $requiredPorts
    )
    $report = Get-FirewallPolicyReport -ServiceExe $serviceExe -RuleEvidence $evidence -ErrorMessage $null

    if (-not $report.required_ports_covered) {
        throw "expected TCP 15100 and 15200 coverage"
    }
    if (@($report.required_ports | Where-Object { $_.port -eq 15101 }).Count -ne 0) {
        throw "TCP 15101 must remain diagnostics-only, not a required firewall port"
    }
    if (-not $report.broad_or_public_pattern_detected) {
        throw "expected broad/Public-style pattern detection"
    }
    if (@($report.broad_or_public_patterns | Where-Object { $_.name -eq "diagnostics-only" }).Count -ne 0) {
        throw "diagnostics-only TCP 15101 rule must not be classified as broad"
    }

    Write-Host "connectivity_diagnostics_firewall_policy_fixtures=passed"
}

if ($SelfTest) {
    Invoke-FirewallPolicySelfTest
    exit 0
}

$uniquePorts = @($Ports | Sort-Object -Unique)
$localListeners = @($uniquePorts | ForEach-Object { Get-LocalListenerReport -Port $_ })
$remoteReports = @()
if ($RemoteHost) {
    $remoteReports = @($uniquePorts | ForEach-Object {
        Test-RemotePort -ComputerName $RemoteHost -Port $_ -TimeoutSeconds $TimeoutSeconds
    })
}

$report = [pscustomobject]@{
    schema_version = 1
    generated_at_utc = (Get-Date).ToUniversalTime().ToString("o")
    remote_host = $RemoteHost
    ports = $uniquePorts
    read_only = $true
    local_network_profiles = Get-NetworkProfileReport
    local_listeners = $localListeners
    side_by_side_guidance = Get-SideBySideGuidance -LocalListeners $localListeners
    remote_reachability = $remoteReports
    firewall_hint = Get-FirewallRuleHint
    guidance = [pscustomobject]@{
        pairing_port = 15200
        transport_port = 15100
        side_by_side_probe_ports = $uniquePorts
        private_profile_only = $true
        service_program = "%ProgramFiles%\Boundless\boundless-service.exe"
        public_network_warning = "Do not expose Boundless ports on Public networks or through router port forwarding."
    }
}

if ($Json) {
    $report | ConvertTo-Json -Depth 8
    exit 0
}

Write-Host "Boundless connectivity diagnostics (read-only)"
Write-Host "Ports: $($uniquePorts -join ', ')"
Write-Host ""
Write-Host "Network profiles:"
foreach ($profile in $report.local_network_profiles) {
    Write-Host ("- {0} [{1}] category={2}" -f $profile.name, $profile.interface_alias, $profile.network_category)
}

Write-Host ""
Write-Host "Local listeners:"
foreach ($listener in $report.local_listeners) {
    if (-not $listener.listening) {
        Write-Host ("- TCP {0}: not listening" -f $listener.port)
        continue
    }

    foreach ($entry in $listener.listeners) {
        $owner = $entry.owning_process
        Write-Host ("- TCP {0}: family={1} scope={2} pid={3} name={4} owner={5} path={6}" -f $listener.port, $entry.address_family, $entry.bind_scope, $owner.pid, $owner.name, $entry.owner_kind, $owner.path)
        if ($entry.owner_kind -ne "boundless") {
            Write-Host ("  mitigation: {0}" -f $entry.mitigation)
        }
    }
}

if ($report.side_by_side_guidance.Count -gt 0) {
    Write-Host ""
    Write-Host "Side-by-side guidance:"
    foreach ($guidance in $report.side_by_side_guidance) {
        Write-Host "- $guidance"
    }
}

if ($RemoteHost) {
    Write-Host ""
    Write-Host "Remote reachability to $($RemoteHost):"
    foreach ($remote in $report.remote_reachability) {
        if ($remote.reachable) {
            Write-Host ("- TCP {0}: reachable" -f $remote.port)
        } else {
            Write-Host ("- TCP {0}: not reachable ({1})" -f $remote.port, $remote.error)
        }
    }
}

Write-Host ""
Write-Host "Firewall guidance:"
Write-Host "- This script did not create or edit firewall rules."
Write-Host "- Boundless pairing uses TCP 15200; trusted transport uses TCP 15100. TCP 15101 is checked only for side-by-side dogfood collisions."
Write-Host "- firewall_hint reports whether TCP 15100 and 15200 appear covered by enabled inbound allow rules for %ProgramFiles%\Boundless\boundless-service.exe on Private profile with LocalSubnet-or-narrower remote scope."
Write-Host "- firewall_hint also reports broad/Public/Any-style matching rules as evidence; it does not change them."
Write-Host "- If a rule is needed, create it only after explicit approval, only for Private profile, and only for %ProgramFiles%\Boundless\boundless-service.exe."
Write-Host "- Do not expose Boundless ports on Public networks or through router port forwarding."
