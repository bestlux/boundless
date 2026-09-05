# Product Backlog

This is the ordered work needed to turn the current Windows implementation into a supportable public product. The [roadmap](v5-roadmap.md) explains the outcomes; the [product matrix](parity/mouse-without-borders.md) owns capability and evidence status. Historical BND-NEXT briefs remain in the [archived backlog](history/2026-09-04/backlog.md).

Status is deliberately separated from release readiness: **active** means implementation or integration is underway; **open** means required work remains; **deferred** means excluded from the current train. A source change or passing local test does not close a physical acceptance criterion.

## Implemented Hardening Candidate

| Work | Required behavior | Completion evidence |
| --- | --- | --- |
| Bounded logging | Bound serialized records, queue payload, files, retained bytes, age, and write-failure retries. Apply separate budgets to daemon runtime and service startup streams; recognize old oversized log files during cleanup. | Rotation/retention/queue/disk-failure tests; current log-path inventory; real absent-peer soak. A per-stream limit must not be described as a machine-wide budget across every user profile. |
| Peer retry and lifecycle isolation | Use absolute per-peer retry deadlines; bound pre-auth admission and task lifetimes; service transport reads continuously during bulk writes; make connected state volatile and owned by the live session. | Deterministic immediate-refusal/timeout/cancellation tests; bounded duplex and stalled-reader tests; one unhealthy peer cannot block a healthy peer; restart never reloads a false connected state. This incorporates the useful BND-NEXT-42/43 work. |
| User-I/O authority | Open user-selected file sources, destinations, and exports under a revocable selected-console-user token lease. Refuse the operation if the correct unelevated user authority is unavailable. | Source/destination permission and lease-loss tests; no SYSTEM fallback path opens; installed user-folder proof. Preserve the fixed-SID install contract in this train. |
| Input process lifetime | Authenticate broker PID and creation time. Retain receipts only for the same verified incarnation; a new incarnation fails open and releases held state without stale input replay. | PID-reuse/reconnect/replacement and uncertain-delivery tests; physical crash/escape/relaunch/next-handoff matrix. Retains the safety intent of BND-NEXT-31/38/40/44. |
| Tray product flow | Make common status/pairing/layout actions clear, provide local pause, expose degraded capabilities, and remove or disable controls whose behavior is not implemented. | Windows rendering and focused state/interaction checks; physical confirmation that Pause and recovery labels match runtime behavior. BND-NEXT-33's broad design goals are reduced to current user tasks. |
| Peer-approved transport testing | Add a short, bounded, memory-only test run over existing authenticated peer transport, with explicit peer approval and stop/expiry behavior. | Protocol/admission/expiry/revocation/resource tests and reports identifying actual measured transport samples. Wire 4.5 compatibility and both-peer upgrade requirements are explicit. This does not measure physical input or clipboard integration. |
| Honest gates and release channels | Classify layout tests as unit evidence, validate actual sample-based reports, separate previews from public promotion, and preserve required Windows/installer evidence. | Negative evidence fixtures, gate tests, source validation, and prerelease/not-latest release configuration. No claim that a release has been published. |

All rows have implementation and local evidence in the [dated verification record](validation/windows-hardening-2026-09-04.md). Their installed and physical acceptance remains open. The app manifests still identify 5.0.16; candidate `5fa97d8` and its artifact hashes distinguish this work from the published release. Current installed versions are not inferred from manifests.

## Required Installed And Physical Evidence

Run one coherent matrix against compatible, identified builds on both PCs. Record exact versions/hashes, Windows/session context, scenario, outcome, and remaining risk. The [launch ledger](release/launch-ledger.md) preserves older observations; do not silently upgrade them to proof of the new candidate.

- Leave a paired peer unavailable and verify bounded logs, CPU, memory, task count, quiet UI, and recovery when it returns. Include sleep/resume and repeated service/tray restarts in the planned reliability evidence.
- Establish actual Windows release-build baselines for per-process idle CPU/private bytes, connect/handoff/recovery latency, input progress under bulk contention, warmed UI frame CPU, and peak log bytes/drop counters. Define and enforce regression budgets before claiming fastest/lightest behavior or comparing another app.
- Exercise emergency escape, local pause, forced tray/broker/injector loss, held keys/buttons, Quit/relaunch, and the next successful handoff.
- Prove touchpad wheel behavior, conventional wheel, numpad with opposite Num Lock states, mixed-DPI/multi-display handoff, and ordinary elevated-window capability within its supported scope.
- Test text, small bitmap, policy-boundary bitmap, oversized image rejection, feature isolation, continuous input during clipboard failure, and redacted diagnostic output.
- Test explicit file transfer in both directions using user-visible folders, permission denial, cancellation, collision handling, and lease loss. Default-deny or unavailable authority must produce an actionable outcome.
- Prove the current installer path's running-product upgrade, repair, service/API health, process counts, and uninstall. When the helper is retired, carry forward these product outcomes rather than obsolete helper-specific choreography.

## Next Product Work

### 1. Replace The Install Contract — BND-NEXT-45D–F

The 5.0.16 baseline contains cooperative shutdown, stable CLI JSON, and `doctor --install`. Build on those enablers to remove installation-time SID capture, make a plain MSI the canonical installer, and delete the install helper and redundant fixtures.

Preserve the selected-user authorization property across console-user changes and no-session states. Acceptance is fresh install, silent install, running-product upgrade, same-version replacement where supported, repair, and uninstall with healthy documented postconditions. A lower script line count is useful, but it is not the acceptance test.

### 2. Simplify Privilege And Process Ownership

Review the service, user broker, injector, and tray responsibilities after the safety changes are integrated. Keep privileged surfaces minimal and revocable. Remove duplicate state and lifecycle compensation when one owner can enforce the same invariant. Preserve the explicit ordinary-elevated-app boundary; do not expand into secure desktops or other sessions as incidental refactoring.

### 3. Complete Setup And Recovery — BND-NEXT-27, 20E, 21/45G, 22

Make trust rotation/reset a first-class flow against the actual runtime profile. Explain manual-host fallback, one-way discovery, incompatible versions, and port collisions. Decide and implement the installer-owned firewall policy explicitly, including scope, consent, repair, and uninstall ownership. A fresh two-PC user must reach a working layout without shell diagnosis.

### 4. Complete File Consent And One Explorer File — BND-NEXT-36

Add actionable receipt consent or per-peer opt-in, a user-visible receive folder, and one local regular-file copy/receive workflow. Enforce the file-sharing setting. Show rejection, unsupported formats, progress, cancellation, collisions, and cleanup. Keep folders, multiple files, network paths, resumable transfer, and general shell integration deferred.

### 5. Qualify The Public Windows Contract

Define the supported Windows/session/display matrix, resolve signed distribution and public support policy, and complete repeated installed evidence. Prove four computers total before claiming that topology. Consider automatic layout and extra settings only when actual onboarding/use evidence supports the added complexity.

Treat measured efficiency as part of this qualification. Current sink-throughput, broker-stage, duplex, paired-TLS, and egui-frame measurements are local implementation evidence. They do not establish the full Windows release-build resource/latency baseline or a competitor advantage; those need comparable actual-product runs and explicit regression budgets.

### 6. Shrink CI And Release Orchestration — BND-NEXT-32

After the installer surface shrinks, consolidate duplicated orchestration while preserving the risks each gate covers. Keep fast Rust and focused contract checks, matching Windows integration/installer checks, and explicit physical release evidence. Engineering prereleases and public promotion must stay distinguishable.

## Deferred And Retired Work

Audio/virtual microphones, cloud/relay transport, networking-stack migration, cross-platform feature parity, and remote desktop are outside this plan. Larger file workflows, lock-screen/secure-desktop control, automatic updates, and speculative image spooling need separate evidence and scope decisions.

The old helper-quiescence implementation path is retired as a future direction; its shutdown, bounded-upgrade, and recovery outcomes remain required. Superseded BND-NEXT briefs and earlier feature/release trains are preserved [verbatim in history](history/2026-09-04/backlog.md). Use them for rationale and regression cases, not as a second ranked backlog.
