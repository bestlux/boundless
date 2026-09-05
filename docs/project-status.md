# Boundless Project Status

Current context for implementers and release reviewers. Use the [docs index](README.md) to find user instructions, architecture, product scope, and evidence policy without treating older milestone pages as current authority.

## Release Baseline

- Baseline source: `9bad1b5c36e31aa81f8764e72fed66785f85c05a`, release 5.0.16, dated 2026-07-15 in [CHANGELOG.md](../CHANGELOG.md).
- Workspace, release-please, and Windows package manifests remain 5.0.16 during this hardening train. A source branch is not evidence of an installed version.
- Boundless remains pre-release software. A published numeric release is not a qualified public Windows product or full Mouse Without Borders parity.
- The active peer-test protocol work targets wire 4.5 and is incompatible with 4.4; both peers must use compatible builds. This does not change the historical 5.0.16 release artifact.
- Configuration schema 6 drops live connection/last-seen observations while migrating durable schema-5 data. Rollback requires a private pre-upgrade state backup and compatible old peers; see [migration guidance](user/migration.md).
- No new publication, installed-app change, or physical two-PC validation is implied by this status update.

## Support Posture

Windows is the product target. The first public beta target is two PCs; four computers total needs separate runtime qualification. Linux compile/test coverage does not establish desktop input, clipboard, tray, installer, service, or named-pipe parity.

The tray is the normal user entry point. The CLI is for diagnostics, automation, and recovery. [The capability matrix](parity/mouse-without-borders.md) is the authoritative feature/evidence inventory; [the roadmap](v5-roadmap.md) and [backlog](backlog.md) define intended changes.

## Service Mode Boundary

The machine-wide MSI installs `BoundlessService` as LocalSystem with automatic start, using the MSI-owned Program Files payload. The current installation contract still supplies a fixed selected desktop-user SID. The default local control endpoint is `npipe://./pipe/boundlessd-api`.

The service host does not itself prove interactive-desktop behavior. Input/clipboard use a user-session broker; current hardening ties input ownership to a verified broker incarnation and user file operations to a revocable selected-console-user authority lease. Those changes do not replace the installation-time SID contract yet.

Ordinary elevated-application input has an explicit helper/capability boundary. The released unsigned dogfood exception is not a public trusted-publisher, UIAccess, lock-screen, UAC secure-desktop, alternate-admin, or other-session guarantee. See [service-mode guidance](user/service-mode.md), [the broker architecture](architecture/user-session-input-broker.md), and [the trust model](security-trust-model.md).

## Current Work

The locally validated hardening candidate implements bounded logs, per-peer retry/session ownership and duplex progress, user-authorized file I/O, verified input process lifetime, a smaller truthful tray flow with local pause, peer-approved transport testing, and evidence/release-policy accuracy. These are targeted refactors of the existing Rust architecture.

The [dated verification record](validation/windows-hardening-2026-09-04.md) identifies implementation `5fa97d8`, 773 passing workspace tests, the three review rounds, optimized local benchmarks and the built unsigned MSI. The candidate has not been installed or physically qualified. The remaining outcome and acceptance list lives in [the backlog](backlog.md), rather than a duplicated completion table here.

## Canonical Release Flow

Use Conventional Commits and release-please for version metadata. Validate source and the matching Windows package before producing a candidate. Use [release readiness](release/release-readiness.md) for the gate contract and [the product scorecard](performance/product-scorecard.md) to interpret actual measured behavior.

Engineering releases are intended to be explicitly marked prerelease and not latest. Public promotion requires its own decision, known artifact provenance, installation/upgrade evidence, accurate capability claims, and the required real Windows observations. This documentation change does not publish or promote a release.

The smallest applicable checks remain the preferred development loop. Rust behavior changes require formatting, workspace Clippy with warnings denied, and workspace tests; Windows runtime/installer changes also need matching PowerShell validation. Layout unit tests, test fixtures, paired transport probes, physical input tests, and installed-product tests are different evidence classes.

## Known Validation Gaps

| Area | Current boundary |
| --- | --- |
| Work-PC disk-exhaustion report | The work PC's version and exact oversized file are unknown. The current logging/retry investigation and hardening must not be represented as a reproduced diagnosis of that specific installation. |
| Unattended operation | Bounded source mechanisms and local fault tests still need real extended resource/reconnect evidence. Log budgets are per stream/security context, not a universal quota across every Windows profile. |
| Input and clipboard | Older two-PC successes and failures are recorded in the launch ledger. Current physical escape, numpad, touchpad, clipboard-boundary, fault-isolation, and next-handoff proof remains open. |
| User file authority | Candidate authorization and permission tests do not substitute for installed user-folder, session/lease-loss, and both-direction transfer validation. |
| Elevation | Ordinary same-user elevated windows need the exact supported capability and installed evidence; secure desktops, other sessions, and alternate-admin control remain unsupported. |
| Installer | The fixed-SID helper contract remains. Plain-MSI replacement and runtime desktop-user authorization are future work; current installed upgrade/repair/uninstall claims require matching evidence. |
| Topology and displays | Four-PC, mixed-DPI, and multi-display runtime qualification remains open. The validator's four-remote-peers-plus-local limit is only a configuration capability. |
| File product flow | Default-deny/global trusted-peer opt-in is not per-peer consent. Explorer copy/paste and public drag/drop claims remain incomplete or deferred. |
| Peer testing | A passing bounded, peer-approved test demonstrates measured transport behavior only; it cannot prove physical capture/injection, clipboard integration, or full product readiness. |
| Performance and UX | Local real-sink, input-stage, duplex, paired-TLS, and warmed egui-frame measurements exist. Actual Windows release-build idle CPU/private bytes, connect/handoff/recovery, contention, frame CPU, and logging baselines/regression budgets remain required. No competitor speed advantage is proven. |

Keep [the launch ledger](release/launch-ledger.md) as dated evidence. Earlier oversight accounting and release-train status are [archived](history/2026-09-04/project-status.md), not additional current-state requirements.
