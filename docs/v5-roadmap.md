# Boundless v5 Roadmap

## Goal

Ship Boundless v5 as a Windows-first, release-grade multi-PC control product that exceeds Mouse Without Borders by combining reliable keyboard/mouse handoff, clipboard and file workflows, validated service-mode support, richer diagnostics, stronger trust posture, and an operator-grade tray experience.

V5 should close the current parity gaps while preserving Boundless' architecture: a daemon owns runtime state, shared crates own product logic, IPC DTOs define the surface, the tray is the canonical first-run UX, and CLI/console remain diagnostics and automation fallbacks.

V5 is still not:

- a remote desktop product,
- screen sharing,
- a multi-user SaaS,
- a general file sync tool,
- a privileged remote administration tool,
- a cross-platform parity promise,
- or a security boundary against a compromised local administrator.

## Ideal V5 End-State

At v5, Boundless should feel better than Mouse Without Borders for a Windows power user:

- A normal user can install Boundless, launch the tray, pair two to four Windows machines, arrange them visually, and move keyboard/mouse control across edges without reading CLI docs.
- The Windows service path supports elevated applications and lock-screen scenarios where the operating system allows it, with clear status and recovery when service mode is unavailable.
- Clipboard sharing handles text, bitmap images, and file copy workflows with explicit size limits, progress, failure reporting, dedupe, and user-visible history.
- File transfer is a product workflow, not only transport plumbing: users can copy/paste or send files, see progress, choose the receive folder, and recover from partial transfer failures.
- Input handoff is low-latency and predictable across mixed DPI, multi-display, corner-blocking, wrap/no-wrap, relative/absolute movement, and cursor visibility settings.
- Pairing is safer and smoother than shared-key setup: discovery, challenge confirmation, trust rotation, manual fallback, and clear diagnostics are all first-class.
- The tray dashboard exposes the full settings surface and can explain connection health, firewall/subnet problems, service status, and feature state without hiding warnings.
- Runtime telemetry is local, bounded, and actionable: events, latency summaries, reconnect causes, clipboard/file failures, and input-drop counters are visible in CLI and tray.
- Release validation proves four-node topology, service install/uninstall, elevated-app behavior, clipboard/file workflows, and upgrade recovery on Windows.

The honest v5 promise is **release-grade Windows multi-PC control with stronger diagnostics, safer pairing, and better failure behavior than Mouse Without Borders**, not universal remote control or hard security isolation.

## Product Principles

- Keep the tray dashboard as the happy path; keep CLI and console as power tools.
- Prefer explicit trust and visible health over silent best-effort connection behavior.
- Make every user-facing feature reflect a real runtime path, not only protocol/state plumbing.
- Keep shared behavior out of the tray when it belongs in `app-services`, `ipc-api`, `adapter-ipc-grpc`, `daemon`, or `core-*`.
- Treat Windows service mode as a capability with prerequisites and failure states, not as a magic elevation guarantee.
- Fail closed for unsafe file names, oversized payloads, untrusted peers, incompatible protocol versions, and ambiguous pairing.
- Default file receipt to explicit consent or explicit per-peer opt-in; a paired peer is not enough authority to write files silently.
- Give users predictable control over edge switching, wrap behavior, corner blocking, clipboard/file sharing, and service mode.
- Keep local operation offline-capable. Network discovery can help, but manual pairing and manual endpoints must remain viable.
- Make diagnostics specific enough that support can distinguish config, firewall, service, named-pipe, TLS trust, discovery, and runtime bugs.
- Do not claim cross-platform parity until non-Windows capture, injection, tray, installer, and control endpoint behavior are validated.

## Current Baseline

Boundless already has important v5 foundations:

- Rust workspace with daemon, IPC API, gRPC adapter, CLI, Windows glue, tray UI, and shared `core-*` crates.
- Windows tray dashboard with pairing, layout, settings, reconnect, close-to-tray, and daemon auto-start attempts.
- CLI setup/console flows for daemon health, discovery, pairing, layout, feature toggles, diagnostics, input owner/capture target, and pending pairing requests.
- Trust-bundle and nearby challenge-confirm pairing.
- TLS transport with heartbeat, reconnect generation, backpressure, chunked clipboard image transfer, file chunk credit flow, and protocol-version checks.
- Windows clipboard runtime for text and bitmap image watch/apply with echo suppression.
- Windows input capture via low-level hooks with polling fallback, and Windows injection via `SendInput`.
- Layout-driven edge handoff behind `easy_mouse` and `wrap_mouse`.
- Per-user MSI packaging with tray, daemon, CLI, reset helper, shortcuts, release signing hooks, and installer smoke validation.

The key current caveat remains: protocol/state plumbing does not by itself make clipboard, file transfer, anti-idle, or input routing product-ready.

## V5 Workstreams

### V5-1: Parity Contract And Product Readiness Matrix

Why:

Boundless needs a durable definition of what Mouse Without Borders parity means and where Boundless intentionally exceeds it. Without a contract, implementation will drift between low-level runtime fixes and broad product claims.

Deliverables:

- `docs/parity/mouse-without-borders.md` with one row per feature and setting.
- Status values: `not-started`, `plumbing`, `cli-ready`, `tray-ready`, `validated`, `deferred`, `out-of-scope`.
- Columns for Mouse Without Borders behavior, Boundless target behavior, owner crate/module, CLI surface, tray surface, validation command, and release blocker status.
- Initial rows for:
  - up to four computers,
  - keyboard/mouse sharing,
  - device layout,
  - refresh/reconnect,
  - service/elevated/lock-screen support,
  - wrap mouse,
  - share clipboard,
  - transfer files,
  - hide cursor at edge,
  - draw cursor,
  - validate remote IP,
  - same subnet only,
  - block screen saver,
  - relative mouse movement,
  - block screen corners,
  - status notifications,
  - easy mouse edge switching,
  - pairing/security key equivalent,
  - install/upgrade/uninstall.
- Release checklist that links every v5 workstream to the matrix rows it completes.

Implementation notes:

- Place the matrix under `docs/parity/` rather than `docs/architecture/`; it is a product contract, not only architecture.
- Treat existing README claims as evidence only when backed by code or validation.
- Update the matrix in every v5 feature PR.

Done when:

- A reviewer can answer "what is still missing for v5 parity?" from one document.
- Every public v5 claim is classified as validated, deferred, or out-of-scope.
- The matrix is referenced by README, release notes, and the v5 readiness packet.

### V5-2: Four-Machine Topology And Layout UX

Why:

Mouse Without Borders advertises control of up to four computers. Boundless has multi-peer layout primitives, but v5 needs a validated product path for two, three, and four machines.

Deliverables:

- Explicit four-machine support contract in config validation and docs.
- Tray layout editor that supports one-row, grid, and freeform cardinal layouts for up to four peers plus local machine.
- CLI layout commands that validate the same constraints as tray.
- Clear behavior for duplicate local cells, disconnected peers, hidden peer chains, ambiguous display names, and unreachable layout edges.
- `switch_all` and edge handoff behavior validated with two, three, and four nodes.
- Topology export/import for support diagnostics without secrets.

Implementation notes:

- Keep layout parsing and validation in daemon/app-service/shared helpers, not duplicated in tray.
- Extend existing layout tests under `crates/daemon/src/state/tests/layout_and_validation.rs`.
- Extend tray layout tests under `crates/tray/src/dashboard/layout.rs` and related dashboard tests.
- Add a four-node smoke profile only if the unit tests cannot prove runtime ordering and reconnect behavior.

Done when:

- A user can arrange up to four machines in tray and apply layout without CLI.
- Edge handoff works across all cardinal directions, wrap mode, and return-to-local paths.
- Disconnected peers are skipped or surfaced predictably, not silently selected.
- Validation covers at least two-node, three-node, and four-node topology behavior.

### V5-3: Windows Service Mode For Elevated And Lock-Screen Scenarios

Why:

Mouse Without Borders has service mode for elevated apps and lock-screen control. Boundless currently uses a per-user tray-first install and daemon auto-start. V5 needs a real service story with honest OS-boundary documentation.

Deliverables:

- Windows service install/status/start/stop/uninstall commands.
- MSI option or separate admin command to install the service.
- Service-owned daemon process model with a clear boundary between service runtime and tray UI.
- Named-pipe or local IPC ACL model that allows tray/CLI control without exposing remote write access.
- Service status in tray and CLI: installed, running, stopped, incompatible, permission denied, stale pipe, elevated-required.
- Elevated-app input injection validation.
- Lock-screen support validation where feasible; if not feasible, document exact OS boundary and mark it not implemented.
- Upgrade and uninstall behavior that stops old service instances and prevents duplicate daemon/tray processes.

Implementation notes:

- Add a dedicated service host crate only if `crates/daemon-host` cannot own service-specific lifecycle cleanly.
- Keep platform-specific service code in `crates/platform-windows` or a Windows-only service crate.
- Avoid making the tray a service manager with business logic; tray should call stable IPC commands.
- Installer changes must update `packaging/windows/package-manifest.json`, WiX files, release consistency checks, and installer smoke.

Done when:

- Boundless can run in normal per-user mode and service mode with clear status.
- Elevated application control is validated on Windows.
- Lock-screen support is either validated or explicitly classified as not implemented with a concrete reason.
- Service install, upgrade, restart, and uninstall are covered by PowerShell validation.

### V5-4: Input Handoff Excellence

Why:

The central product promise is seamless control. Boundless must be better than "it mostly switches"; it must be predictable across real Windows desktop setups.

Deliverables:

- Low-latency input path budgets for capture-to-send, receive-to-inject, and end-to-end handoff.
- Settings and runtime behavior for:
  - easy mouse edge switching,
  - wrap mouse,
  - block corners,
  - relative mouse movement,
  - hide cursor at edge,
  - draw remote cursor or cursor marker where useful,
  - release/escape unlock,
  - keyboard hotkeys for reconnect, switch all, toggle easy mouse, lock machine.
- Multi-monitor and mixed-DPI handoff logic.
- Absolute and relative movement modes with clear fallback behavior.
- Input drop/backpressure counters surfaced in diagnostics.
- Recovery behavior when hooks fail and polling fallback is active.

Implementation notes:

- Keep event representation in `core-input`.
- Keep Windows capture/injection details in `platform-windows`.
- Keep queueing, coalescing, and ownership in daemon state.
- Add UI controls through shared feature/config APIs; do not make tray-only settings.
- Extend `scripts/dev/edge-handoff-trace.ps1` and `scripts/dev/input-trace-matrix.ps1` to produce release evidence.

Done when:

- Users can switch across edges repeatedly without stuck input ownership.
- Escape/release reliably returns control locally.
- Mixed DPI and multi-display setups have documented and validated behavior.
- The tray can tell the user when capture mode is hook, polling fallback, unsupported, or disabled.
- Latency budgets are measured and included in the v5 readiness packet.

### V5-5: Clipboard And File Workflow Parity

Why:

Mouse Without Borders users expect clipboard text and file transfer to feel natural. Boundless has strong clipboard/file primitives, but v5 must productize them.

Deliverables:

- Clipboard text sharing with user-visible enable/disable and status.
- Clipboard bitmap image sharing with size limits, validation errors, and status messages.
- File copy workflow via clipboard where Windows exposes copied file paths.
- Explicit send-file action in tray and CLI.
- Drag/drop file transfer classification with either a supported workflow or an explicit v5 deferral.
- Transfer progress, completion, cancellation, retry, and failure state.
- Receive-folder settings in tray, including organize-by-peer and explicit per-peer auto-accept opt-in.
- Default-deny receive policy unless the user accepts a transfer or enables auto-accept for a specific trusted peer.
- Safe receive directory enforcement, safe filename, path traversal, duplicate name, partial file, temp file cleanup, size, and hash mismatch handling.
- Policy decision on size limits:
  - match Mouse Without Borders' 100 MB file limit for default compatibility, or
  - exceed it with a larger configurable limit and a conservative default.
- Local transfer history for recent clipboard/file events.

Implementation notes:

- Keep transfer validation in `core-transfer` and daemon state.
- Extend protocol/backpressure only through `docs/architecture/network-v1.md`.
- Treat clipboard-file copy as a product workflow distinct from arbitrary background file sync.
- Do not auto-open received files.
- Do not silently accept files from untrusted peers.
- Do not silently accept files from trusted peers unless the user has opted that peer into auto-accept and the transfer passes path, size, hash, and temp-file validation.

Done when:

- A user can copy a file on one paired machine and paste or receive it on another through a documented workflow.
- Tray shows transfer progress and failure reasons.
- Oversized, unsafe, interrupted, duplicate, and hash-mismatch cases have tests.
- Validation covers clipboard text, clipboard image, small file, large-limit rejection, interrupted transfer, reconnect recovery, and receive-folder configuration.

### V5-6: Pairing, Trust Rotation, And Network Safety

Why:

Boundless should exceed shared-key pairing by making trust explicit, recoverable, and diagnosable.

Deliverables:

- Guided tray pairing for discovered and manual peers.
- Pairing challenge confirmation with replay protection, invalid-attempt lockout, and clear retry behavior.
- Trust rotation or "new key" equivalent that revokes existing connections safely.
- Peer remove/reimport behavior that resets reconnect generations and stale trust state.
- Validate remote machine IP / reverse DNS option where useful, with warnings when DNS is unreliable and tests proving the warning/enforcement behavior.
- Same-subnet-only option enforced for outbound connection attempts and inbound trust acceptance.
- Manual endpoint override and discovery fallback.
- Firewall and port diagnostics with actionable messages.
- Protocol compatibility and upgrade mismatch warnings.
- Local control-plane boundary contract covering current-user named-pipe ACLs, service-to-user privilege separation, localhost TCP fallback warnings, and tests proving unauthorized local users cannot drive privileged daemon actions.

Implementation notes:

- Keep trust material in `core-security`.
- Keep nearby pairing protocol in daemon/app-service layers.
- Keep network endpoint selection in daemon network runtime.
- Avoid relying on display names as stable identity.

Done when:

- A non-technical user can pair two machines from tray without copying files.
- A power user can recover using CLI when discovery fails.
- Re-keying/revocation removes stale trust and forces explicit re-pairing.
- Diagnostics distinguish discovery failure, TCP reachability, TLS trust, protocol mismatch, and authorization rejection.

### V5-7: Complete Tray Settings Surface

Why:

Mouse Without Borders exposes its settings in a GUI. Boundless cannot exceed it if key controls remain hidden in CLI/config.

Deliverables:

- Tray settings for all v5 product controls:
  - share input,
  - share clipboard,
  - transfer file,
  - easy mouse,
  - wrap mouse,
  - block corners,
  - relative movement,
  - hide cursor at edge,
  - draw cursor marker,
  - same subnet only,
  - validate remote IP,
  - anti-idle/block screen saver behavior,
  - hotkeys,
  - receive folder,
  - service mode status/actions.
- Status and notification settings for clipboard, file, reconnect, and network messages.
- Dashboard health summary that cannot show green while daemon status is degraded.
- Settings reset and safe reset entry points.

Implementation notes:

- Add missing config fields first, then IPC DTOs, then adapter methods, then tray controls.
- Use shared query/command models in `app-services`.
- Keep controls compact and operational; avoid making a marketing page.
- Every setting must show unsupported/unavailable state when the runtime cannot enforce it.

Done when:

- A user can configure all v5 parity settings without editing config files.
- CLI and tray report the same effective values.
- Unsupported settings are disabled with specific reasons.
- Setting changes persist and survive daemon/tray restart.

### V5-8: Reliability, Auto-Recovery, And Observability

Why:

Exceeding Mouse Without Borders means making failure modes easier to understand and recover from.

Deliverables:

- Local bounded event store for transport, input, clipboard, file transfer, service, and pairing events.
- Reconnect reason classification.
- Tray-visible health state per peer: connected, reconnecting, disconnected, trust error, protocol mismatch, firewall suspect, service issue.
- CLI diagnostics dump with redacted config, peer state, recent events, latency summaries, and installer/service state.
- Auto-reconnect backoff with manual refresh/reconnect.
- Dead daemon, stale named pipe, duplicate daemon, and stale service detection.
- Support bundle export with a redaction manifest that excludes secrets and sensitive metadata by default.
- Explicit opt-in full diagnostic mode for support cases where the user intentionally includes sensitive local metadata.

Implementation notes:

- Reuse existing transport event patterns; avoid unbounded logs.
- Keep event detail structured enough for tests and support tools.
- Add redaction tests for diagnostics output. Default redaction must cover trust material, pairing artifacts, peer IDs, machine IDs, fingerprints, IPs/endpoints, local paths, request IDs, lockout IPs, and any file-transfer paths unless the user opts into full diagnostics.

Done when:

- Most user-visible failures produce a specific next action.
- Diagnostics can identify the failing layer without a developer reading raw logs.
- Support bundle generation is safe to attach to issues by default.

### V5-9: Installer, Upgrade, And Release Hardening

Why:

The release artifact is the product. Boundless must install, upgrade, start, reset, and uninstall cleanly.

Deliverables:

- MSI support for per-user tray mode and optional service mode.
- Version propagation checks across workspace manifests, package manifest, MSI asset names, release-please config, and changelog.
- Upgrade tests from the last supported v4 build to v5.
- Upgrade-while-running behavior for tray, daemon, and service.
- Uninstall cleanup for shortcuts, service, stale pipes, install root, and uninstall registry entries.
- Signing policy finalized for stable v5.
- Release notes that separate implemented, validated, preview, and deferred features.

Implementation notes:

- Keep installer validation in `scripts/dev/installer-smoke.ps1` and release scripts.
- Add service validation only after service mode exists.
- Do not silently make signing a hard gate until release policy variables and secrets are documented.

Done when:

- A clean Windows machine can install v5 and reach a healthy tray/daemon state.
- Upgrade from v4 to v5 preserves or safely migrates config.
- Uninstall leaves no running Boundless process or registered service.
- Release CI produces signed or explicitly unsigned artifacts according to policy.

### V5-10: Validation Harness And Readiness Packet

Why:

V5 should not rely on broad smoke tests for confidence, but release readiness needs repeatable proof across product paths.

Deliverables:

- `scripts/dev/release-readiness.ps1` that orchestrates targeted checks and writes a summary artifact.
- Unit gates:
  - `cargo fmt --all -- --check`,
  - `cargo clippy --workspace --all-targets -- -D warnings`,
  - `cargo test --workspace`.
- Runtime gates:
  - two-node smoke,
  - three-node smoke,
  - four-node topology smoke or equivalent deterministic validation,
  - edge handoff trace matrix,
  - focused daemon/core clipboard tests plus runtime clipboard/file smoke,
  - pairing recovery matrix,
  - installer smoke,
  - service smoke when implemented.
- Readiness packet under `artifacts/release-readiness/` with exact command output, skipped checks, environment details, and risk classification.
- CI split:
  - PR CI remains unit-focused,
  - scheduled/manual Windows workflows run extended runtime checks,
  - release workflow blocks on installer and v5 readiness gates.

Implementation notes:

- Keep runtime checks targeted and diagnosable.
- Never hide skipped checks; every skip must include reason and impact.
- Avoid requiring four physical machines for every developer run; use deterministic local multi-daemon harnesses where possible and reserve physical validation for release candidates.

Done when:

- A clean checkout can produce a v5 readiness packet.
- Release managers can see exactly which checks passed, failed, or were skipped.
- The packet maps back to the parity matrix and workstream done criteria.

### V5-11: Documentation, Support, And Migration

Why:

A release-grade alternative needs clear onboarding, troubleshooting, and honest scope boundaries.

Deliverables:

- README rewritten around user outcomes, not only developer commands.
- First-run guide for two-machine pairing.
- Advanced guide for four-machine layouts.
- Service mode guide with permissions, limitations, and recovery.
- Clipboard/file transfer guide.
- Troubleshooting guide for discovery, firewall, stale daemon, named pipe, service, input capture, clipboard, and file transfer.
- Migration guide from v4 to v5 and from Mouse Without Borders to Boundless.
- Security/trust model document for local pairing, TLS trust, service mode, local IPC, diagnostics, and residual risks.

Implementation notes:

- Keep docs factual. Do not claim lock-screen, elevated-app, or hard isolation behavior until validated.
- Include exact commands for CLI recovery paths.
- Keep screenshots optional until UI stabilizes.

Done when:

- A new user can install, pair, arrange, use, troubleshoot, and uninstall Boundless from docs alone.
- Support issues can ask for a readiness/support bundle rather than raw log spelunking.
- Docs distinguish release-grade, preview, unsupported, and out-of-scope behavior.

## Suggested Sequencing

1. Parity contract and product readiness matrix.
2. Four-machine topology and layout UX.
3. Windows service mode for elevated and lock-screen scenarios.
4. Input handoff excellence.
5. Clipboard and file workflow parity.
6. Pairing, trust rotation, and network safety.
7. Complete tray settings surface.
8. Reliability, auto-recovery, and observability.
9. Installer, upgrade, and release hardening.
10. Validation harness and readiness packet.
11. Documentation, support, and migration.

This order lands the parity contract and topology first, then proves the service boundary before broadening input, clipboard/file, tray settings, reliability, and release hardening.

## V5 Non-Goals

- Remote desktop or screen streaming.
- Cloud pairing or relay service.
- Multi-user remote administration.
- Arbitrary background file sync.
- Secret or credential sharing between machines.
- Clipboard history beyond recent local transfer/status events.
- Linux/macOS parity.
- Mobile support.
- Kernel drivers unless service-mode validation proves user-mode APIs cannot satisfy the v5 promise.
- Hard security guarantees against local administrator compromise.
- Silent autonomous trust decisions.

## Readiness Bar

V5 is ready only when:

- the parity matrix has no unclassified required rows,
- two-, three-, and four-machine topology behavior is validated,
- tray-first pairing, layout, settings, reconnect, and diagnostics are complete,
- keyboard/mouse handoff is reliable across mixed DPI, multi-display, wrap, no-wrap, corner-blocking, and release/unlock paths,
- clipboard text, clipboard image, and file workflows are user-ready and validated,
- service mode is implemented and validated, with lock-screen behavior either validated or explicitly documented as blocked by OS constraints,
- install, upgrade, startup, reset, and uninstall pass on Windows,
- diagnostics identify common failure modes without raw log analysis,
- release documentation is honest about unsupported and preview behavior,
- and the v5 readiness packet contains exact validation output for every release-blocking gate.

The v5 release packet must include:

- commit SHA and version,
- Windows build and runner details,
- validation command list and outputs,
- parity matrix snapshot,
- known limitations,
- skipped checks with impact,
- installer artifact names and signing status,
- upgrade source version,
- and release manager sign-off.
