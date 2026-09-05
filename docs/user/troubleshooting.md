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

- peer does not appear under Home → Add a PC,
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

The connectivity diagnostics script is read-only. It reports the active Windows network profiles, local listener/process ownership for TCP `16100` and `16200`, and remote TCP reachability when `-RemoteHost` is supplied. TCP `16200` is the nearby pairing listener; TCP `16100` is the transport listener used after trust is established. Legacy ports `15100`/`15200` and Mouse Without Borders port `15101` remain diagnostic observations.

In JSON output, `firewall_hint` reports read-only evidence for the expected policy shape: enabled inbound allow rules for `%ProgramFiles%\Boundless\boundless-service.exe`, Private profile, TCP `16100` and `16200`, and `LocalSubnet`-or-narrower remote scope. It also flags broad, Public, or Any-style matching rules. Legacy/MWB ports remain diagnostics-only, not default firewall requirements.

Boundless v5 does not silently add firewall rules. If pairing fails with a message such as `connect nearby pairing endpoint 10.10.0.187:16200 timed out`, treat it as a target reachability or firewall problem for remote TCP `16200` before debugging trust or code entry.

If manual host pairing works only after local firewall changes, verify inbound TCP `16100` and `16200` reachability for `%ProgramFiles%\Boundless\boundless-service.exe` on the Private network profile. Any firewall change should be run from an elevated shell only after explicit approval. Keep rules scoped to the Private profile and the Boundless service executable, for example:

```powershell
$ServiceExe = "$env:ProgramFiles\Boundless\boundless-service.exe"
New-NetFirewallRule -DisplayName "Boundless TCP 16100 Private" -Direction Inbound -Action Allow -Profile Private -Protocol TCP -LocalPort 16100 -Program $ServiceExe -RemoteAddress LocalSubnet
New-NetFirewallRule -DisplayName "Boundless TCP 16200 Private" -Direction Inbound -Action Allow -Profile Private -Protocol TCP -LocalPort 16200 -Program $ServiceExe -RemoteAddress LocalSubnet
```

If you add firewall rules manually, restrict them to trusted private LAN profiles and known peer IPs. Do not port-forward or expose Boundless control, pairing, or transport ports to the internet or public networks.

## Mouse Without Borders Side-By-Side

Symptoms:

- Mouse Without Borders or PowerToys is running during Boundless dogfood,
- diagnostics show another process listening on required TCP `16100` or `16200`, or legacy/MWB TCP `15100`/`15101`/`15200`,
- pairing or transport failures are ambiguous because both products are installed.

Checks:

```powershell
& $BoundlessCtl diagnostics dump
& "$env:ProgramFiles\Boundless\Boundless-ConnectivityDiagnostics.ps1"
```

The diagnostics bundle reports local listener ownership under `port_listeners` with address family, bind scope, port, process owner, and mitigation text. Support bundles redact endpoint-style addresses and full local paths by default; pass `--include-filenames` only when support explicitly asks for basename-level path context.

Boundless now defaults to TCP `16100`/`16200`, avoiding MWB's common `15100`/`15101` listeners. Old supported configs using 15100 migrate once; see [Migration](migration.md). Stop competing input sharing during qualification even when ports differ. If another process owns a required Boundless port, `network_port` can be changed; the nearby pairing listener is derived with an offset of `+100`. Apply the same port plan to every participating machine and adjust only the matching scoped firewall rules. Do not reset trust because of a listener collision.

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

Press Ctrl twice on the local keyboard to return control. Home also offers Pause input. Confirm Easy Mouse, wrap, and corner blocking in Sharing, then the active arrangement in Arrange PCs or:

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

File receipt is default-deny. Enable file transfer and trusted-peer auto-accept explicitly, and use a receive folder accessible to the selected user. There is currently no individual-transfer approval inbox. Per-peer auto-accept remains follow-up work. A changed or unavailable Windows user session can reject file operations even while the service and network remain healthy.

## Log storage and disk growth

The hardening implementation bounds both disk log streams independently:

| Stream | Active file | Segment size | Files retained | Maximum retained bytes |
| --- | --- | --- | --- | --- |
| Runtime | `boundlessd.log` | 10 MiB | 10 including active | 100 MiB |
| Service startup | `boundless-service-startup.log` | 1 MiB | 4 including active | 4 MiB |

Each stream also limits a record to 16 KiB and queues at most 256 records. Older daily runtime files matching the logger's exact legacy naming pattern participate in cleanup. Rotation and initialization run away from input/control execution; a storage failure suspends disk writes for a cooldown instead of growing an unbounded fallback file. Redacted diagnostic exports include `component_health.logging`: configured budgets, readiness, written/dropped/oversized record counts, and storage failures. Counters reset per process and do not require disk access to read. See [Project Status](../project-status.md) for validation of the hardening build.

Runtime logs live under `Boundless\logs` in the account's local application-data directory. A per-user daemon typically uses `%LOCALAPPDATA%\Boundless\logs`; the LocalSystem service has a separate system-profile data directory. Service startup logs are under `%ProgramData%\Boundless\logs`. App size in Windows Installed Apps may exclude these locations.

These budgets apply per stream and security context. An older installed binary can still have the original unbounded logger, and another account's log directory is separate. Check installed/runtime versions before assuming a source fix is active. Do not grant broad access to protected service-profile folders to inspect them.

For a report, preserve the path, size, timestamps, installed version, and a small excerpt. If disk space is critically low, stop the affected Boundless runtime first so it cannot keep growing the file, then remove only positively identified log files you intend to discard. Configuration, identity, and trust files are not logs. A working retry backoff and a bounded log sink are both required; changing the log level alone is insufficient.
