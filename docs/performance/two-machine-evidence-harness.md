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

Validate the file-transfer lab scenarios without two machines or large payload writes:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-file-transfer-lab.ps1 -Mode Validate

Validate the reconnect, input handoff, and soak lab scenarios without two machines or disruptive operations:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-reconnect-input-soak-lab.ps1 -Mode Validate

Generate deterministic clipboard and image clipboard dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-clipboard-lab.ps1 -Mode DryRun -Role coordinator -Iterations 10

Generate deterministic file-transfer dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-file-transfer-lab.ps1 -Mode DryRun -Role coordinator -Iterations 3

Include the 1 GiB large-file row only as an explicit opt-in:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-file-transfer-lab.ps1 -Mode DryRun -Role coordinator -IncludeLarge

Generate deterministic reconnect, input handoff, and soak dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-reconnect-input-soak-lab.ps1 -Mode DryRun -Role coordinator -Iterations 3

Include the 2-hour soak metadata row as a synthetic passed row only as an explicit opt-in:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-reconnect-input-soak-lab.ps1 -Mode DryRun -Role coordinator -IncludeManualLongSoak

Generate deterministic dry-run artifacts:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode DryRun -Role coordinator -Iterations 5

Capture sanitized host metadata on each PC before a lab pass:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b

Summarize sanitized observations collected by a later lab driver:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -ObservationPath .\artifacts\performance\two-machine-evidence\observations.json

Observation files may be a JSON array or a packet with an observations array. The harness imports the core fields scenario, iteration, role, status, started_at_utc, latency_ms, duration_ms, bytes, and failure_kind. Unrecognized fields are ignored so raw peer IDs, local paths, endpoints, or payload text are not copied into artifacts.

The harness also preserves optional sanitized scenario metadata when present: scenario_variant, direction, payload_kind, payload_label, payload_bytes, policy_limit_bytes, policy_expected, payload_synthetic, provisional_classification, and provisional_classification_reason. These fields are intended for lab scenario definitions and must remain metadata only, never raw clipboard text, image bytes, file names, local paths, peer IDs, endpoints, or machine IDs.

File-transfer observations may also include setup_latency_ms, integrity_hash_status, expected_hash_label, received_hash_label, partial_file_status, receive_path_class, cleanup_status, file_count_class, file_count, retry_count, and reconnect_count. These fields must use sanitized labels and classes only. Do not store user file contents, private file names, raw hashes from private files, full local paths, peer IDs, trust secrets, endpoints, or machine IDs.

Reconnect, input handoff, and soak observations may also include failure_subsystem, input_capture_state, active_peer_class, transport_event_summary, soak_profile, soak_duration_minutes, manual_disruptive, and resource_trend_samples. These are sanitized labels and bounded numeric samples only. Do not store raw peer IDs, machine IDs, endpoints, adapter names, service account names, full local paths, clipboard or file contents, screenshots, secrets, or private host identifiers.

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

File-transfer summaries also preserve setup latency percentiles, retry/reconnect totals, direction and variant labels, file-count classes, integrity/hash status counts, cleanup status counts, partial-file status counts, and receive-path class counts when represented by observations.

Reconnect/input/soak summaries also preserve retry/reconnect totals, manual-disruptive row count, soak duration percentiles for successful rows, bounded CPU/memory trend summaries, active-peer class counts, input-capture state counts, failure-subsystem counts, soak-profile counts, and transport-event summary counts when represented by observations.

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

## File Transfer Lab

The script scripts/dev/perf-file-transfer-lab.ps1 is the BND-NEXT-15 prep slice. It does not read user files, copy user file contents, create large payloads by default, invoke the daemon transfer path, pair devices, reset trust, change firewall rules, or install/uninstall software. It emits metadata-only synthetic observations, then feeds those observations into scripts/dev/perf-two-machine-evidence.ps1 so the JSON and Markdown artifacts match the shared two-machine evidence shape.

Default behavior:

- directions: A-to-B and B-to-A
- iterations: 3 per preset and direction
- enabled presets: single small file, many small files, and medium 100 MiB metadata-only row
- disabled preset: large 1 GiB row as skipped/no-op metadata unless -IncludeLarge is supplied
- privacy: payload_synthetic=true, payload_contents_recorded=false, and no raw file contents, private file names, local paths, peer IDs, endpoints, trust secrets, or machine IDs in artifacts

Current file-transfer lab presets:

| preset | represented payload | file count class | default behavior |
| --- | ---: | --- | --- |
| single-small-file | 4 KiB | single-file | passed synthetic metadata |
| many-small-files | 128 x 4 KiB | many-small-files | passed synthetic metadata |
| medium-100mb | 100 MiB | medium-file | passed metadata-only fixture row; no 100 MiB artifact is written |
| large-1gb-opt-in | 1 GiB | large-file | skipped no-op unless -IncludeLarge is supplied |

The 1 GiB row is intentionally opt-in. Do not remove that guard or convert fixture mode into default large payload generation.

For the later real two-PC lab, use disposable synthetic files whose names do not include private project/customer/user information. Record only sanitized observation rows, then summarize them:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -Scenario file-transfer -ObservationPath .\artifacts\performance\file-transfer-lab\observations.json -ReleaseEvidence

Real lab observation rows should include scenario=file-transfer, scenario_variant, direction, iteration, role, status, setup_latency_ms, latency_ms or duration_ms for end-to-end duration, bytes, throughput source data, payload_kind, payload_label, payload_bytes, payload_synthetic=true, file_count_class, file_count, integrity_hash_status, expected_hash_label, received_hash_label, partial_file_status, receive_path_class, cleanup_status, retry_count, reconnect_count, provisional_classification, and failure_kind when applicable.

Hash and integrity requirements for the real lab:

- compute the expected SHA-256 over the synthetic source payload on the sending PC
- compute the received SHA-256 over the final received payload on the receiving PC
- mark integrity_hash_status=matched only when the received hash matches the expected hash
- mark failed rows with failure_kind=hash-mismatch when hashes differ
- mark failed rows with partial_file_status=partial-present when a partial received file remains
- mark failed rows with receive_path_class=unexpected-local-path if the received payload lands outside the designated lab receive root
- mark cleanup_status=stale-temp-detected if temp or .part files remain after cleanup

Interpretation remains provisional until real two-PC evidence exists:

- no-op: skipped by design, opt-in-only, or metadata-only row
- acceptable: completed within the lab's provisional working band
- warning: completed but deserves follow-up before release claims
- fail: failed scenario row, hash mismatch, partial-file status, unexpected receive path, stale temp cleanup, or measured result outside the provisional working band

Use measured p50, p95, max, throughput, setup latency, success/failure/skipped counts, payload bytes, file-count class, direction, retry/reconnect notes, hash status, receive-path class, and cleanup status to decide the next optimization. Do not claim BND-NEXT-9C readiness, Mouse Without Borders parity, secure desktop, lock-screen, UAC, or elevated-app parity from this file-transfer lab.

## Reconnect, Input Handoff, And Soak Lab

The script scripts/dev/perf-reconnect-input-soak-lab.ps1 is the BND-NEXT-16 prep slice. It does not restart services, restart the tray, disable adapters, interrupt the network, sleep or resume the machine, pair devices, reset trust, change firewall rules, elevate, install/uninstall software, capture screenshots, read the clipboard, or move files. It emits metadata-only synthetic observations, then feeds those observations into scripts/dev/perf-two-machine-evidence.ps1 so the JSON and Markdown artifacts match the shared two-machine evidence shape.

Default behavior:

- directions: A-to-B and B-to-A for reconnect/input rows
- iterations: 3 per reconnect/input preset and direction
- reconnect presets: service restart, tray restart, and manual network-loss row
- input preset: repeated edge handoff attempts with one synthetic classified failure
- soak presets: 30-minute synthetic metadata row plus 2-hour manual row skipped/no-op by default
- privacy: payload_synthetic=true, payload_contents_recorded=false, and no raw peer IDs, machine IDs, endpoints, adapter names, paths, clipboard/file contents, screenshots, or secrets in artifacts

Current reconnect/input/soak lab presets:

| preset | scenario | default behavior | manual/disruptive label |
| --- | --- | --- | --- |
| reconnect-service-restart | reconnect-input | passed synthetic metadata only | manual-runbook-required |
| reconnect-tray-restart | reconnect-input | passed synthetic metadata only | manual-runbook-required |
| reconnect-network-loss-manual | reconnect-input | skipped no-op | manual-disruptive-opt-in |
| input-edge-handoff | reconnect-input | repeated synthetic handoff rows with latency and one classified input failure | metadata-only-fixture |
| soak-30-minute | soak | passed synthetic metadata with bounded CPU/memory trend samples | metadata-only-fixture |
| soak-2-hour-manual | soak | skipped no-op unless -IncludeManualLongSoak is supplied | manual-long-run-required |

For the later real two-PC lab, collect sanitized host metadata on both PCs, perform one manual scenario at a time, record only sanitized observation rows, then summarize them:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role coordinator -HostLabel pc-a
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Capture -Role peer -HostLabel pc-b
    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\perf-two-machine-evidence.ps1 -Mode Summarize -Role coordinator -Scenario reconnect-input,soak -ObservationPath .\artifacts\performance\reconnect-input-soak-lab\observations.json -ReleaseEvidence

Manual scenario order for a real two-PC run:

1. Confirm both PCs are paired/trusted and idle enough for a short manual lab. Record sanitized build, role, and host labels with Capture mode.
2. Service restart scenario: manually restart only the intended Boundless service on the target PC, then measure time until the peer is reachable and input/transport state is stable again. Record scenario_variant=reconnect-service-restart, latency_ms, duration_ms, retry_count, reconnect_count, active_peer_class, input_capture_state, transport_event_summary, status, and failure_subsystem if failed.
3. Tray restart scenario: manually close and reopen only the tray on the target PC, then measure time until tray-visible state matches daemon state. Record scenario_variant=reconnect-tray-restart and the same reconnect fields.
4. Network-loss scenario: run only when the user explicitly chooses the disruptive step. Manually disable/enable the intended adapter, unplug/replug network, or otherwise create a short network interruption. Do not let the script change adapters or firewall rules. Record scenario_variant=reconnect-network-loss-manual and classify likely failure_subsystem as network, firewall, discovery, transport, service, tray, input, clipboard, file-transfer, or unknown.
5. Input handoff scenario: perform repeated edge transitions in both directions. Record one row per attempt with latency_ms, status, input_capture_state, active_peer_class, retry_count, failure_subsystem, and failure_kind when a handoff fails.
6. Soak 30-minute scenario: run the paired PCs under the intended idle/clipboard/input background mix for 30 minutes. Record soak_profile=30-minute, soak_duration_minutes, failure counts, retry/reconnect counts, transport_event_summary, and bounded resource_trend_samples.
7. Soak 2-hour scenario: run only as a manual long-run pass when the user has time. Record soak_profile=2-hour with the same fields. Keep it out of default PR gates.
8. Optional sleep/resume observation: only when the user explicitly triggers sleep and resume manually. Record it as a reconnect-input observation with a clear scenario_variant such as reconnect-sleep-resume-manual and manual_disruptive=true. The scripts do not put the machine to sleep.

Real lab observation rows should include scenario=reconnect-input or scenario=soak, scenario_variant, direction where meaningful, iteration, role, status, started_at_utc, latency_ms for reconnect or handoff attempts, duration_ms for end-to-end scenario duration, retry_count, reconnect_count, failure_subsystem, failure_kind when applicable, input_capture_state, active_peer_class, transport_event_summary, soak_profile, soak_duration_minutes, resource_trend_samples, manual_disruptive, payload_synthetic=true, provisional_classification, and provisional_classification_reason.

Use only these likely failure_subsystem labels unless a future schema revision expands them: service, discovery, transport, input, clipboard, file-transfer, firewall, network, tray, or unknown.

Resource trend samples must stay bounded. Prefer 4 to 12 rows per soak observation with only sample_index, elapsed_seconds, cpu_percent, and memory_mb. Do not include process paths, command lines, account names, raw host names, raw peer IDs, endpoints, screenshots, clipboard contents, file contents, or secrets.

Interpretation remains provisional until real two-PC evidence exists:

- no-op: skipped by design, manual-only, disruptive-only, or metadata-only row
- acceptable: completed within the lab's provisional working band
- warning: completed but deserves follow-up before release claims
- fail: failed scenario row, unstable reconnect/input state, excessive retries, resource trend concern, or measured result outside the provisional working band

Use measured p50, p95, max, reconnect duration, input handoff latency, success/failure/skipped counts, retry/reconnect totals, input capture state, active peer class, transport event summary, CPU/memory trend, soak duration/profile, and failure subsystem to decide the next reliability fix. Do not claim BND-NEXT-9C readiness, Mouse Without Borders parity, secure desktop, lock-screen, UAC, or elevated-app parity from this lab.

## Evidence Boundary

Dry-run and fixture packets are developer diagnostics only. They prove schema, redaction, and summary math, not product speed or reliability.

Release evidence requires a real two-machine run with matching coordinator and peer packets, known build provenance, and at least one scenario observation for every selected scenario. Empty metadata-only capture packets remain developer diagnostics even when the ReleaseEvidence switch is supplied. This harness is suitable input for review of actual scenario rows, but it does not make BND-NEXT-14, BND-NEXT-15, or BND-NEXT-16 default PR gates.

The harness does not claim lock-screen, secure desktop, UAC prompt, elevated-app, or Mouse Without Borders parity. Those remain separate Windows lab evidence items.
