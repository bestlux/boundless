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

The default bundle includes versions, runtime mode, service install/path/version parity, local listener ownership for Boundless-related TCP ports, component health, file-transfer settings, peer health aliases, and recent redacted transfer or transport events. It excludes clipboard plaintext, private keys, trust secrets, cert/key material, tokens, auth material, raw peer IDs, raw machine IDs, transfer IDs, request IDs, raw endpoints, and local paths.

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

Discovery depends on local network behavior. Manual host pairing remains the fallback when mDNS is unavailable or filtered. Reset Network and Safe Reset keep live mDNS discovery results; if peers still do not appear after reset, mDNS or firewall policy may be blocking discovery or peer reachability.

## Firewall Or Network Reachability

Symptoms:

- pairing request times out,
- transport reconnect loops,
- diagnostics mention firewall-suspect events.

Checks:

```powershell
& $BoundlessCtl transport events --limit 100
& $BoundlessCtl diagnostics dump
& "$env:ProgramFiles\Boundless\Boundless-ConnectivityDiagnostics.ps1" -RemoteHost 10.10.0.187
```

Transport events are a bounded in-memory diagnostic ring, not durable history. State transitions and failures take precedence over high-rate activity. Repeated wake, input-frame, injection, anti-idle, reconcile, and transfer-progress activity is represented by bounded summaries with `sample_count`, `first_seen`, and `last_seen` rather than one retained record per sample. Use `--limit 0` to print the full retained ring. Use `--kind <substring>` and `--exclude-kind <substring>` to focus the view; filters are applied before the limit, for example `transport events --limit 100 --exclude-kind input_runtime`.

The connectivity diagnostics script is read-only. It reports the active Windows network profiles, local listener/process ownership for TCP `15100`, `15101`, and `15200`, and remote TCP reachability for those ports when `-RemoteHost` is supplied. TCP `15200` is the nearby pairing listener; TCP `15100` is the transport listener used after trust is established. TCP `15101` is included to make side-by-side dogfood with Mouse Without Borders or other tools easier to diagnose.

In JSON output, `firewall_hint` reports read-only evidence for the expected policy shape: enabled inbound allow rules for `%ProgramFiles%\Boundless\boundless-service.exe`, Private profile, TCP `15100` and `15200`, and `LocalSubnet`-or-narrower remote scope. It also flags broad, Public, or Any-style matching rules. TCP `15101` remains diagnostics-only, not a default firewall requirement.

Boundless v5 does not silently add firewall rules. If pairing fails with a message such as `connect nearby pairing endpoint 10.10.0.187:15200 timed out`, treat it as a target reachability or firewall problem for remote TCP `15200` before debugging trust or code entry.

If manual host pairing works only after local firewall changes, verify inbound TCP `15100` and `15200` reachability for `%ProgramFiles%\Boundless\boundless-service.exe` on the Private network profile. Any firewall change should be run from an elevated shell only after explicit approval. Keep rules scoped to the Private profile and the Boundless service executable, for example:

```powershell
$ServiceExe = "$env:ProgramFiles\Boundless\boundless-service.exe"
New-NetFirewallRule -DisplayName "Boundless TCP 15100 Private" -Direction Inbound -Action Allow -Profile Private -Protocol TCP -LocalPort 15100 -Program $ServiceExe
New-NetFirewallRule -DisplayName "Boundless TCP 15200 Private" -Direction Inbound -Action Allow -Profile Private -Protocol TCP -LocalPort 15200 -Program $ServiceExe
```

If you add firewall rules manually, restrict them to trusted private LAN profiles and known peer IPs. Do not port-forward or expose Boundless control, pairing, or transport ports to the internet or public networks.

## Mouse Without Borders Side-By-Side

Symptoms:

- Mouse Without Borders or PowerToys is running during Boundless dogfood,
- diagnostics show another process listening on required TCP `15100` or `15200`, or diagnostics-only TCP `15101`,
- pairing or transport failures are ambiguous because both products are installed.

Checks:

```powershell
& $BoundlessCtl diagnostics dump
& "$env:ProgramFiles\Boundless\Boundless-ConnectivityDiagnostics.ps1"
```

The diagnostics bundle reports local listener ownership under `port_listeners` with address family, bind scope, port, process owner, and mitigation text. Support bundles redact endpoint-style addresses and full local paths by default; pass `--include-filenames` only when support explicitly asks for basename-level path context.

Prefer stopping Mouse Without Borders/PowerToys before pairing Boundless machines when it owns required Boundless TCP `15100` or `15200`. MWB/PowerToys on diagnostics-only TCP `15101` is side-by-side evidence, not a Boundless pairing or transport port collision by itself. If you need side-by-side dogfood and a required Boundless port is actually owned by another process, Boundless already supports changing the daemon `network_port`; the nearby pairing listener is derived from that port with an offset of `+100`, so `network_port = 16100` pairs on TCP `16200`. Apply the same alternate port plan to every participating Boundless machine before pairing. Do not reset trust just because a local listener collision is detected.

For installed builds, stop the tray/service, locate the daemon config, edit `network_port`, then restart Boundless:

```powershell
& "$env:ProgramFiles\Boundless\boundlessd.exe" print-config-path
```

Use this only as a dogfood workaround. Boundless does not create firewall rules, elevate diagnostics, import Mouse Without Borders configuration, or mutate network state in this flow.

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

The machine-wide MSI registers and starts `BoundlessService` from `%ProgramFiles%\Boundless\boundless-service.exe` when installed with an explicit `BOUNDLESS_ALLOWED_USER_SID=S-...`. Manual `boundlessctl service install` is a developer fallback for unpackaged builds. The service control pipe is ACL'd to `SYSTEM`, Administrators, and the selected user SID.

When `BoundlessService` is installed, the tray should use the service-owned daemon and must not start a separate per-user `boundlessd.exe`. If the tray or CLI cannot reach the service pipe, restart `BoundlessService` or repair the MSI install, and verify the install selected the intended user SID. Do not start a competing per-user daemon in service mode. Elevated-app and lock-screen claims remain deferred until Windows runtime evidence proves them.

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
