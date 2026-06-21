# Troubleshooting

Start every support case by capturing current state instead of guessing from UI symptoms.

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
$BoundlessDaemon = "$env:ProgramFiles\Boundless\boundlessd.exe"
& $BoundlessCtl daemon status
& $BoundlessCtl diagnostics dump
```

For release candidates, also attach the latest `scripts/dev/release-readiness.ps1` packet.

## Diagnostic Bundle Privacy

`boundlessctl diagnostics dump` writes a redacted JSON bundle and a `.redaction.txt` manifest, then prints both paths. Use `--offline` when the daemon is not reachable; offline bundles still include CLI version and service metadata, but in-memory peer health and recent transfer history are unavailable. Use `--open-folder` to open the containing folder after export.

The default bundle includes versions, runtime mode, service install/path/version parity, component health, file-transfer settings, peer health aliases, and recent redacted transfer or transport events. It excludes clipboard plaintext, private keys, trust secrets, cert/key material, tokens, auth material, raw peer IDs, raw machine IDs, transfer IDs, request IDs, and local paths.

Filenames are redacted by default. Passing `--include-filenames` is an explicit opt-in that includes only basename values, not full local paths.

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
- `service start` succeeds but `daemon status` cannot reach `npipe://./pipe/boundlessd-api`,
- elevated-app or lock-screen behavior is expected but not working.

Checks:

```powershell
& $BoundlessCtl service status
& $BoundlessCtl daemon status
```

Service mode is not silently registered or started by the machine-wide MSI. Install it only from an elevated shell and the admin-protected `%ProgramFiles%\Boundless\boundless-service.exe` path. The service control pipe is ACL'd to `SYSTEM`, Administrators, and the installing user.

If the service is running but the CLI cannot connect, stop the normal tray and per-user daemon first so only one process owns the named pipe. Elevated-app and lock-screen claims remain deferred until Windows runtime evidence proves them.

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
