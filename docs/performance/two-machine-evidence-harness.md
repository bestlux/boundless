# Two-Machine Performance Evidence Harness

Status: foundation harness. This is developer test infrastructure until a real two-PC lab run supplies evidence from both machines.

## Purpose

The script scripts/dev/perf-two-machine-evidence.ps1 creates sanitized JSON and Markdown packets for later Windows two-machine performance and reliability runs. The harness is intentionally non-invasive: it does not pair devices, reset trust, change firewall rules, install or uninstall software, elevate, start or stop services, or read clipboard/file payload contents.

Use it for these scenario classes:

| scenario | release question it will eventually support |
| --- | --- |
| text-clipboard | small clipboard latency and failure rate |
| image-clipboard | large clipboard image latency and throughput when bytes are known |
| file-transfer | transfer duration, throughput, and failed iterations |
| reconnect-input | reconnect plus input handoff latency and failures |
| soak | longer reliability pass with failure counts and throughput where available |

## Commands

Validate the harness without two machines:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-fixtures.ps1

Generate deterministic dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode DryRun -Role coordinator -Iterations 5

Capture sanitized host metadata on each PC before a lab pass:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b

Summarize sanitized observations collected by a later lab driver:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -ObservationPath .\artifacts\performance\two-machine-evidence\observations.json

Observation files may be a JSON array or a packet with an observations array. The harness only imports these fields: scenario, iteration, role, status, started_at_utc, latency_ms, duration_ms, bytes, and failure_kind. Extra fields are ignored so raw peer IDs, local paths, endpoints, or payload text are not copied into artifacts.

## Artifact Contract

The JSON packet uses schema boundless.performance.two_machine.v1 and contains:

- repo metadata: branch, commit, dirty state
- build metadata: whether expected local target binaries were present and their version output when safely available, without storing binary paths
- environment metadata: role, host label, OS/build, PowerShell version, process architecture
- network metadata: profile category/connectivity and IPv4/IPv6 availability, without raw IP addresses
- Boundless ownership metadata: service installed/status/start mode/start account class/path class plus tray and daemon process counts, without process paths or raw account names
- privacy flags proving clipboard/file payload contents, raw peer IDs, raw machine IDs, raw local paths, and raw IP addresses were not recorded
- scenario observations and summary rows
- relative artifact paths for the JSON and Markdown outputs

Scenario summaries use nearest-rank percentiles over successful latency rows:

| metric | meaning |
| --- | --- |
| p50 | nearest-rank median latency in milliseconds |
| p95 | nearest-rank p95 latency in milliseconds |
| max | maximum successful latency in milliseconds |
| throughput_mbps | total successful bytes over total successful duration when bytes and duration are known |
| failure_count | rows with status = failed |

## Evidence Boundary

Dry-run and fixture packets are developer diagnostics only. They prove schema, redaction, and summary math, not product speed or reliability.

Release evidence requires a real two-machine run with matching coordinator and peer packets, known build provenance, scenario observations from actual Boundless operations, and review of all failed or missing rows. This harness is suitable input for that review, but it does not make BND-NEXT-14, BND-NEXT-15, or BND-NEXT-16 default PR gates.

The harness does not claim lock-screen, secure desktop, UAC prompt, elevated-app, or Mouse Without Borders parity. Those remain separate Windows lab evidence items.
