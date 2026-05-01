# Troubleshooting

Start every support case by capturing current state instead of guessing from UI symptoms.

```powershell
$BoundlessCtl = "$env:LOCALAPPDATA\Programs\Boundless\boundlessctl.exe"
$BoundlessDaemon = "$env:LOCALAPPDATA\Programs\Boundless\boundlessd.exe"
& $BoundlessCtl daemon status
& $BoundlessCtl diagnostics dump
```

For release candidates, also attach the latest `scripts/dev/v5-readiness.ps1` packet.

## Discovery

Symptoms:

- peer does not appear in Status & Pairing,
- manual host works but discovery does not,
- discovered host is stale.

Checks:

```powershell
& $BoundlessCtl pair discover
& $BoundlessCtl daemon status
```

Discovery depends on local network behavior. Manual host pairing remains the fallback when mDNS is unavailable or filtered.

## Firewall Or Network Reachability

Symptoms:

- pairing request times out,
- transport reconnect loops,
- diagnostics mention firewall-suspect events.

Checks:

```powershell
& $BoundlessCtl transport events --limit 100
& $BoundlessCtl diagnostics dump
```

Boundless v5 does not silently add firewall rules. Firewall rule install/check/remove remains a release follow-up unless a future admin-approved command is used.

If you add firewall rules manually, restrict them to trusted private LAN profiles and known peer IPs. Do not port-forward or expose Boundless control, pairing, or transport ports to the internet or public networks.

## Stale Daemon Or Named Pipe

Symptoms:

- tray starts but status is unavailable,
- CLI cannot connect to `npipe://./pipe/boundlessd-api`,
- Windows returns access denied or file not found for the pipe.

Checks:

```powershell
& $BoundlessCtl daemon status
& $BoundlessDaemon print-config-path
```

`Access is denied. (os error 5)` can mean another daemon instance still owns the named pipe. `The system cannot find the file specified. (os error 2)` usually means no daemon recreated the pipe.

## Service Mode

Symptoms:

- `boundlessctl service status` reports not installed,
- elevated-app or lock-screen behavior is expected but not working.

Checks:

```powershell
& $BoundlessCtl service status
```

Service mode is not silently installed by the per-user MSI. Elevated-app and lock-screen claims remain deferred until IPC ACL and service privilege-boundary validation are complete.

## Input Capture

Symptoms:

- mouse does not switch at edges,
- input remains captured by a peer,
- polling fallback is active.

Checks:

```powershell
& $BoundlessCtl input owner
& $BoundlessCtl input capture-target
& $BoundlessCtl input capture-stop
```

Confirm Easy Mouse, wrap, corner blocking, and the active layout in Settings or:

```powershell
& $BoundlessCtl layout preview
```

## Clipboard And File Transfer

Symptoms:

- clipboard payload does not arrive,
- file transfer is rejected,
- received file location is unexpected.

Checks:

```powershell
& $BoundlessCtl transport events --limit 100
& $BoundlessCtl file-transfer config
```

File transfer is default-deny unless the user accepts the transfer or enables trusted-peer auto-accept and the transfer passes path, size, hash, and temp-file validation. Per-peer auto-accept remains follow-up work.
