# Functional measurements and evidence

Use the actual product path for measurements. Keep report-format fixtures, local runtime benchmarks, paired transport measurements, and physical desktop acceptance distinct. A synthetic payload sent by the running product is useful measurement; a script that invents latency numbers is a fixture.

## Paired transport from one controller

The receiving PC grants a temporary, peer-specific permission. From the controller, run the bounded authenticated RTT and bulk echo checks:

```powershell
# On the receiving PC, permit the controller's existing paired identity.
boundlessctl paired-test allow <controller-peer-id> --seconds 300

# On the controller, collect the actual daemon report.
boundlessctl --json paired-test run <receiver-peer-id> --samples 20 --payload-bytes 65536 > paired-test.json

# Validate the saved report without contacting either PC.
./scripts/dev/validate-paired-test.ps1 -ReportPath paired-test.json `
  -RequireRealPaired -ExpectedDaemonSha256 <candidate-executable-sha256> `
  -OutputPath artifacts/paired-transport-validation.json

# On the receiving PC, revoke permission early when finished.
boundlessctl paired-test revoke
```

This permission is not remote shell access. These probes use generated in-memory bytes; they do not drive keyboard/mouse injection, clipboard, file workflows, emergency unlock, recovery, or resource-budget tests. Read the report's `not_tested` list. The receiver's permission expires and has request/byte budgets.

`PairedTestReport` schema version 1 records the run ID and time, both daemon identities and executable SHA-256 values, daemon/protocol/platform versions, process and transport-session identities, bounded raw RTT samples, computed percentiles, completed/requested counts, verified round-trip bytes, and errors. Timing uses the controller's monotonic clock. An absent build-time source revision remains unknown; it is never inferred from the current checkout.

| Category | What ran | What it does not prove |
| --- | --- | --- |
| `loopback` | Actual authenticated TCP/TLS exchange through loopback | Two physical PCs or LAN behavior |
| `real_paired` | Actual authenticated exchange through a non-loopback TCP socket | Distinct physical hardware or desktop acceptance |
| `synthetic` | In-memory post-authentication fixture | Actual TCP/TLS or hardware |

The validator accepts successful `loopback`/`real_paired` reports and requires at least 20 samples for each probe by default. `-RequireRealPaired` rejects loopback. It recomputes counts, p50/p95 and verified byte totals, rejects malformed or partial successes, checks distinct endpoint identities and session IDs, and requires valid executable hashes. It records a hash of the input report. A failed diagnostic report remains useful for investigation but fails this acceptance command.

`-ExpectedDaemonSha256` binds both reported executing binaries to the selected candidate. Use the hash of the executable actually hosting the daemon: `boundless-service.exe` for service mode, `boundlessd.exe` for direct mode. Omit it for exploratory comparisons; the result then explicitly says `candidate_hash_bound=false`. `-ExpectedSourceRevision` requires matching full build-time commit identities on both sides when available. Reports older than seven days or implausibly in the future fail by default; `-MaxEvidenceAgeHours` sets the review window.

This is consistency checking, not attestation. An edited JSON file or dishonest authenticated peer can fabricate observations. Keep the original report and exact candidate artifact with the operator's physical-machine record when making public claims. The validated output schema is `boundless.validation.paired_transport.v1`; it always says physical two-PC acceptance is not proven.

## Runtime, disk-budget and UI benchmarks

```powershell
./scripts/dev/functional-benchmarks.ps1
./scripts/dev/functional-benchmarks.ps1 -Benchmark transport
./scripts/dev/functional-benchmarks.ps1 -Benchmark logging
./scripts/dev/functional-benchmarks.ps1 -Benchmark ui
```

The wrapper builds daemon library and tray test executables with `--locked`, two build jobs, and a separate target directory. It executes exactly one ignored test per selected benchmark, rejects missing metric output, and validates named safety bounds. It preserves each binary hash, checkout commit/dirty state, toolchain, platform, raw measurements, and execution logs under `artifacts/performance/functional-benchmarks/`. A dirty checkout is recorded as such; the executable hash identifies what actually ran.

| Benchmark | Actual exercised code | Measurements and limits |
| --- | --- | --- |
| `transport` | Production peer worker with injected refusal/immediate close and noisy reconcile signals; real ownership/queues with an in-memory stalled writer | Retry attempt count over an elapsed window; ten unrelated-peer progress measurements while another writer is stalled. These are local runtime contracts, not physical network/input latency. |
| `logging` | Production bounded disk writer on a temporary local filesystem directory | 256 MiB processed through 10 MiB segments with a 100 MiB / ten-file retention cap; elapsed time, throughput, final/peak retained bytes and file count. Retained data must stay within the cap. |
| `ui` | Actual offscreen egui layout and tessellation for Home, Arrange and Files at regular/compact sizes | 30 warmup and 200 measured frames per case; raw component/combined nanoseconds and recomputed nearest-rank summaries. Measures CPU work, excluding native presentation, GPU, broker and IPC. No universal hardware timing threshold. |

The output schema is `boundless.performance.functional_benchmarks.v1`. These intentionally opt-in measurements are not automatic user-machine stress tests. They do not operate the installed service or physical input/clipboard. They are useful before and after a refactor; hardware-dependent throughput is recorded rather than compared to an invented universal speed floor.

## Input-stage timing

```powershell
./scripts/dev/test-suite.ps1 -Profile trace `
  -EndpointA <control-endpoint-a> -EndpointB <control-endpoint-b> `
  -TraceEnforceBudgets -TraceMinimumSamples 20
```

Trace collection without `-TraceEnforceBudgets` remains diagnostic. The readiness latency gate always enables it. The enforced gate needs enough fresh capture-to-receive, receive-to-apply, and capture-to-apply samples for every target, plus jitter. Historical events present at the start are excluded. Missing samples and excessive budgets fail. Suspected clock skew fails an end-to-end claim instead of replacing it with a different receiver-only metric. The former `-AdjustForClockSkew` option was removed for that reason; use same-clock paired RTT when clocks cannot support cross-machine stage timing.

Stage telemetry does not by itself prove that a physical target application responded correctly. Keep physical keyboard, touchpad, emergency-unlock, clipboard and recovery checks in the candidate acceptance record.

## Metadata fixtures

The old `perf-clipboard-lab.ps1`, `perf-file-transfer-lab.ps1`, and `perf-reconnect-input-soak-lab.ps1` generate synthetic observations for report-shape, redaction, and summary-math tests. A fixture row labeled `passed` means that its represented example passed; no runtime scenario or elapsed soak was executed.

`perf-two-machine-evidence.ps1` can still collect local metadata or summarize manually recorded observations. Its repo-target binary hashes are file snapshots, not evidence of running processes. All its packets remain developer diagnostics. `-ReleaseEvidence` now fails explicitly; re-summarizing a fixture cannot promote it to runtime evidence. Missing observation status also fails rather than silently becoming a pass.

```powershell
./scripts/dev/functional-validation-fixtures.ps1
./scripts/dev/perf-two-machine-fixtures.ps1
```

These fixture commands need no paired PCs, installed-product changes, or large payloads. The functional fixtures exercise rejection of missing samples, inconsistent percentiles/bytes, candidate mismatches, unsupported scope, retry storms, and disk-budget overruns, including the public validator's nonzero exit behavior.
