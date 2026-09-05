# Windows Product Roadmap

Boundless is working toward a polished public Windows alternative to Mouse Without Borders. Keep the Rust workspace and its useful input, trust, transport, and transfer foundations. Refactor the boundaries that prevent safe daily use; do not restart the entire application.

The first public beta target is two Windows PCs. Four computers total is a later supported topology, subject to its own runtime evidence. Neither target is a claim that the current release is qualified. Current capability and evidence status live in the [product matrix](parity/mouse-without-borders.md); concrete work lives in the [backlog](backlog.md).

## Outcomes, In Order

| Outcome | Scope | Acceptance before moving the claim forward |
| --- | --- | --- |
| 1. Safe unattended operation | Bounded disk logs, retry scheduling, connection/task lifetimes, queues, and independent peer progress | An absent or failing peer cannot create unbounded disk use, sustained busy retries, unbounded tasks, or stale connected state. Fault tests cover immediate refusal, timeout, cancellation, write failure, and restart; an actual extended run measures resource use. |
| 2. Local control always recovers | Verified broker process identity, input delivery lifetime, local pause, escape, detach, and held-input release | Tray/broker/injector loss and replacement cannot replay stale input or strand local control. Deterministic recovery tests pass, followed by physical keyboard/mouse, emergency escape, and next-handoff proof on both PCs. |
| 3. User data stays under user authority | File source/destination access and user-selected exports use a valid selected-console-user lease; service operations fail closed when it is unavailable | User file operations never fall back to SYSTEM authority. Tests cover lease loss, wrong user, source/destination permissions, and partial cleanup. Installed proof verifies user-visible folders and both transfer directions. |
| 4. The app tells the truth | Clear connection/capability state, usable pairing/layout/recovery, local pause, and removal or disabling of controls without runtime behavior | Rendered Windows UI and state tests cover fresh, connected, offline, degraded, and unsupported states. A setting's effective behavior matches its label. A paired transport test reports only what it measured. |
| 5. Installation is a normal Windows workflow | Product-owned health checks, runtime desktop-user authorization, plain MSI, helper deletion, and an explicit firewall policy | Fresh install, running-product upgrade, repair, and uninstall work without shell choreography; trust/layout recover as documented. Authorization remains narrow across user switching. Signing and distribution policy are resolved before public promotion. |
| 6. Everyday workflows are complete | Text and supported-size images, visible file receipt/consent, one local Explorer file, simple two-PC layout | Physical two-PC input/clipboard tests and user workflows pass on a known candidate. Receipt uses a user-visible folder and an actionable consent policy; unsupported formats have a visible outcome. |
| 7. Support claims match real use | Repeated installed evidence, multi-monitor/mixed-DPI coverage, elevated-app scope, and four-PC topology | Each public claim has exact-build Windows evidence. Four-PC handoff/reconnect and peer failure are exercised before advertising four-PC support. Support bundles are bounded and redacted. |
| 8. Fast, light daily use is measured | Idle resources, connection/handoff/recovery latency, bulk contention, UI frame CPU, and logging cost | Establish actual Windows release-build baselines and explicit regression budgets on supported hardware. Repeat the same scenarios after changes; comparative speed claims require equivalent competitor measurements. |

Outcomes 1–4 are the active hardening train. Their implementation and local tests do not close the installed or physical acceptance in this table. The current installation-time selected-user SID remains in place until outcome 5 replaces that contract deliberately.

## Architecture Direction

Preserve shared product commands/models, explicit peer trust, TLS transport, Windows user-session input, and the transfer policy primitives. Narrow ownership inside those boundaries: bounded logging sinks, per-peer connection/session lifetimes, continuously serviced transport reads, revocable user-I/O authority, and input receipts tied to a verified process incarnation.

Keep the tray focused on interaction. It should present supported capabilities and invoke shared operations, not acquire more workflow or privileged orchestration logic. Revisit the process/privilege model after the current safety work so unnecessary broker, injector, service, and installer complexity can be removed with evidence.

This direction does not require a new networking stack or UI framework. Replace a module when its current ownership prevents a tested invariant; preserve useful behavior and regression evidence around that replacement.

## Performance And UX Acceptance

The fastest/lightest goal requires reproducible measurements of the actual Windows product. Use [the product scorecard](performance/product-scorecard.md) to record build identity, hardware, Windows/session context, scenario, measurement method, sample distribution, and the chosen regression budget.

- Measure idle CPU and private bytes for every participating process, including the offline-peer state.
- Measure connection, handoff, and recovery latency, plus input progress while another peer stalls or bulk traffic is active.
- Measure warmed UI frame CPU and visible responsiveness during refresh, transfer progress, and degraded states.
- Measure peak retained log bytes, bounded queue use, dropped-record counters, and sink cost under normal and repeated-failure load.

The current local evidence includes a real logging-sink throughput benchmark over a 256 MiB cached workload, full-duplex/stalled-peer transport tests, input-broker stage measurements, paired TLS RTT/echo-integrity probes, and warmed egui frame CPU measurements. These establish useful implementation baselines. They do not establish physical handoff latency, whole-product idle footprint, or comparative speed against another app. Actual Windows release-build runs and explicit regression budgets remain required before performance claims.

## Release And Evidence Boundary

The manifests remain at 5.0.16 while this source work is being integrated. The peer-approved test protocol change targets wire 4.5; it is incompatible with 4.4 and requires both peers to run compatible builds. It does not establish that either PC has been upgraded.

Configuration schema 6 deliberately stops persisting live observations such as connected state and last-seen time. Migration from schema 5 preserves durable configuration/trust data while dropping those observations. Rollback requires a private pre-upgrade state backup and compatible old peers; see the [migration guide](user/migration.md) and [network architecture](architecture/network-v1.md).

Use [release readiness](release/release-readiness.md) for automated gate policy and [the product scorecard](performance/product-scorecard.md) for the meaning of measured results. A generated fixture, layout unit test, or successful transport probe cannot prove physical input capture, Windows injection, clipboard integration, or installed-product readiness.

No physical two-PC validation or installed-app changes are included in the current hardening implementation. Automatic engineering releases are intended to be prereleases; public promotion requires an explicit decision and the matching evidence. Do not turn a numeric version or green CI into a general-availability claim.

## Deferred Scope

- Lock-screen, UAC secure-desktop, other-session, and alternate-admin control remain unsupported. Ordinary elevated-app control has its own limited contract.
- Folders, multiple files, network-file clipboard workflows, resumable transfer, and public drag/drop claims follow the single-local-file workflow.
- Automatic cluster layout convergence, fullscreen exception rules, anti-idle enhancements, and additional hotkeys follow the core workflows and their measured usability needs.
- Audio routing, virtual microphone drivers, cloud relay, screen streaming, multicast input, and Linux/macOS feature parity are outside this Windows release plan.

The [superseded V5 roadmap](history/2026-09-04/v5-roadmap.md) preserves the earlier scope and sequencing. It is historical context, not an additional release contract.
