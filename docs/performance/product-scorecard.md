# Product acceptance scorecard

Use this scorecard to decide whether an exact candidate is ready for a stated audience and scope. Build success, fixture success, transport probes, and physical desktop success answer different questions. See [functional measurements](two-machine-evidence-harness.md) for commands and evidence schemas, and [release readiness](../release/release-readiness.md) for automated gate policy.

## Evidence classes

| Class | Evidence | Permitted conclusion |
| --- | --- | --- |
| Deterministic tests | Actual policy/state/session code with controlled failure injection | The tested behavioral contract holds in that controlled environment |
| Metadata fixture | Invented observations passed through report generation | Artifact shape, redaction and calculation checks work |
| Local benchmark | Actual worker/queue/logging code with injected connections, in-memory streams, or local filesystem; offscreen egui layout/tessellation | Measured local runtime/resource/CPU-render behavior; no physical LAN or native desktop claim |
| Paired transport | Executing authenticated daemons report raw RTT, echo integrity, exact executable hashes and session identities | Transport behavior for those endpoints and that run; `real_paired` means non-loopback, not hardware attestation |
| Physical acceptance | Operator-controlled run on identified PCs using exact installed candidate artifacts | Supported keyboard, mouse, clipboard, installation and recovery claims exercised in that run |

Record each category as passed, failed, or not measured, with its source class, run/artifact identity, and next action. A missing measurement is not a zero, a synthetic fixture is not a fast run, and an implementation is not an acceptance result.

## Required product questions

| Category | Observable acceptance | Evidence needed |
| --- | --- | --- |
| Host safety | Offline/flapping peers obey retry bounds; disk logs stay below their hard total/segment/file caps; input remains locally recoverable; task/handle/memory counts return to baseline | Deterministic fault/resource contracts, actual runtime/filesystem benchmarks, and unattended installed-candidate observation |
| Install/startup | Canonical installer succeeds; one tray and daemon owner; correct user-session broker; named-pipe/API health; preserved settings/trust after upgrade; repair and uninstall postconditions | Exact MSI lifecycle checks plus ordinary-user install/UAC/logon/reboot acceptance |
| Discovery/pairing | Both PCs can establish intended trust through product UI and recover a comprehensible error without unexplained resets | Physical network/profile cases with exact versions and both endpoint results |
| Transport | Stable session identity, expected candidate hashes, complete samples, matched payload integrity, progress under mixed workload | Paired transport report plus local failure/fairness benchmarks; additional sustained mixed traffic where claimed |
| Input | Both-direction screen-edge handoff works in target apps; physical escape works during supported faults; releases do not leave held input; touchpad/Num Lock behave correctly | Physical desktop acceptance plus deterministic safety/authorization tests |
| Clipboard | Text and supported image boundaries copy/paste both ways while input remains responsive; oversized/failed payloads degrade visibly and safely | Actual Windows clipboards and target applications; independent result checks, not only telemetry |
| File workflows | Claimed product flow completes with matching content, intended destination/consent, and bounded cleanup/cancellation behavior | Actual supported user journey and received-data hash, plus transport/cancellation tests |
| UX/accessibility | First-run, layout, offline and recovery states are understandable; keyboard focus and accessible names work at supported DPI/window sizes | Rendered Windows UI, keyboard-only and accessibility inspection; model tests supplement these |
| Endurance/recovery | Sleep/resume, peer absence and reconnect do not cause locks, runaway writes/CPU, unbounded growth, or lost trust | Real unattended/interruptible candidate runs with duration, event counts and resource trend recorded |

Host-safety limits are hard contracts and fail immediately when breached. Throughput and interactive latency targets must name the scenario, measurement clock and machine/network conditions; calibrate provisional bands with repeated real runs. Do not use blanket speed floors from metadata fixtures.

## Release interpretation

**Private dogfood/preview:** run applicable deterministic and installation gates; record every missing physical category and every known issue. The automatic release workflow publishes prereleases and never marks them latest, even when a tag contains only digits and dots. Signing follows configured policy.

**Public beta for a bounded feature scope:** establish repeated physical two-PC evidence for every advertised/core category, using exact installed binaries, plus host-safety and recovery evidence. Failed core safety or input-recovery cases block the claim. Optional out-of-scope features must be clearly disabled/deferred, not inferred ready from protocol support.

**Stable/parity claims:** require a separate review or promotion process using fresh, claim-scoped physical evidence and candidate identity. Neither `-Policy stable` in the automated packet generator nor a passing paired-transport report attests the entire desktop product. Four-PC, elevated-context, secure-desktop, lock-screen and other broader claims need their own matching evidence.
