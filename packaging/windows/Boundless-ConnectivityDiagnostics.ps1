param(
    [string]$RemoteHost,

    [ValidateRange(1, 65535)]
    [int[]]$Ports = @(15100, 15101, 15200),

    [ValidateRange(1, 30)]
    [int]$TimeoutSeconds = 4,

    [switch]$Json
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

function Get-FirewallRuleHint {
    param([int[]]$Ports)

    $serviceExe = Join-Path $env:ProgramFiles "Boundless\boundless-service.exe"
    try {
        $rules = @(Get-NetFirewallRule -Direction Inbound -Action Allow -Enabled True -Profile Private -ErrorAction Stop |
            Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
            Where-Object { $_.Program -eq $serviceExe })
        return [pscustomobject]@{
            checked = $true
            service_exe = $serviceExe
            matching_private_inbound_program_rules = $rules.Count
            required_ports = $Ports
            note = "This script is read-only; it does not create or edit firewall rules."
        }
    } catch {
        return [pscustomobject]@{
            checked = $false
            service_exe = $serviceExe
            matching_private_inbound_program_rules = $null
            required_ports = $Ports
            error = $_.Exception.Message
            note = "This script is read-only; it does not create or edit firewall rules."
        }
    }
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
    firewall_hint = Get-FirewallRuleHint -Ports $uniquePorts
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
Write-Host "- Boundless pairing uses TCP 15200; trusted transport uses TCP 15100. TCP 15101 is checked for side-by-side dogfood collisions."
Write-Host "- If a rule is needed, create it only after explicit approval, only for Private profile, and only for %ProgramFiles%\Boundless\boundless-service.exe."
Write-Host "- Do not expose Boundless ports on Public networks or through router port forwarding."
