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
- MSI-owned updates are the supported payload update model: the MSI installer owns install, upgrade, repair, and uninstall of tray, daemon, and service payloads.
- Service and tray self-update modes are unsupported/deferred. Do not claim service-mode update parity from daemon/tray success alone.
- Release readiness records `service_update_ownership` and `n_minus_1_msi_upgrade` gates; stable policy fails if N-1 MSI evidence is skipped or malformed.

## Canonical Release Flow

1. Land Conventional Commit changes through normal PR review.
2. Let release-please prepare release metadata.
3. Run structural consistency: scripts/release/assert-release-consistency.ps1.
4. Produce a release-readiness packet: scripts/dev/release-readiness.ps1 -Policy stable.
5. Supply installer evidence with scripts/dev/installer-smoke.ps1 or an existing installer-smoke summary.
6. For stable MSI-owned update readiness, supply N-1 upgrade evidence with scripts/dev/installer-smoke.ps1 -InstallerPath <current-msi> -PreviousInstallerPath <prior-msi> -KeepArtifacts.
7. Supply service evidence with scripts/dev/service-smoke.ps1 when service-mode claims are in scope.
8. Publish only after the release-readiness packet is ready; stable policy fails failed, skipped, missing, or stale supported evidence.

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
| n_minus_1_msi_upgrade | open | Release-readiness now has an explicit gate, but the evidence still requires a prior GitHub Release MSI asset and Windows installer lab run. Stable packets fail when this evidence is skipped or malformed. | docs/release/v5-release-hardening.md, docs/release/release-readiness.md |
| service_update_orchestration | deferred | MSI-owned payload updates are the supported boundary; a tray notification or installer-launch UX remains future product work, and service/tray self-update remains unsupported. | this document |
| interactive_desktop_service_mode | deferred | Lock-screen and elevated-app behavior require Windows runtime evidence. | docs/release/v5-release-hardening.md |
| transport_fault_injection_harness | partially landed | PR #89 landed a narrow post-auth session fault harness; BND-NEXT-7 used it for behavior-neutral reactor cleanup, while broad multi-peer/runtime fault coverage remains deferred. | this document, docs/architecture/network-v1.md |
| mixed_dpi_input_matrix | deferred | Mixed-DPI and negative-coordinate monitor validation needs Windows hardware/runtime evidence. | this document |
| stale_non_file_evidence | deferred | Current strict freshness enforcement is file-summary based; external workflow/run freshness still depends on release review metadata. | docs/release/release-readiness.md |
| clipboard_image_spooling | deferred | BND-NEXT-8A bounded the measured outbound/local image allocation path; inbound/apply still materializes full BMP buffers and should become streaming or spooling work only with separate architecture evidence. | docs/performance/clipboard-image-memory.md |

## Next Backlog Step

- BND-NEXT-7 is complete at the behavior-neutral post-auth session reactor cleanup level. PRs #90-#93 landed the code and earlier docs boundaries; [docs/architecture/network-v1.md](architecture/network-v1.md) records the final architecture state.
- BND-NEXT-8A profiled clipboard image memory pressure and applies a bounded allocation fix for local/outbound large image paths; [docs/performance/clipboard-image-memory.md](performance/clipboard-image-memory.md) records the command, path map, evidence, and recommendation.
- BND-NEXT-9A makes MSI-owned service update readiness explicit in release-readiness evidence. Actual N-1 validation still requires current and prior MSI artifacts in a Windows installer lab.
- Full `SessionEvent`/`SessionPhase` machinery, broader multi-peer/runtime fault coverage, graceful per-session join lifecycle, clipboard image streaming/spooling, tray update notification UX, and feature/product behavior remain deferred.

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
| MSI-owned service update readiness | landed | release-readiness.ps1, release-readiness-fixtures.ps1 | Readiness records MSI-owned ownership and N-1 MSI upgrade evidence separately from unsupported service/tray self-update modes. |
| Full MSI-owned service updater | deferred | service_update_orchestration gap | MSI owns payload updates; tray notification/installer-launch UX remains future product work, and service/tray self-update is unsupported. |
| Full transport session reactor rewrite | partially landed | PRs #90-#93 and docs/architecture/network-v1.md | BND-NEXT-7 landed behavior-neutral post-auth boundaries: `SessionRuntime`, `SessionExitReason`, and named read/flush helpers. Full `SessionEvent`/`SessionPhase` machinery remains deferred. |
| Pairing listener admission hardening | landed | PR #88 | Admission caps, duplicate manual join handling, and pre-consumption capacity checks are implemented; broader setup UX remains deferred. |
| Runtime task supervisor | landed | PR #87 | Top-level task ownership, redacted health, and deterministic shutdown foundation are implemented; richer lifecycle policy remains follow-up reliability work. |
| Clipboard image memory optimization | partially landed | scripts/dev/profile-clipboard-image-memory.ps1; docs/performance/clipboard-image-memory.md | BND-NEXT-8A profiled synthetic 2 MiB and 8 MiB BMP payloads and bounded the outbound/local allocation path; inbound/apply full-buffer streaming or spooling remains deferred. |
| Explorer shell integration | deferred | product feature backlog | Requires installer/shell UX work after transfer readiness. |
| Clipboard status/policy UX | deferred | product feature backlog | Requires tray UX work; default clipboard content history remains out of scope. |
| Guided two-device setup | deferred | product feature backlog | Builds on pairing/status diagnostics; not part of this docs/validation task. |
| Broad reusable fault-injection harness | partially landed | PR #89 and transport_fault_injection_harness gap | Narrow post-auth session coverage exists; broad multi-peer/runtime fault coverage remains deferred outside BND-NEXT-7. |
| BND-PRO-3.3 remaining open work | deferred | clipboard_image_spooling gap; product feature backlog | BND-NEXT-8A profiled and bounded the outbound/local clipboard image allocation path. Remaining work is inbound/apply full-buffer spooling if future evidence warrants it, plus deferred product UX backlog items. |
