# Windows hardening assessment and local verification

Reviewed and implemented 2026-09-04–05. Product target: a polished Windows-to-Windows alternative to Mouse Without Borders. This is a development candidate, not a release or installed-product qualification.

## Decision

**Keep Rust and the existing codebase; substantially refactor selected runtime and privilege boundaries.** A full restart would discard working Win32 input, key semantics, authenticated pairing, TLS, clipboard deduplication, transfer validation and regression knowledge. Continuing the old backlog unchanged would leave demonstrated resource and authority problems ahead of public launch.

The first implementation train now addresses those immediate defects and simplifies the tray. The remaining architectural work is consequential: the full daemon still runs as LocalSystem in service mode, and the user input engine still shares the tray process. Scoped user authority and safer process recovery reduce those risks; they are not a complete process sandbox or independent engine.

## Candidate identity

- Baseline: `9bad1b5c36e31aa81f8764e72fed66785f85c05a`.
- Final implementation: `5fa97d85309de64ed6ebd4e58d4e8e844a48f597` on `codex/windows-product-hardening`. The independent review boundary and final correction are recorded below.
- App/package manifests remain 5.0.16; this candidate is distinct from the published 5.0.16 artifacts.
- Peer wire protocol 4.5, configuration schema 6, input-broker IPC revision 7. Upgrade both PCs and keep daemon/tray binaries compatible. Schema-5 durable settings and trust migrate; live connection observations are discarded. Follow the [migration guide](../user/migration.md) for backup and rollback.
- Local evidence uses Windows 11 build 26100, x64, Rust/Cargo 1.94.0, locked dependency builds. Incremental compilation was disabled after local cache access-denied rename failures; no global tooling or cache deletion was required.

## What changed and why

| Area | Implemented behavior | Practical boundary |
| --- | --- | --- |
| Offline peer and logs | Absolute retry deadlines survive noisy reconcile wakes and short-lived sessions. Repeated connection failures are summarized. Runtime logs retain at most 10 × 10 MiB; startup logs 4 × 1 MiB, with 14-day retention. Records are bounded to 16 KiB and the lossy queue to 256 records / 4 MiB payload allocation. Disk failure has a cooldown and bounded diagnostic; shutdown is bounded. | Budgets are per stream/security context. Exact recognized legacy files are handled; unknown files are untouched. An extended installed soak is still required. |
| Peer scheduling and lifetime | Per-peer transition ownership, independent receive progress, TLS admission/deadline bounds, bounded reader buffers and scoped session/task cleanup. Configuration schema 6 separates durable settings from live connectivity. | Synthetic contention and local TLS prove selected invariants; LAN handoff latency and sustained resource use remain unmeasured. |
| Windows file authority | User-selected sources, receive publication and the actual diagnostics RPC run under a revocable selected-console-user lease. No SYSTEM fallback on missing user authority. Sources retain authorized handles; no-overwrite publication handles collisions. | The installed service still selects a fixed allowed SID. Revocation is checked at operation boundaries; it cannot interrupt an already-started OS file call. |
| File cancellation | At most 64 open/pending sources, including cancelled blocking workers. Each read retains its file and permit together; registry locks are released before storage I/O. Reconnect can clear transfer state while a slow read finishes. | Abrupt cancellation can leave partial receive staging. Receiver publication acknowledgment and fuller cleanup ownership remain future work. |
| Input ownership | Broker identity includes PID and process creation time. Process replacement discards uncertain ordinary input, conservatively releases possible held keys/buttons, and requires a fresh handoff. Recovery releases retain their dedicated lane until acknowledged, including lost replies and detach. | Native input across hard death is not exactly once. Cleanup needs a live replacement broker; complete presentation/engine process separation remains open. |
| Local pause | The tray releases its local lock before waiting for IPC. Paused service sessions keep clipboard exchange alive while capture, ordinary injection, relock and owner claims are denied. Resume requires fresh authority. | Physical pause/crash/elevated-helper tests have not been run for this candidate. |
| Public UI | Home, Arrange PCs, Files, Sharing and Support; clearer first-run pairing, freshness-aware status, unavailable actions disabled, native file selection, local pause, redacted support export and temporary paired-test consent. AccessKit is enabled. | Offscreen rendering and interaction tests are not native screen-reader, file-dialog, mixed-DPI or two-PC UX qualification. |
| AI-friendly tests | `paired-test allow/status/revoke/run` provides peer-scoped temporary consent, actual TLS RTT and synthetic bulk echo integrity, bounded requests, raw samples and executable/session identity. | Transport only: no remote input, clipboard, user files, commands or automatic disruption. Broader one-controller product testing remains roadmap work. |
| Documentation and gates | Current capability matrix, ranked roadmap/backlog, user/developer guidance and trust/transport maps replace stale status. Historical plans remain preserved. Evidence validators reject missing/fabricated samples and distinguish fixtures from physical results. Future engineering releases are prerelease and not latest. | No release was published or promoted. No full Mouse Without Borders parity or competitor advantage is claimed. |

## Disk-exhaustion evidence

The baseline's real peer worker, using injected failed connections, made **7,900 attempts in 251 ms** because wakeups bypassed its intended delay. This was a bounded regression experiment, not 7,900 TCP connections or a disk-write benchmark. The fix enforces retry timing independently of wake frequency; log caps provide a separate protection if another loop becomes noisy.

The reported work PC's version and oversized filename remain unknown, so its reported 500 GB growth is not reproduced or attributed to an exact build. Earlier read-only inspection on this PC found installed metadata for 5.0.13, a stopped service/no running Boundless process, 44,307 bytes of accessible daemon logs and a 39,565-byte startup log. The service-profile directory was access denied; that profile was not cleared of historical growth. No installed logs were deleted, and the installed app was not replaced.

## Validation

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed. |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Passed. |
| `cargo test --locked --workspace -- --test-threads=2` | **773 passed**, 5 explicit profiling tests ignored, across 32 test/doc-test groups. Relevant profiling tests were run separately in release mode. |
| Functional evidence, release-readiness and two-machine performance fixtures | Passed under PowerShell 7 and Windows PowerShell 5.1. These validate evidence/gate behavior; they do not launch physical tests. |
| Windows authority and input tests | Included actual restricted-token permission denial, denied diagnostic export, named-pipe rights/identity, authorized handle/path replacement, publication collision, and simulated input/send/cancellation faults. Native user input was not injected. |
| Dashboard rendering | 29 offscreen fixtures rendered; representative first-run, Files, Sharing and degraded/compact states inspected. |
| Release executables and MSI | Locked optimized builds; MSI ProductVersion and staged binary hashes checked. See candidate artifact identity below. |
| Documentation and version consistency | Current document links/status and release metadata checked. Published changelog preserved as history. |

The earlier packaging-script fixtures passed in PowerShell 7; their Windows PowerShell 5.1 run hit an OS `AccessDenied` on `Start-Process` in `Boundless-Install.ps1:2565`. The MSI build succeeding does not close that fixture limitation. No installed upgrade, service replacement, UAC, session-switch or physical two-PC operation ran during this hardening work.

The functional coverage exercises behavior: failed connection deadlines under wake storms; short-lived sessions; independent peer progress under blocked output; actual writer rotation, queue allocation and disk failure; user-token permission denial and path replacement; source capacity through aborted opens and reads; actual reconnect cleanup; process replacement and possible held input; lost cleanup responses; clipboard while paused beyond broker expiry; and actual trusted/untrusted TLS, consent expiry, quotas, revocation and session replacement. These tests are stronger than source-shape assertions, but their fixtures have explicit limits.

## Optimized measurements

Measured on clean `5fa97d8`, optimized test executables:

| Workload | Local observation |
| --- | --- |
| Refused connections under noisy reconcile wakes | 3 attempts in 3,251 ms. |
| Immediate session close under noisy wakes | 3 attempts in 3,260 ms. |
| Unrelated peer progress while another peer's bulk writer is stalled | 17–80 µs across 10 injected-stream samples, within a 250 ms functional deadline. |
| Real bounded log writer | 256 MiB / 32,768 records processed in 158 ms; observed peak retained bytes 100 MiB, final 96 MiB in 10 files. Approximately 1,611 MiB/s through the OS write cache. |
| Input stage + exact acknowledgment | p95 6.6 µs for 64 frames / 192 events, 10,000 iterations. |
| Process-loss release recovery state | p95 6.3 µs for the same workload, 10,000 iterations. |
| Home / Arrange / Files frame CPU at 1100 × 800 | Combined layout + tessellation p95 21.0 / 32.8 / 44.9 µs, 200 warmed frames per view. |
| Home / Arrange / Files frame CPU at 800 × 600 | p95 19.6 / 33.2 / 40.1 µs, 200 warmed frames per view. |
| Actual loopback TLS, 64-byte echo | 8 samples; p50 27 µs, p95 152 µs. |
| Actual loopback TLS, 64-KiB echo integrity | 8 samples; p50 417 µs, p95 574 µs; 1 MiB round-trip payload verified. |

The TLS fixture's eight samples are an exploratory measurement, below the default 20-sample evidence gate; validation explicitly used `-MinimumSamples 8`. Its report identifies the test executable and loopback category, with no installed-candidate or physical-PC assertion. The main raw packet is `candidate-benchmarks-release.json`, SHA-256 `55dc780c2d94423b3f811619a6f196fe548130ef682ca064139e5c65bfd8b4eb`. The additional input/TLS measurements and executable hash are in the `candidate-*` logs and provenance JSON beside it.

Numbers are local observations, not product-wide latency budgets or a fastest/lightest claim. Log throughput measures the operating-system write cache, not durable fsync throughput. UI measurements time warmed egui layout/tessellation, excluding the native window, GPU, presentation and IPC. Input-state timings exclude native capture/injection. TLS measurements below use loopback, not two physical PCs.

## Review and artifacts

Three fresh independent review rounds covered resource/concurrency behavior, Windows/peer authority, and input/product/evidence behavior against clean candidate checkouts. **Six distinct findings were fixed: one P1 and five P2.** They concerned paused clipboard lifetime, removed-peer attachment, capacity during cancelled source opens and reads, recovery-lane loss on detach, and premature cancellation receipts during failed key cleanup. New behavioral regressions reproduced the two receipt defects before their fixes.

The third round at `f8b47b9` returned clean authority and resource reviews and one input finding. Its final correction is `5fa97d8`, reviewed locally and covered by the full validation above. The configured three-round cap was reached; there was no additional independent review round of that last correction. No finding is silently classified as clean or left without a disposition.

Raw evidence is local under `artifacts/reviews/2026-09-04/` (gitignored): full workspace and Clippy logs, Windows PowerShell fixture logs, benchmark JSON with raw samples and hashes, paired TLS output, the review ledger, and 29 rendered dashboard fixtures in `ui-current/`. The initial ground-up assessment and detailed subsystem reports are retained there as dated pre-implementation evidence. Their baseline proposals are superseded by the implemented contracts summarized here.

The final unsigned local MSI is `Boundless-5.0.16-5fa97d8-windows-x64.msi` (18,763,776 bytes), SHA-256 `1071fc5c5e1d0cc22b9ad8492d9bde8454c731302483d455989a01db8db42398`. Its matching `-install.ps1`, `SHA256SUMS.txt`, and `candidate-build-manifest.json` are in the evidence directory. The manifest records all five executable hashes; staged payloads match the built files, and the MSI reports ProductVersion 5.0.16. This supersedes the earlier `f8b47b9` package. Signing remains optional for this local engineering candidate. Nothing was installed, pushed or published.

## Ordered work after this candidate

1. Qualify compatible candidate builds on two real Windows PCs: offline soak and recovery, held-input crash/escape/next handoff, both clipboard directions and failures, explicit file transfer/permission denial, sleep/resume, and installed upgrade/repair/uninstall. Capture executable hashes, Windows/session context and raw measurements.
2. Replace the fixed-SID install-helper contract with a simpler plain MSI and explicit runtime desktop-user authorization, preserving standard-user boundaries and revocation.
3. Reduce the machine service's authority and extract the user input engine from presentation lifetime. Keep any elevated injector narrowly authenticated and explicitly activated.
4. Complete first-run and repair flows, supported firewall ownership, per-peer file consent and the smallest useful Explorer single-file workflow.
5. Extend one-controller testing with individually consented functional workloads and fault observation, keeping each report honest about what ran on the peer. Establish idle CPU/private-byte, handoff/recovery, contention and UI budgets from real optimized builds.
6. Qualify the public Windows/session/display matrix, signed distribution and support policy; simplify redundant release orchestration after the installer contract shrinks. Qualify four PCs separately. Other platforms remain deferred.

The [roadmap](../v5-roadmap.md), [backlog](../backlog.md) and [capability matrix](../parity/mouse-without-borders.md) own the evolving plan. This page records the candidate and evidence at this date.
