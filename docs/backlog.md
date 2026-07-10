# Boundless Backlog

Prioritized, implementation-ready story briefs for the path to a real release. Each brief is written to be handed to a coding agent as a standalone task: context and evidence first, then scope, likely files, acceptance criteria, and validation. Keep IDs stable; mark stories done here and in [release/launch-ledger.md](release/launch-ledger.md) when they land.

Ordering rationale: P0 stories are things a launch user would hit in their first hour. P1 stories are the reliability/trust surface that made dogfood expensive. P2 stories are hardening and paper cuts that compound.

Evidence base: the 2026-07-07 and 2026-07-10 two-PC dogfood sessions recorded in [release/launch-ledger.md](release/launch-ledger.md). Transport, pairing, layout propagation, input handoff, and service-mode text clipboard are proven on real asymmetric-reachability hardware. The same sessions exposed first-hour recovery, image-clipboard, discovery, and diagnostics failures that remain ahead of broader feature work.

---

## Landed in v5.0.11 (2026-07-08) — code complete, evidence status below

Full implementation briefs live in git history (`git show ac4d4d0:docs/backlog.md`).

| story | commit | remaining evidence before marking proven |
| --- | --- | --- |
| BND-NEXT-23 (P0) — service honors SCM stop | 0828513 | The 5.0.11 install over a running 5.0.10 service is the first real-binary upgrade this fix will face. Capture a verbose msiexec log showing no multi-minute `ServiceControl` stall, and `Stop-Service` completing in seconds with a broker attached and a peer connected. |
| BND-NEXT-24 (P0) — broker-routed clipboard | 9bd45dd | Reopened after live partial pass: text passed both directions in 5.0.12 service mode, but a policy-valid 6.29 MB bitmap exceeded the broker RPC limit, detached the shared input/clipboard broker, and disabled input. See the active brief below. |
| BND-NEXT-25 (P1) — transport events readable | a96613b | Reopened by 5.0.12 BND-NEXT-34 evidence. The v5.0.13 candidate now bounds repeated activity/failures and prioritizes causal records; full per-stage counters and the installed sustained-input trace remain pending under BND-NEXT-34. |
| BND-NEXT-26 (P1) — packaging-script CI | 0305022 | Done — self-tests + CLI daemon-status output contract wired into `ci.yml` and release validation, green on main. Optional: demonstrate a deliberate `^machine_id=` regression fails CI locally. |

---

## v5.0.13 fix train (2026-07-10) — engineering status, not dogfood proof

These commits are integrated on the candidate branch. `Code complete` means the scoped implementation and automated coverage exist; `needs installed evidence` means the behavior has not yet passed the two-PC, UAC, or physical-device acceptance checks below.

| story | candidate status | primary commits | remaining gap |
| --- | --- | --- | --- |
| BND-NEXT-24 | in progress | 0235d25, 106d289, 4593fea, a6d76f4 | Broker fault isolation, the symmetric 9 MiB control-plane limit, transient retry, failed-sequence suppression, and sequence-aware newest-payload preservation are implemented. Explicit tray/CLI `clipboard-degraded` versus `input-degraded` UX is still missing, and the installed Paint size/fault matrix has not run. |
| BND-NEXT-29 items 1, 2, and 5 | code complete; needs installed evidence | 5dc1521, 7a39d66 | The helper verifies package/runtime postconditions and packaging removes stale WiX output. A real helper-driven 5.0.12→5.0.13 upgrade remains pending; item 3 is open and item 4 still needs installed version verification. |
| BND-NEXT-31 | code complete; needs installed evidence | dacfad8, 6f1bd28, 68f42f1 | Bounded shutdown, fail-open cleanup, held-input release, replacement, and stale-owner ordering have automated coverage. Quit/relaunch during active capture, first post-relaunch handoff/escape, and upgrade-while-running still need installed proof. |
| BND-NEXT-34 | in progress | e3f1752, 77ec135, b3c64d9, b1f38a1 | Priority retention and bounded activity/failure aggregation are implemented. The complete per-stage counter vocabulary and a 60-second installed two-PC trace remain acceptance gaps. |
| BND-NEXT-35 | code complete; needs installed evidence | d516728 | Native SCM recovery, the explicit action, bounded polling, and UAC fallback have automated coverage; stopped-to-running recovery still needs standard-user validation on both PCs. |
| BND-NEXT-37 | code complete; needs installed evidence | 42a31ad, 6115c6b, b1f38a1 | Event creation and raw-output sanitization retain metadata only in automated tests. An installed unique-sentinel sweep across API, CLI, logs, and diagnostic bundles remains pending. |
| BND-NEXT-38 | code complete; needs installed evidence | be32484, 6cdb326, 10ef0ed | Local fail-open escape, lease expiry, reconciliation, and direct-capture unlock have deterministic coverage. Installed stalled-IPC/daemon-loss/UI-hang fault smoke and the next-handoff check remain pending. |
| BND-NEXT-39 | implementation candidate; needs physical evidence | 06facb7, dfeba9c | Raw Input vertical/horizontal high-resolution wheel capture and cross-source deduplication are implemented. The EliteBook source path is still unknown; pointer-input fallback is evidence-gated, and trackpad plus conventional-wheel proof has not run. |

---

## BND-NEXT-24 (P0, in progress): Isolate service-mode clipboard failures and carry policy-valid images

Status: the broker-isolation and policy-valid IPC path are implemented on the v5.0.13 candidate, but the explicit degraded-state tray/CLI UX and installed service-mode Paint matrix remain open.

### Context and evidence

Installed 5.0.12 proved service-mode text clipboard in both directions for the first time. The image path then failed below the advertised policy limit: Paint placed a 1254×1254 32-bit bitmap on the clipboard (about 6.29 MB), while Boundless permits images up to 8 MB. The tray-to-service unary RPC retained its lower default message limit. That error won a shared `select!`, cancelled input exchange, detached the user-session broker, and retried the same clipboard sequence every three seconds. Peer health still said connected/trusted while mouse and keyboard input fell back to unsupported Session 0 injection.

A follow-up copy of a 400×400 Paint bitmap was immediately followed by an unavailable edge handoff. The 01:25 capture showed the service, peer, and user-session broker healthy again with no target or lock, but all 50 retained events were broker/anti-idle wake noise from a 3.8-second window. That confirms a second user-visible image-copy/input interaction and a BND-NEXT-34 evidence failure; it does not prove that the small image hit the same RPC-size fault.

### What to build

- Make input and clipboard broker supervision independent: clipboard read, validation, IPC, or apply failure must never cancel input capture/injection or detach the input broker.
- Carry every policy-valid clipboard image through the tray/service boundary in both directions. Use a symmetric message contract with protobuf overhead accounted for, or chunk the local IPC payload; do not silently lower the 8 MB product policy.
- Classify a non-retryable clipboard sequence once, retain a bounded failure summary, and wait for clipboard contents to change instead of replaying an attach/detach storm.
- Surface exact clipboard degradation in tray/CLI while peer and input health remain truthful.

### Acceptance criteria

- The observed 6.29 MB bitmap and boundary payloads up to the configured 8 MB policy transfer through Paint in both directions.
- Payloads above policy are rejected once with an actionable size error; they do not detach input, loop every three seconds, or retry until the clipboard changes.
- Mouse and keyboard handoff remain usable throughout local read errors, request-side oversize, response-side oversize, remote apply failure, and clipboard unavailability.
- Normal-size image reads and encoding may not monopolize the input exchange loop; edge detection and emergency unlock remain responsive while clipboard work is in progress.
- Input/clipboard broker state and peer health distinguish attached, clipboard-degraded, input-degraded, and fully healthy states.
- Deterministic tests cover both IPC directions, message-size overhead, independent task failure, unchanged failed-sequence suppression, and recovery after a short text copy.

### Out of scope

- Clipboard image streaming/spooling across the peer transport; this failure occurred before peer transport and below the current policy limit.
- Explorer copied-file clipboard semantics, tracked separately in BND-NEXT-36.

### Validation

Focused broker/IPC fault tests; installed service-mode Paint copy/paste at small, observed, boundary, and over-limit sizes; continuous input handoff during each clipboard failure case.

---

## BND-NEXT-35 (P0, needs installed evidence): Start a stopped installed service from the tray without console flashes

Status: code complete in d516728; automated state/offer tests pass, but standard-user stopped-to-running recovery has not run on either dogfood PC.

### Context and evidence

After reset on both 5.0.12 PCs, launching Boundless from Start opened the tray but left the installed automatic `BoundlessService` stopped. The tray repeatedly reported backend failure and flashed a console window until the user opened elevated PowerShell and ran `Start-Service BoundlessService`. The current tray correctly refuses to launch a competing per-user daemon when the service exists, but it only queries service state and bails; its retry loop repeatedly launches visible `sc.exe query` processes.

### What to build

- Preserve fail-closed daemon ownership while adding a bounded, quiet Windows service recovery path for installed/stopped and start-pending states.
- Present one explicit “Start Boundless service” action. Start without elevation when service ACLs permit; otherwise request UAC once and report cancellation/access denied without retrying or flashing terminals.
- Wait for both SCM Running and the named-pipe API before declaring recovery, then reconnect the existing tray/brokers without another launch.
- Keep missing-service development fallback and running-but-unreachable repair guidance distinct.

### Acceptance criteria

- With the installed service explicitly stopped, Start Menu launch reaches a healthy service/API through one user-visible recovery action and no manual shell.
- No console, PowerShell, or `sc.exe` window flashes during state queries, start, polling, failure, or retry.
- Access denied, UAC cancellation, start timeout, running/unreachable, start-pending, and missing-service cases remain stable and actionable; no exponential retry storm occurs.
- Recovery never starts `boundlessd.exe`, never creates a second service or tray, and leaves service ownership LocalSystem with the configured allowed-user SID.
- Focused state-machine tests plus an installed standard-user Windows smoke prove stopped-to-running recovery on both dogfood PCs.

### Blocked by

None. Preserve BND-NEXT-11’s service-ownership rule rather than reverting to a per-user daemon fallback.

---

## BND-NEXT-38 (P0, needs installed evidence): Make emergency input unlock local and IPC-independent

Status: code complete in be32484, 6cdb326, and 10ef0ed; deterministic fault-path coverage exists, but installed fault injection and post-recovery handoff remain unproven.

### Context and evidence

The 5.0.12 tray lifecycle smoke passed single-instance launch and visually recovered connection, trust, layout, input, and clipboard after Quit/relaunch. The first real left-edge handoff then trapped local input. CODY-PC recorded `input_handoff` at 01:29:16.957, `input_lock_engaged requested=true applied=true` at 01:29:17.002, and eight outgoing frames through 01:29:36.251. Double-Control did not return control and no escape event was recorded. Force-ending tray PID 58820 restored local input; Windows recorded Application Hang 1002 (`Top level window is idle`), and the daemon fell back to `service_session_unsupported` at 01:29:43 while retaining the EliteBook as the configured target.

The safety boundary is architecturally confirmed even though the destination injection failure is not: the hook currently queues `EscapeUnlock` but keeps swallowing input until the tray completes another broker RPC and the daemon replies with lock disabled. A stalled exchange, clipboard failure, tray hang, daemon loss, or full event queue can therefore defeat the emergency escape.

### What to build

- Make the escape gesture atomically release the local hook lock before any channel send, IPC, daemon, network, or UI work; reconcile capture state asynchronously afterward.
- Give the local lock a short broker-exchange lease or watchdog owned outside the dashboard/UI loop. If successful exchanges stop, fail open to local control within a documented bound no longer than the daemon’s broker-stale window.
- Provide one reconciliation operation for local escape/lease expiry that asynchronously clears capture, releases remote held keys/buttons, and suppresses immediate edge recapture. BND-NEXT-31 invokes the same primitive during graceful tray shutdown.
- Record one bounded safety transition with cause (`escape` or `lease_expired`) without logging keys or per-frame activity.

### Acceptance criteria

- Double-Control restores local mouse and keyboard within 100 ms while the broker RPC is stalled, daemon is unavailable, clipboard exchange has failed, event queues are full, or the dashboard is hung.
- If no successful broker exchange occurs, the local hook lock releases automatically within three seconds without requiring Task Manager or process termination.
- Normal escape and lease timeout clear the configured/active capture target, release remote held keys/buttons, and prevent immediate edge recapture.
- The escape gesture remains reliable under realistic human tap timing while avoiding accidental single-Control unlocks; exact timing is covered by deterministic tests.
- Installed Windows fault smoke proves local recovery during handoff with a deliberately stalled exchange and confirms the next handoff works without restart.

### Dependencies and scope boundary

Coordinate with BND-NEXT-24’s independent broker supervision. This story owns the fail-open hook primitive, watchdog, and reconciliation operation; BND-NEXT-31 owns invoking them during graceful Quit and broker lifecycle. Do not wait for either story to make the local emergency path fail-safe.

---

## BND-NEXT-39 (P0, implementation candidate needs evidence): Carry laptop two-finger scrolling through remote input

Status: 06facb7 and dfeba9c implement and deduplicate Raw Input wheel data. The implementation remains evidence-gated until the EliteBook identifies that as its source path; add pointer input only if the physical trace requires it.

### Context and evidence

EliteBook-to-CODY control passed movement, buttons, typing, handoff, return, and initial Double-Control escape, but two-finger vertical scrolling on the EliteBook trackpad produced no movement in remote browser/chat surfaces. The device/driver path was not captured, so do not yet classify it as a Windows Precision Touchpad. Boundless has end-to-end generic `MouseWheel` serialization and `SendInput` injection, but capture only accepts wheel messages surfaced through the low-level mouse hook. Its Raw Input path registers a generic mouse and reads only relative X/Y; it ignores raw wheel flags, and there is no touchpad/pointer capture path. Current diagnostics cannot distinguish “gesture never reached capture” from filtering or small-delta loss, so the exact capture mechanism remains an evidence-gated implementation choice.

### What to build

- Use a targeted development trace to identify the EliteBook device/driver and prove where its gesture disappears without retaining individual input content. Durable production counters belong to BND-NEXT-34.
- Capture vertical and horizontal two-finger scrolling through the appropriate supported Windows path. Prefer existing Raw Input wheel data when the device emits it; use touchpad-capable pointer input only when required by the observed device path.
- Deduplicate legacy hook, Raw Input, and pointer representations so one physical gesture never scrolls twice.
- Preserve signed high-resolution deltas and configured Windows scroll direction through broker IPC, peer transport, and injection.

### Acceptance criteria

- While the EliteBook owns remote capture, two-finger vertical and horizontal pans scroll the remote application without scrolling the local surface, and diagnostics identify the actual source/device path used.
- Deltas `±1`, `±40`, and `±120` survive capture, broker IPC, wire encoding/decoding, and `SendInput`; direction, gesture completion, and inertia do not reverse, disappear, or continue indefinitely.
- A conventional physical wheel/hwheel still works, and duplicate event sources do not double-scroll.
- Input disable, emergency escape, broker detach, and tray restart terminate active scrolling safely.
- Tests cover source classification, legitimate touchpad input versus injected-loop suppression, both axes and high-resolution deltas, IPC/wire round trips, and injection records.
- Installed two-PC validation uses the EliteBook trackpad and, when available, a conventional mouse wheel.

### Dependencies and scope boundary

BND-NEXT-34 owns durable bounded stage counters. This story may use temporary targeted tracing or consume those counters, but must not add a second production telemetry system. Do not broaden into arbitrary three-/four-finger gesture sharing.

---

## BND-NEXT-27 (P1, top active priority): Trust rotation and reset as a first-class product flow

### Context and evidence

Stale LocalSystem service trust caused the entire historic two-PC connect blocker (weeks of dogfood failure), and the recovery path was a PowerShell script whose failure mode was a warning line plus silently cleaning the wrong (user) profile. The 2026-07-07 rotation that finally fixed the blocker was only observable through script stdout. Trust lifecycle is too load-bearing to live outside the product.

### Scope

- Tray dashboard: show local identity/trust age and a "Reset trust and pairing" action with an explicit confirmation ceremony (reuse the daemon's `rotate-trust:<machine_id>` confirmation token model), a clear "restart required" completion state, and a loud, unmissable failure state.
- CLI: `boundlessctl pair rotate-trust` already exists — add a human-friendly `boundlessctl trust status` (identity created-at, trusted peer count, per-peer trusted-since/fingerprint; `peer list` already shows some of this).
- `Boundless-Reset.ps1` becomes a thin wrapper over the same daemon API with unchanged flags; local file cleanup remains only behind `-ForceLocalCleanup` and must exit nonzero when the daemon API path was requested but failed (no silent fallback).
- Update `packaging/windows/README.txt` and the smoke runbook template to the product flow.

### Likely files

- `crates/tray/src/dashboard/` (workflow/model), `crates/app-services/src/commands.rs`, `crates/cli/src/commands.rs`, `packaging/windows/Boundless-Reset.ps1`

### Acceptance criteria

- A user can rotate trust from the tray without any shell, sees restart-required, and sees failure loudly if the daemon API is unreachable.
- `Boundless-Reset.ps1 -All` without `-ForceLocalCleanup` exits nonzero if rotate-trust did not happen (today it warns and continues into user-profile cleanup).
- Tray/CLI tests cover confirm-token, failure, and restart-required states.

### Validation

`scripts/dev/check.ps1 -Area workspace`; manual tray rotation on an installed build followed by re-pairing.

---

## BND-NEXT-21 (P1): Installer-owned Private/local-subnet firewall policy (implementation)

The design and fail-closed requirements are fully specified in [architecture/one-sided-reachability.md](architecture/one-sided-reachability.md) (BND-NEXT-21 sections) — read those before starting; they are the contract. This entry adds the dogfood evidence and scopes the implementation slice.

### Context and evidence

2026-07-07 diagnostics: one PC had zero inbound Boundless rules, making reachability one-sided. The smoke still passed because reverse initiation covered it — but only because *one* direction was open. MWB's out-of-box reliability comes from its installer-owned `localSubnet` firewall exception (verified in PowerToys source: `installer/PowerToysSetupVNext/MouseWithoutBorders.wxs`). Manual `New-NetFirewallRule` runbook steps are not a launch answer.

### Scope

Exactly the "Recommended Shape If Later Approved" section of the design doc: explicit opt-in MSI/helper-owned rule for `%ProgramFiles%\Boundless\boundless-service.exe`, Private profile + LocalSubnet scope, TCP 15100 and 15200 only (never 15101), repair/upgrade/uninstall ownership, fail-closed on every unverifiable prerequisite, diagnostics reporting of the expected rule shape. This remains human-gated: land it behind an explicit installer option that defaults per product decision, and do not merge without that decision recorded.

### Acceptance criteria

The "Evidence Before Connectivity Claims" and "Fail-Closed Requirements" lists in the design doc, verbatim, plus: `Boundless-ConnectivityDiagnostics.ps1` `firewall_hint` recognizes the MSI-owned rule as the expected shape.

### Validation

`scripts/dev/installer-smoke.ps1` extended for rule create/repair/uninstall evidence; negative evidence that no Public/`remoteip=any` rule is ever created; real two-PC install with both directions reachable and no manual firewall steps.

---

## BND-NEXT-22 (P2): MWB side-by-side and port-collision productization

Scoped in [architecture/one-sided-reachability.md](architecture/one-sided-reachability.md) §BND-NEXT-22. Dogfood reality (2026-07-07): MWB runs on both PCs between Boundless test windows as the user's working fallback, and owns TCP 15100 (IPv6) while Boundless holds IPv4 — a silent split. Diagnostics already detect and classify this; the product does not.

Scope: surface the collision in the tray (peer health or a dedicated warning), and provide a guided alternate-`network_port` flow that applies the same port on all trusted peers (pairing port = network_port + 100). Acceptance: with MWB listening on 15100, the tray shows the collision and the guided flow moves both machines to a working alternate port without breaking trust. Files: diagnostics surface already exists in `crates/app-services/src/diagnostics.rs`; add tray workflow + config propagation.

Add the 2026-07-10 start-order evidence to validation: MWB was still running when Boundless first installed/launched, and input recovered only after MWB was fully removed plus broader Boundless recovery. The exact hook-collision cause was not isolated, so do not claim it as confirmed. Validate start-order permutations and require a narrow recovery—stop MWB and restart only the tray broker—without trust/network reset. Surface active MWB hook/port ownership separately from generic backend failure.

---

## BND-NEXT-20E (P1, ready for agent): Self-heal and explain one-way discovery

### Context and evidence

On the routed dogfood topology, CODY-ELITEBOOK (`10.10.0.187`) discovered CODY-PC (`192.168.1.102`) while CODY-PC showed no discovered peers. Direct TCP from CODY-PC to the EliteBook succeeded on 15100 and 15200, manual-host pairing succeeded, and discovery returned after reset/restart. The blocked-side error reported `role_reversal_attempted=false`. The symptom is confirmed; whether the cause was a stopped browse, an unusable resolution, interface churn, or routed multicast behavior is not.

### What to build

- Supervise discovery lifecycle with bounded restart/backoff when browse stops, its receiver closes, or interface state changes.
- Record why a resolved service is discarded and expose active, healthy, last-success, and last-error discovery state instead of one `mdns_active` boolean.
- When automatic reverse signaling is impossible, immediately explain the exact other-machine manual-host action. Persist a successfully paired/reachable candidate so reconnect does not depend on fresh mDNS.
- Keep cross-subnet multicast support decision-gated: do not promise that every routed network reflects mDNS.

### Acceptance criteria

- Injected browse stop, receiver close, and interface-change cases recover without daemon restart and without duplicate registrations.
- Missing TXT properties, empty addresses, scope problems, and filtered self-records produce bounded diagnostic reasons instead of silent disappearance.
- The exact two-subnet installed topology has a recorded restart/reinstall matrix; unsupported routed multicast produces immediate manual-host guidance rather than indefinite empty discovery.
- After successful manual pairing, service/tray restart reconnects by persisted trusted candidate even when mDNS remains one-way.
- Role-reversal diagnostics explain why reversal was or was not attempted and never imply success when no signaling path exists.

### Dependencies and scope boundary

Build on the existing BND-NEXT-20 candidate and role-reversal model; no relay/cloud or broad discovery redesign.

---

## BND-NEXT-28 (P2): `boundlessctl --json`

Machine-readable output (`--json`) for `daemon status`, `peer list`, `transport events`, `feature list`. Motivation: the c2e1509 bug class — scripts regex-scraping single-line prose — plus the CI contract tests (BND-NEXT-26, landed with a text fixture that JSON would make sturdier) and support tooling. Keep the human format the default and unchanged. Acceptance: the four commands emit stable JSON with a `schema_version` field; `Boundless-Reset.ps1` machine-id lookup prefers JSON with regex fallback; CLI tests snapshot the JSON shape. Files: `crates/cli/src/commands.rs`, `crates/cli/src/console.rs`, `packaging/windows/Boundless-Reset.ps1`.

## BND-NEXT-29 (P2): Install/packaging paper cuts (bundle)

Independent small items, one PR each or one sweep; all observed 2026-07-07:

1. **Code complete; installed validation pending (5dc1521, 7a39d66).** `Boundless-Install.ps1` printed `boundless_install_exit_code=0` on a run where nothing was installed (wedged-mutex session). The helper now verifies Windows Installer registration, manifest/runtime versions, service identity/state, API health, and tray postconditions, and rejects stale evidence; prove those checks during the real 5.0.12→5.0.13 helper-driven upgrade.
2. **Code complete (5dc1521).** `scripts/release/package-windows.ps1` removes `packaging/windows/installer/obj` and `bin` before `dotnet build`, preventing the stale-output MSB3030 failure. Keep the packaging-script smoke in the release gate.
3. **Open.** Same-version dogfood upgrades silently no-op (`MajorUpgrade` without `AllowSameVersionUpgrades`). Either add `AllowSameVersionUpgrades="yes"` or make the packaging script refuse to build an MSI whose version equals an already-published dogfood artifact. Decide and document in `packaging/windows/README.txt`.
4. **Needs installed verification.** `boundlessctl daemon status` reported `daemon_version=5.0.0` on 5.0.10 installs. The workspace version bump in v5.0.11 (55c5f6b) likely resolves this — verify against the installed v5.0.13 package, and if the value still lags, wire the real package version through.
5. **Code complete; installed validation pending (5dc1521, 7a39d66).** Double-clicking the released 5.0.12 MSI failed with Windows Installer error 1603 because `BOUNDLESS_ALLOWED_USER_SID` was absent; a later helper/property-driven install succeeded. The helper is now the documented primary entry point and verifies product/version registration, the service command’s allowed-user SID, service Running, daemon API health, and one responsive tray. Prove the UAC/helper path on both PCs; direct raw-MSI launch is not the supported entry point.

Tray single-instance ownership moved to standalone story BND-NEXT-31 because duplicate trays interfere with the backend connection and need their own runtime acceptance criteria.

Acceptance: each item has a targeted test or self-test where the surface allows; install success is defined by registered version plus healthy service/API/tray postconditions, not process exit alone.

---

## BND-NEXT-31 (P1, needs installed evidence): Enforce one tray instance and a safe broker lifecycle per Windows user session

Status: single-instance ownership passed installed 5.0.12 dogfood. The v5.0.13 candidate implements bounded Quit/relaunch cleanup and broker replacement/stale-owner ordering (dacfad8, 6f1bd28, 68f42f1); installed active-capture lifecycle and upgrade evidence remain pending.

### Context and evidence

Dogfood can launch any number of `boundlesstray.exe` processes in the same desktop session. Duplicate trays each start their own dashboard and broker activity, then compete for or lose access to the same backend service. The resulting connection failures look like daemon, named-pipe, or service instability even though the initiating defect is duplicate UI ownership. Installer smoke detects an unexpected tray count after upgrade, but normal application launch has no single-instance guard.

Repeated Start Menu launch three times left one tray/dashboard, proving the ownership guard on installed 5.0.12. Quit/relaunch then created a new tray and visually restored connection/trust/layout, but its first handoff trapped local input until force termination. That run produced the fourth Windows Application Hang 1002 since July 9 and left no graceful broker-detach event. Treat single-instance ownership as partial success, not story completion.

### Scope

- Acquire a per-user, per-session Windows single-instance primitive before starting the dashboard, input broker, clipboard broker, or backend polling.
- When a second launch is attempted, activate or focus the existing dashboard and exit the new process successfully. If activation is temporarily unavailable, exit with a clear diagnostic instead of starting another broker owner.
- Do not use a machine-global lock that prevents a different interactive Windows session or user from running its own tray.
- Keep lifecycle behavior correct across normal exit, crash recovery, sign-out, MSI upgrade, and tray restart.
- Make Quit signal, cancel, and bounded-join the broker supervisor: unlock locally first, flush held-input releases, detach, clear any capture target, then close the dashboard process.
- Record enough local diagnostics to distinguish "existing tray activated" from daemon or IPC connection failure without exposing sensitive state.

### Likely files

- `crates/tray/src/main.rs`, `crates/tray/src/dashboard.rs`, `crates/platform-windows/src/`, `scripts/dev/installer-smoke.ps1`

### Acceptance criteria

- Repeated Start Menu, shortcut, and direct executable launches leave exactly one tray process in the current user session.
- A second launch focuses or opens the existing dashboard and does not start another input/clipboard broker.
- Different Windows users or interactive sessions do not block one another.
- After clean exit or forced termination, the tray can be launched again without manual cleanup or reboot.
- Quit/relaunch while connected and while actively captured leaves local input unlocked, clears stale capture ownership, and the first post-relaunch handoff plus emergency escape succeeds.
- Upgrade-while-running and installer smoke still finish with exactly one tray process and a healthy daemon API connection.
- Focused tests cover lock acquisition, second-launch behavior, stale-owner recovery, and session scoping; a Windows runtime check proves the process-count behavior.

### Validation

Targeted tray/platform tests; `scripts/dev/installer-smoke.ps1` process-count coverage; manual repeated-launch smoke from shortcut, Start Menu, and executable in one session.

---

## BND-NEXT-32 (P1): Simplify CI/CD for one-user dogfood iteration

### Context and evidence

Boundless currently has one active user and is iterating through private two-PC dogfood rather than supporting a broad public release population. CI and release work have accumulated multiple workflows, release paths, PowerShell harnesses, platform-specific gates, and recovery fixes. Recent releases succeeded, but repeated workflow and installer-validation hiccups made routine iteration expensive and obscured which checks protect a real product risk versus historical process complexity. The desired outcome is not maximum automation; it is a small, legible system that gives fast feedback during development and preserves the few Windows/release proofs that matter.

### Scope

This is an analysis and migration-design story before workflow changes:

1. Inventory `.github/workflows/`, reusable actions, `scripts/dev/`, and `scripts/release/`: triggers, dependencies, duplicated checks, secrets, artifacts, typical duration, recent failure causes, and whether each gate protects PR correctness, dogfood evidence, or public release mechanics.
2. Review the latest meaningful CI and release runs, including failures that required follow-up commits, and separate product failures from flaky, stale-assumption, environment, and orchestration failures.
3. Record the operating model explicitly: one maintainer/user, Windows-first runtime, Linux compile/test coverage, rapid dogfood builds, rare deliberate releases, optional signing, and physical two-PC evidence that cannot honestly be replaced by hosted CI.
4. Propose a target flow with the fewest useful lanes. Evaluate a fast required PR/main lane, manual or scheduled extended Windows validation, an explicit dogfood-package lane, and a deliberate release-promotion lane. Recommend exact triggers and ownership rather than preserving every current workflow by default.
5. Define target feedback times, required versus advisory checks, cancellation/concurrency policy, artifact retention, failure messages, rerun policy, release rollback/recovery, and the minimum branch-protection rules appropriate for a single maintainer.
6. Produce a staged, reversible migration plan that deletes or consolidates redundant paths, preserves evidence contracts, and can be validated before old workflows are removed.

### Likely files and outputs

- Inputs: `.github/workflows/*.yml`, `.github/actions/`, `scripts/dev/`, `scripts/release/`, `docs/release/`, recent GitHub Actions runs and release history.
- Output: a short CI/CD current-state report plus an ADR or implementation plan under `docs/` that includes the proposed workflow diagram, gate matrix, migration slices, and explicit non-goals.

### Acceptance criteria

- Every current workflow and release entry point has an owner, purpose, trigger, cost/duration estimate, evidence output, and keep/change/remove recommendation.
- The proposal is tailored to current one-user dogfooding and identifies what must change if external contributors or users materially increase.
- Required PR feedback is narrow and fast; physical Windows dogfood, installer, and release evidence remain visible but are not misrepresented as routine unit CI.
- The release path has one documented source of truth for versioning, packaging, validation, and publication, with no ambiguous automatic/manual overlap.
- The plan names which current checks become required, advisory, manual, scheduled, consolidated, or deleted and explains the risk tradeoff.
- Implementation is broken into independently reversible PRs with success metrics such as median feedback time, avoidable reruns, and failure-diagnosis time.

### Validation

Review the proposal against at least the last 30 relevant CI/release runs and the v5.0.11 release incident history; dry-run or branch-test each proposed workflow slice before removing its predecessor.

---

## BND-NEXT-33 (P1 epic): Tray and PC-layout experience refresh

### Product intent

Replace the weakest parts of the current tray dashboard—especially PC layout—with an interface that is quick to understand, calm during normal operation, and mostly unnecessary for common one- and two-computer setups. Preserve the settings view as the strongest existing baseline: its information density and grouping are broadly useful, so this epic should refine it only where research finds concrete friction rather than redesigning it for visual consistency alone.

The default experience should infer a sensible shared topology when computers connect. A user should not have to manually place the first peer every time. Manual layout editing remains available for unusual physical arrangements and must produce one canonical layout that converges across connected peers.

### Child stories

#### BND-NEXT-33A: Audit the current tray and generate design directions

- Map the current Status & Pairing, layout, transfer, diagnostics, and settings journeys using screenshots and code-backed state descriptions.
- Identify duplicated information, hidden state, backend-dependent failure states, and actions that are too easy to invoke repeatedly.
- Generate several substantially different UI directions for the dashboard and PC-layout view, including compact normal-operation and degraded/recovery states. Use realistic one-, two-, three-, and four-PC data rather than empty mockups.
- Evaluate the concepts against time-to-understand, common-task click count, layout legibility, accessibility, keyboard use, resize behavior, and honest representation of disconnected or partially synchronized peers.
- Select a direction and record the rationale before production implementation. Settings should remain recognizable unless a proposed change has a specific usability benefit.

Done when a review packet contains the current-state audit, task flows, multiple visual concepts, edge-state variants, evaluation matrix, and one approved implementation direction.

#### BND-NEXT-33B: Define zero-configuration layout policy for one to three PCs

- Define a deterministic cluster-wide concept of the main/anchor PC and stable peer ordering; do not independently center "self" on every machine.
- One PC: self-only layout, with layout editing visually de-emphasized.
- Two PCs: choose a predictable left/right default using the main PC and first peer, while allowing a one-action swap.
- Three PCs: default the main PC to the middle and place the first two peers left/right in stable order.
- Decide how reconnect, replacement, renamed peers, removed trust, and a newly selected main PC affect the inferred layout.
- Never overwrite a layout the user has explicitly edited without a visible confirmation or reset-to-automatic action.

Done when the policy is documented with state-transition examples and deterministic tests for one-, two-, and three-PC clusters, reconnect ordering, and manual overrides.

#### BND-NEXT-33C: Auto-apply and converge the canonical layout

- Represent whether a layout is automatic or manually overridden and identify its authoritative revision/source.
- On pair/connect, derive the default layout once, persist it, and propagate the same canonical matrix to all participating peers.
- Make updates idempotent and resilient to reconnects, duplicate sessions, temporarily offline peers, and competing stale revisions.
- Surface pending, applied, conflict, and failed synchronization states without requiring raw diagnostics.
- Provide "Use automatic layout," "Swap sides," and "Edit layout" paths appropriate to cluster size.

Done when two connected PCs reach the same usable left/right layout without opening the editor, a three-PC cluster consistently anchors the main PC in the middle, and reconnect does not require re-placement or erase manual overrides.

#### BND-NEXT-33D: Implement the selected dashboard and layout editor

- Build the approved visual direction on existing shared app-service/query models; do not move daemon workflow logic into the tray.
- Make normal connected state compact and obvious, with deeper diagnostics and configuration available through progressive disclosure.
- Replace ambiguous free-form placement interactions with clear spatial affordances, snap/adjacency feedback, undo/reset, and a visible distinction between automatic and custom layout.
- Design explicit loading, empty, disconnected, backend-unavailable, partial-sync, and destructive-confirmation states.
- Preserve the settings view's current grouping and information coverage unless the approved design documents a measured improvement.

Done when the implementation matches the approved responsive states, keyboard and pointer flows work, automated state/model tests pass, and rendered Windows screenshots are reviewed at common display scales.

### Epic acceptance criteria

- A first-time one- or two-PC user can pair and begin edge handoff without manually opening the layout editor.
- All connected peers display and enforce the same canonical topology after connect/reconnect.
- The tray prevents duplicate-instance ambiguity through BND-NEXT-31 before the refreshed UX relies on a single local UI owner.
- Backend unavailable, stale layout, offline peer, and synchronization failure states explain the next action without raw log inspection.
- The chosen UI direction is reviewed visually before implementation, and the implemented Windows UI is rendered and inspected rather than accepted from unit tests alone.

### Likely files and validation

- `crates/tray/src/dashboard/`, `crates/tray/src/dashboard.rs`, `crates/app-services/`, `crates/ipc-api/`, `crates/adapter-ipc-grpc/`, daemon layout/state operations, and focused topology/layout tests.
- Validate incrementally with model/workflow tests, deterministic multi-daemon topology tests, rendered Windows screenshots, and real two-PC dogfood. Use three-/four-node smoke only for slices that change multi-peer convergence.

---

## BND-NEXT-34 (P1, in progress): Keep high-rate runtime telemetry from evicting diagnostic events

**Category:** bug

Status: e3f1752, 77ec135, b3c64d9, and b1f38a1 implement priority retention plus bounded activity/failure aggregation. Full per-stage counters and the installed 60-second causal trace are still open.

### Context and evidence

BND-NEXT-25 removed retained `input_runtime_wake` safety-tick noise and added CLI kind filters, but 5.0.12 two-PC dogfood exposed additional high-rate paths. While the user pushed the cursor through a configured edge, `runtime_wake channel=input_capture source=input_broker_exchange` and later per-frame `input_frame`/inject-failure records were retained roughly every 15 milliseconds. The bounded event ring then evicted the useful `input_handoff` transition before a follow-up query could retrieve it. A later capture retained exactly 50 broker/anti-idle wake records spanning only 3.8 seconds and no clipboard, edge, or failure cause. Live status proved that the broker was exchanging, but retained telemetry could not preserve a concise causal path.

### Desired behavior

High-frequency wake/activity signals must remain available as bounded health summaries without displacing state transitions, failures, or causal input-path evidence. Operators should be able to answer, from one bounded query or support bundle, whether an edge handoff was detected and where input stopped: capture, queue, transport, receive, or injection. CLI filtering is useful for presentation, but it is not sufficient if important records have already been evicted from storage.

### Key interfaces

- The bounded runtime/transport event store and its retention policy: distinguish diagnostic state transitions and failures from high-rate activity samples.
- Runtime wake recording: coalesce, rate-limit, aggregate, or count repeated equivalent wake events while preserving useful totals and last-seen timestamps.
- Input handoff telemetry: retain a correlated path from edge detection through capture target change, outgoing frame disposition, remote receive, and injection outcome without logging every mouse frame.
- CLI and diagnostic bundle queries: expose useful filters and summaries over the retained data, with clear event-kind vocabulary and bounded output.

### Acceptance criteria

- During at least 60 seconds of continuous mouse movement and broker exchange, the retained ring still contains the most recent `input_handoff` state transition and any input queue/transport/injection failure.
- Equivalent high-rate wake events are represented by bounded aggregate or sampled records that include count and time range; retained-event growth is not proportional to mouse polling frequency.
- A diagnostic query can distinguish: edge never detected, capture activated, outgoing frames queued, transport delivery failed, remote frames received, injection skipped, and injection failed.
- Bounded per-kind/per-source counters distinguish low-level hook, Raw Input, touchpad/pointer normalization, broker acceptance, network send/receive, and injection without retaining individual key or pointer content.
- `--kind` and `--exclude-kind` continue to filter before `--limit`, and filtering behavior has deterministic tests against both aggregate and high-value event kinds.
- Event retention has explicit priority/budget tests proving low-value activity cannot evict newer state transitions or errors under sustained input, clipboard, anti-idle, and reconnect activity.
- Default diagnostics remain local, bounded, and redacted; raw per-movement logging is not introduced.

### Out of scope

- Fixing the separate 5.0.12 input-delivery failure observed in the same dogfood session.
- Building a general remote telemetry or cloud logging service.
- Retaining every mouse/keyboard event or increasing the ring without a bounded retention policy.

### Validation

Focused event-store/runtime tests with a synthetic high-rate broker stream; CLI filter tests; diagnostics-bundle redaction tests; installed two-PC edge-handoff trace proving useful events remain queryable after sustained movement.

---

## BND-NEXT-37 (P1, needs installed evidence): Never retain clipboard content in runtime events

Status: code complete in 42a31ad, 6115c6b, and b1f38a1 with metadata-only event creation and output sanitization; the installed unique-sentinel sweep remains pending.

### Context and evidence

During the successful 5.0.12 text-clipboard test, `boundlessctl transport events --kind clipboard` printed the first 80 characters of copied text. Clipboard content is stored in the in-memory event ring at event creation and returned unchanged through the raw event API/CLI; only support-bundle generation redacts it. Local-only storage does not make clipboard plaintext appropriate diagnostic metadata.

### What to build

- Record only direction, payload type, byte count, disposition, and non-content-derived correlation metadata for clipboard events.
- Remove content at event creation so later redaction is defense in depth, not the primary privacy boundary.
- Apply output-side protection to raw API/CLI paths so older or malformed event producers cannot expose clipboard content.

### Acceptance criteria

- A unique secret sentinel copied in either direction is absent from the in-memory event store, raw event API, CLI output, logs, and default/full diagnostic bundles.
- Successful, disabled, rejected, deduped, replayed, and apply-failed clipboard cases remain diagnosable using metadata only.
- No preview, reversible encoding, or content-derived value suitable for trivial lookup is retained.
- Tests cover incoming and outgoing text plus image metadata without weakening existing endpoint, identifier, path, and filename redaction.

### Blocked by

None. Coordinate event-shape tests with BND-NEXT-34, but do not block this privacy fix on retention redesign.

---

## BND-NEXT-36 (P2, blocked): Deliver one copied Explorer file through a visible receive workflow

### Context and evidence

The dogfooder naturally copied `Boundless-Clipboard-Test.png` in Explorer and pasted on the peer. Windows supplied a file-list clipboard format; Boundless emitted no file or image event and the paste silently did nothing. The gap was documented but not backlog-ready. Service-mode file configuration also defaulted to `C:\WINDOWS\system32\config\systemprofile\AppData\Local\Boundless`, a LocalSystem-owned location a desktop user cannot reasonably discover or use, with trusted-peer auto-accept correctly defaulting false.

### What to build

- Define and deliver one local regular-file clipboard vertical slice: capture the copied path, apply existing size/trust/receive policy, transfer it, and make remote paste/receipt destination and outcome visible.
- Require a user-visible receive folder before enabling receipt; never present the service profile directory as a normal desktop destination.
- Show progress, consent/auto-accept decision, final path, collision rename, rejection, and interrupted cleanup through the tray/Transfer Center.
- Defer folders, multiple files, network paths, resumable transfer, and shell-extension parity unless separately approved.

### Acceptance criteria

- Copying one disposable local file in Explorer and pasting through the supported peer workflow produces one byte-identical file in the explicitly selected user folder.
- Default-deny receive policy gives an actionable consent state, and trusted-peer auto-accept works only after explicit opt-in.
- Duplicate names are collision-safe; interrupted or rejected transfers leave no ambiguous final file and expose cleanup status.
- No file-list attempt is silently treated as bitmap clipboard, and unsupported multi-file/folder/network inputs explain the limitation.
- Installed service-mode tests prove both directions with a user-owned receive directory and no writes to systemprofile.

### Blocked by

BND-NEXT-24 broker fault containment and supported-size image proof. Explicit send-file remains the supported fallback meanwhile.

---

## Post-launch candidates

### BND-NEXT-30: Remote audio device sharing (mic + output across peers)

Dogfood want (2026-07-08): use the personal PC's SM7B/Wave XLR mic and HD 560S headphones for meetings running on the work PC. No mainstream software KVM (MWB, Synergy, Logitech Flow) does this — a real differentiator, but a new subsystem. Phase it by cost:

1. **30a — output forwarding (no driver):** capture the remote PC's render endpoint via WASAPI loopback, Opus-encode, stream over the existing trusted transport, play on the local device. Core engineering is shared audio plumbing: jitter buffer + adaptive resampling for inter-machine clock drift. LAN latency budget ~20–50ms is fine for meetings. Ships standalone value ("hear the other PC in your headphones"). Caveat: loopback observes rather than redirects — the remote PC still renders locally unless its default output points somewhere inaudible.
2. **30b — virtual microphone (driver project):** meeting apps must enumerate a mic *device*; Windows has no user-mode virtual audio endpoint API, so this requires a signed kernel-mode driver (SYSVAD-derived; EV cert + attestation signing + servicing burden) — months, and corporate IT may block third-party drivers on exactly the machines that need it. Stopgap option: render the remote mic stream into a user-installed VB-CABLE and document device selection.

Do not start before the P0/P1 stories above land; write the full implementation brief only after 30a is validated on real hardware. Leans on: healthy transport (proven 2026-07-07) and readable event logs (BND-NEXT-25) for stream diagnostics.

---

## Deliberately not in this backlog

- Relay/cloud transport, QUIC/iroh/libp2p migrations: the 2026-07-07 evidence shows direct TCP with role reversal satisfies the LAN dogfood; revisit only per the decision gates in the one-sided-reachability doc.
- Clipboard image streaming/spooling, lock-screen/elevated-app service parity, mixed-DPI matrix: tracked as explicit gaps in [project-status.md](project-status.md); they need dedicated evidence-driven slices, not backlog stubs.
- Broader file-transfer UX beyond BND-NEXT-36’s one-file tracer bullet: folders, multiple files, network paths, resumable transfer, and richer shell integration remain deferred until the P0/P1 clipboard and broker work is proven.
