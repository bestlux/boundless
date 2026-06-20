# Boundless Project Status

This is the short, version-neutral repo status packet for agents and release workers. It should stay factual, current, and linked to canonical detail rather than becoming a roadmap.

## Current Stable Version

- Stable version: 5.0.0
- Source of truth: workspace Cargo.toml, .release-please-manifest.json, and packaging/windows/package-manifest.json
- Primary release artifact: Windows MSI, plus release metadata and checksums from the release workflow

## Support Posture

- Product target: Windows-first.
- Public status: pre-release software with a stable 5.0.0 release artifact, but runtime behavior and APIs may still change.
- Cross-platform posture: Linux build/test coverage exists, but Windows runtime, tray, input capture, installer, named-pipe, and service behavior are not implied by Linux success.
- Canonical first-run UX: tray dashboard with local daemon. CLI setup and console flows are automation and diagnostics fallbacks.

## Service Mode Boundary

- Default local control endpoint on Windows is npipe://./pipe/boundlessd-api.
- Service mode is optional/admin-owned. MSI service-mode installation is not enabled by default.
- Service validation must cover installed service binary version, path, ACLs, named-pipe/API health, process counts, and install/start/status/stop/uninstall behavior.
- A matching boundless-service.exe --version result is required stable-release evidence when installer smoke evidence is supplied.
- Full MSI-owned service update orchestration remains deferred; do not claim service-mode update parity from daemon/tray success alone.

## Canonical Release Flow

1. Land Conventional Commit changes through normal PR review.
2. Let release-please prepare release metadata.
3. Run structural consistency: scripts/release/assert-release-consistency.ps1.
4. Produce a release-readiness packet: scripts/dev/release-readiness.ps1 -Policy stable.
5. Supply installer evidence with scripts/dev/installer-smoke.ps1 or an existing installer-smoke summary.
6. Supply service evidence with scripts/dev/service-smoke.ps1 when service-mode claims are in scope.
7. Publish only after the release-readiness packet is ready; stable policy fails failed, skipped, missing, or stale supported evidence.

The version-neutral release packet contract lives in [docs/release/release-readiness.md](release/release-readiness.md). The release-hardening limits live in [docs/release/v5-release-hardening.md](release/v5-release-hardening.md).

## Validation Ladder

Use scripts/dev/check.ps1 -Area <area> -Format json for machine-readable area checks. The current high-value areas are:

| area | underlying command | normal use |
| --- | --- | --- |
| workspace | scripts/dev/test-suite.ps1 -Profile quick | fmt, workspace tests, Clippy |
| smoke | scripts/dev/test-suite.ps1 -Profile smoke | workspace plus two-node smoke |
| installer | scripts/dev/installer-smoke.ps1 | MSI install/startup/uninstall evidence |
| service | scripts/dev/service-smoke.ps1 | elevated Windows service-mode evidence |
| release | scripts/dev/release-readiness.ps1 -Policy stable | release packet and evidence gate |
| docs/status | status and component-map checks | agent context freshness |

Removed transitional wrappers:

- Installer smoke compatibility wrapper was removed. Use scripts/dev/installer-smoke.ps1 or scripts/dev/check.ps1 -Area installer.
- Broad validation compatibility wrapper was removed. Use scripts/dev/test-suite.ps1 for profile runs or scripts/dev/check.ps1 for area checks.
- Release readiness compatibility wrapper was removed. Use scripts/dev/release-readiness.ps1 -Policy stable and version-neutral readiness packets.

## Current Architecture Links

- [Architecture component map](architecture/component-map.md)
- [Network architecture map](architecture/network-v1.md)
- [Security and trust model](security-trust-model.md)
- [Mouse Without Borders parity matrix](parity/mouse-without-borders.md)
- [Release readiness packet](release/release-readiness.md)
- [V5 release hardening](release/v5-release-hardening.md)

## Known Validation Gaps

These are not hidden release blockers by default; they require explicit release-review treatment when their claims are in scope.

| gap_id | status | rationale | recorded in |
| --- | --- | --- | --- |
| n_minus_1_msi_upgrade | deferred | Requires a prior MSI artifact and Windows installer lab evidence; stable release packets must record the prior version used or a skip rationale. | docs/release/v5-release-hardening.md |
| service_update_orchestration | deferred | Full MSI-owned service updater is larger than release-readiness evidence and remains outside the default MSI service boundary. | this document |
| interactive_desktop_service_mode | deferred | Lock-screen and elevated-app behavior require Windows runtime evidence. | docs/release/v5-release-hardening.md |
| transport_fault_injection_harness | deferred | Broad reusable multi-peer fault injection is useful test infrastructure but is larger than the validation ladder. | this document |
| mixed_dpi_input_matrix | deferred | Mixed-DPI and negative-coordinate monitor validation needs Windows hardware/runtime evidence. | this document |
| stale_non_file_evidence | deferred | Current strict freshness enforcement is file-summary based; external workflow/run freshness still depends on release review metadata. | docs/release/release-readiness.md |

## Pro Oversight Item Accounting

| original point | state | current owner or evidence | notes |
| --- | --- | --- | --- |
| Persistence and input retry safety foundation | landed | PR #76 | No further action in BND-PRO-3.3. |
| Bounded outbound transfer cursor | landed | PR #77 | No further action in BND-PRO-3.3. |
| Service version release-readiness gate | landed | PR #78 | This status doc records the service-mode boundary. |
| Inbound file finalization and credits | landed | PR #79 | No further action in BND-PRO-3.3. |
| Redacted diagnostic bundle export | landed | PR #80 | Future diagnostics docs can link here without expanding this task. |
| Pairing trust recovery UX | landed | PR #81 | Guided setup remains deferred as a larger UX feature. |
| Transfer Center MVP | landed | PR #82 | Resumable transfers remain intentionally deferred. |
| Keep high-signal workspace, smoke, installer, service, release checks | addressed by this work | scripts/dev/check.ps1, this validation ladder | Uses existing scripts underneath. |
| Remove installer smoke compatibility wrapper | landed | installer-smoke.ps1, check.ps1 -Area installer | Transitional wrapper deleted after docs/automation migrated. |
| Remove broad validation compatibility wrapper | landed | test-suite.ps1, check.ps1 | Canonical commands are test-suite.ps1 and check.ps1. |
| Replace version-specific release readiness naming | landed | release-readiness.ps1 | Stable policy is now named as -Policy stable. |
| Add project status and component map | addressed by this work | this file and docs/architecture/component-map.md | These are the canonical agent context docs. |
| Make release evidence self-describing | landed and addressed by this work | release-readiness.json, release-readiness.md, release_policy, gate reasons/impacts | Stable policy fails failed/skipped/missing/stale supported evidence. |
| Full MSI-owned service updater | deferred | service_update_orchestration gap | Requires product/installer work, not docs-only readiness. |
| Full transport session reactor rewrite | deferred | component map/network architecture | High-risk refactor should be test-driven separately. |
| Pairing listener admission hardening | deferred | component map pairing row | Needs focused runtime tests and limits work. |
| Runtime task supervisor | deferred | component map daemon/runtime rows | Broader task ownership model is out of scope here. |
| Clipboard image memory optimization | deferred | component map clipboard row | Requires runtime profiling and targeted implementation. |
| Explorer shell integration | deferred | product feature backlog | Requires installer/shell UX work after transfer readiness. |
| Clipboard status/policy UX | deferred | product feature backlog | Requires tray UX work; default clipboard content history remains out of scope. |
| Guided two-device setup | deferred | product feature backlog | Builds on pairing/status diagnostics; not part of this docs/validation task. |
| Broad reusable fault-injection harness | deferred | transport_fault_injection_harness gap | Valuable test infrastructure but larger than BND-PRO-3.3. |
| BND-PRO-3.3 remaining open work | open | none after this docs/validation change | Future work should be tracked as separate product/runtime/test issues. |
