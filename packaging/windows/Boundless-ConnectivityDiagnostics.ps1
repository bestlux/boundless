param(
    [string]$RemoteHost,

    [ValidateRange(1, 65535)]
    [int[]]$Ports = @(15100, 15200),

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
            [pscustomobject]@{
                local_address = $listener.LocalAddress
                local_port = $listener.LocalPort
                owning_process = Get-ProcessSummary -ProcessId $listener.OwningProcess
            }
        }
    )

    return [pscustomobject]@{
        port = $Port
        listening = $owners.Count -gt 0
        listeners = $owners
    }
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
    remote_reachability = $remoteReports
    firewall_hint = Get-FirewallRuleHint -Ports $uniquePorts
    guidance = [pscustomobject]@{
        pairing_port = 15200
        transport_port = 15100
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
        Write-Host ("- TCP {0}: {1}:{0} pid={2} name={3} path={4}" -f $listener.port, $entry.local_address, $owner.pid, $owner.name, $owner.path)
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
Write-Host "- Boundless pairing uses TCP 15200; trusted transport uses TCP 15100."
Write-Host "- If a rule is needed, create it only after explicit approval, only for Private profile, and only for %ProgramFiles%\Boundless\boundless-service.exe."
Write-Host "- Do not expose Boundless ports on Public networks or through router port forwarding."
