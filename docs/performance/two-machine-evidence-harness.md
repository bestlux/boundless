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

Validate the clipboard and image clipboard lab scenarios without two machines:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-clipboard-lab.ps1 -Mode Validate

Generate deterministic clipboard and image clipboard dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-clipboard-lab.ps1 -Mode DryRun -Role coordinator -Iterations 10

Generate deterministic dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode DryRun -Role coordinator -Iterations 5

Capture sanitized host metadata on each PC before a lab pass:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b

Summarize sanitized observations collected by a later lab driver:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -ObservationPath .\artifacts\performance\two-machine-evidence\observations.json

Observation files may be a JSON array or a packet with an observations array. The harness imports the core fields scenario, iteration, role, status, started_at_utc, latency_ms, duration_ms, bytes, and failure_kind. Unrecognized fields are ignored so raw peer IDs, local paths, endpoints, or payload text are not copied into artifacts.

The harness also preserves optional sanitized scenario metadata when present: scenario_variant, direction, payload_kind, payload_label, payload_bytes, policy_limit_bytes, policy_expected, payload_synthetic, provisional_classification, and provisional_classification_reason. These fields are intended for lab scenario definitions and must remain metadata only, never raw clipboard text, image bytes, file names, local paths, peer IDs, endpoints, or machine IDs.

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

Clipboard lab summaries also include payload byte min/max and provisional classification counts. The only accepted provisional classifications are no-op, acceptable, warning, and fail. They are labels for organizing lab artifacts before real measurements; they are not product guarantees or release thresholds.

## Clipboard And Image Clipboard Lab

The script scripts/dev/perf-clipboard-lab.ps1 is the BND-NEXT-14 prep slice. It does not read the real Windows clipboard and does not write synthetic payloads to the clipboard. It emits metadata-only synthetic observations, then feeds those observations into scripts/dev/perf-two-machine-evidence.ps1 so the JSON and Markdown artifacts match the shared two-machine evidence shape.

Default behavior:

- directions: A-to-B and B-to-A
- iterations: 10 per preset and direction
- text presets: small 128 B, medium 8 KiB, large 256 KiB
- image presets: screenshot-scale 1366x768 BMP, 1080p BMP, 4K BMP policy-bound sizing row, and near-limit 1448x1448 BMP
- privacy: payload_synthetic=true, payload_contents_recorded=false, and no raw clipboard contents or image bytes in artifacts

Current policy bounds used by the lab:

| preset | scenario | estimated payload bytes | current policy expectation |
| --- | --- | ---: | --- |
| text-small | text-clipboard | 128 | accepted |
| text-medium | text-clipboard | 8192 | accepted |
| text-large-policy-limit | text-clipboard | 262144 | accepted |
| image-screenshot-scale | image-clipboard | 4196406 | accepted |
| image-1080p | image-clipboard | 8294454 | accepted |
| image-4k-policy-bound | image-clipboard | 33177654 | rejected-by-current-policy |
| image-near-limit | image-clipboard | 8386870 | accepted |

The 4K row is included as a skipped no-op sizing row under the current 8 MiB image policy because a raw 3840x2160 32-bit BMP is larger than the current clipboard image limit. Do not convert that row into a passing synthetic observation unless the product policy changes and the new limit is verified from code.

For the later real two-PC lab, collect observations without private clipboard contents. Use synthetic text and image payloads with the same labels and byte counts, then summarize them:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -Scenario text-clipboard,image-clipboard -ObservationPath .\artifacts\performance\clipboard-lab\observations.json -ReleaseEvidence

Real lab observation rows should include scenario, scenario_variant, direction, iteration, role, status, latency_ms, duration_ms, bytes, payload_kind, payload_label, payload_bytes, policy_limit_bytes, policy_expected, payload_synthetic=true, provisional_classification, and failure_kind when applicable. They should not include raw clipboard text, image bytes, local file paths, peer IDs, machine IDs, endpoints, or user clipboard history.

Interpretation remains provisional until real two-PC evidence exists:

- no-op: skipped by design, unsupported by current policy, or metadata-only row
- acceptable: completed within the lab's provisional working band
- warning: completed but deserves follow-up before release claims
- fail: failed scenario row or measured result outside the provisional working band

Use measured p50, p95, max, success/failure/skipped counts, payload bytes, and transport notes to decide the next optimization. Do not claim BND-NEXT-9C readiness, Mouse Without Borders parity, secure desktop, lock-screen, UAC, or elevated-app parity from this clipboard lab.

## Evidence Boundary

Dry-run and fixture packets are developer diagnostics only. They prove schema, redaction, and summary math, not product speed or reliability.

Release evidence requires a real two-machine run with matching coordinator and peer packets, known build provenance, and at least one scenario observation for every selected scenario. Empty metadata-only capture packets remain developer diagnostics even when the ReleaseEvidence switch is supplied. This harness is suitable input for review of actual scenario rows, but it does not make BND-NEXT-14, BND-NEXT-15, or BND-NEXT-16 default PR gates.

The harness does not claim lock-screen, secure desktop, UAC prompt, elevated-app, or Mouse Without Borders parity. Those remain separate Windows lab evidence items.
