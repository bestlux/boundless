# Boundless Backlog

Prioritized, implementation-ready story briefs for the path to a real release. Each brief is written to be handed to a coding agent as a standalone task: context and evidence first, then scope, likely files, acceptance criteria, and validation. Keep IDs stable; mark stories done here and in [release/launch-ledger.md](release/launch-ledger.md) when they land.

Ordering rationale: P0 stories are things a launch user would hit in their first hour (broken upgrade path, broken clipboard). P1 stories are the reliability/trust surface that made dogfood expensive. P2 stories are hardening and paper cuts that compound.

Evidence base: the 2026-07-07 two-PC dogfood sessions recorded in [release/launch-ledger.md](release/launch-ledger.md). Transport, pairing, layout propagation, and input latency are PROVEN GOOD on real asymmetric-reachability hardware (5.0.10-dogfood-c2e1509). The stories below are what's left.

---

## BND-NEXT-23 (P0): BoundlessService must honor SCM stop

### Context and evidence

On a real 5.0.10 install (2026-07-07, CODY-PC), `BoundlessService` never left `Running` state after SCM stop requests — it never even reported `StopPending`. Consequences observed live:

- MSI `ServiceControl(Action=2, Wait=1)` stalled the visible installer at "configuring" for ~2 minutes per install (msiexec gives up waiting, continues).
- One installer session wedged entirely: files copied but the product was never registered with Windows Installer, an invisible session-0 `msiexec` held the `_MSIExecute` mutex, and every subsequent install failed with "another installation is in progress" until `msiserver` was force-stopped and `boundless-service.exe` killed.
- `Restart-Service BoundlessService` hangs indefinitely; only `Stop-Process -Force` works.
- The 5.0.9→5.0.10 upgrade only survived because the binaries were byte-identical (no locked files). Any real binary upgrade will hit locked-file failures or forced reboots until this is fixed.

Prime suspect: the service control handler (or the dispatcher thread that services it) is blocked. The service-mode input broker work (PR #129, commit 51352a3) added long-running runtime loops; check whether the control handler callback shares a thread or lock with them, or whether stop is signaled but the runtime never drains. `capture_backend_mode=user_session_broker` was active when the hang was observed, with a tray broker attached.

### Scope

- The SCM control handler must accept `SERVICE_CONTROL_STOP`, immediately report `STOP_PENDING` with a wait hint, trigger daemon shutdown, and report `STOPPED` when the process is ready to exit.
- Shutdown must complete within single-digit seconds even with: an attached input broker mid-exchange, connected peers, active capture, and pending runtime tasks. `RuntimeTaskShutdown::AbortOnDaemonShutdown` tasks must actually abort.
- If clean drain cannot finish inside the wait hint, prefer abort-and-exit over hanging; a service that dies fast is better than one that wedges installers.

### Non-goals

Do not change MSI `ServiceControl` authoring, service account, or startup type. Do not add a watchdog process.

### Likely files

- `crates/daemon/src/service_main.rs` (control handler, status reporting)
- `crates/daemon/src/runtime_tasks.rs` (shutdown semantics)
- `crates/daemon/src/state/input_broker_ops.rs` / `crates/daemon/src/input/runtime.rs` (broker exchange loops that may block shutdown)
- `scripts/dev/service-smoke.ps1` (stop-timing evidence)

### Acceptance criteria

- `Stop-Service BoundlessService` completes in under 5 seconds with an attached broker and a connected peer (or in a fault-harness equivalent if two-node service state is not reachable in CI).
- Service smoke evidence records stop duration and fails if it exceeds a threshold (suggest 10s).
- A fresh MSI install/upgrade over a running service shows no multi-minute `ServiceControl` stall in a verbose msiexec log.
- Regression test: a unit/integration test proving the shutdown signal aborts the runtime task set even while an input-broker exchange future is pending.

### Validation

`scripts/dev/check.ps1 -Area workspace`, `scripts/dev/service-smoke.ps1` elevated on Windows, plus one manual `msiexec /i ... /l*v` upgrade log inspection.

---

## BND-NEXT-24 (P0): Broker-routed clipboard for service mode

### Context and evidence

Two-PC smoke 2026-07-07: with `share_clipboard=true` on both machines and a healthy trusted transport, copy-paste does nothing in either direction. Diagnosis (confirmed by direct observation): the daemon runs inside `boundless-service` in session 0; `crates/daemon/src/clipboard.rs` polls `WindowsClipboardBackend` in-process. Session 0's window station has an isolated clipboard — user copies are never seen (a local `Set-Clipboard` produced zero clipboard transport events), and inbound remote payloads are written to a clipboard no user can paste from. Input works only because it is brokered through the tray's user session (`crates/tray/src/input_broker.rs`, design in `docs/architecture/user-session-input-broker.md`). Clipboard was never brokered: service-mode clipboard has never worked.

### Scope

- Extend the existing user-session broker protocol so the tray-side broker owns clipboard access when the daemon reports `service_session_unsupported` interactive capability:
  - user→peers: broker polls `GetClipboardSequenceNumber` in its session, reads new payloads, and hands them to the daemon in the existing exchange (or a parallel clipboard RPC — prefer whatever keeps the 8/40ms input exchange loop free of large payload stalls).
  - peers→user: daemon queues remote payloads for the broker; broker applies them with the existing bounded-retry semantics.
- The daemon's clipboard runtime keeps its current queue/echo-suppression/hash logic; only the backend moves behind the broker when in service mode. In user-session daemon mode the direct `WindowsClipboardBackend` path stays as-is.
- Authorization mirrors input: the attach is authorized against the verified pipe client identity, not payload contents.
- When no broker is attached in service mode, clipboard state must report a visible degraded reason (e.g. `clipboard_backend=broker_unavailable`) in UI snapshot/diagnostics rather than silently dropping.

### Non-goals

File-transfer paste, clipboard history UX, and image streaming/spooling (tracked as the `clipboard_image_spooling` gap) stay out; existing size limits apply to brokered payloads unchanged.

### Likely files

- `ipc_api` protos: `InputBrokerExchangeRequest/Reply` or new clipboard broker messages
- `crates/daemon/src/clipboard.rs` (backend indirection — the `ClipboardBackend` trait already exists and is test-faked)
- `crates/daemon/src/state/clipboard_ops.rs`, `state/input_broker_ops.rs`
- `crates/tray/src/input_broker.rs` (or a sibling `clipboard_broker.rs` sharing the attach/supervisor pattern)
- `platform_windows::clipboard_backend` (reused from the tray process)

### Acceptance criteria

- Service-mode two-PC: text copy on A pastes on B and vice versa; image copy within existing size limits works; echo suppression prevents loops (copy on A, paste on B, copy something else on B does not re-apply the old A payload).
- Daemon-side unit tests cover the broker-backend selection and the degraded no-broker path using the existing fake-backend test style in `clipboard.rs`.
- UI snapshot/diagnostics expose which clipboard backend is active.
- No regression in user-session (non-service) clipboard tests.

### Validation

`scripts/dev/check.ps1 -Area workspace`; real two-PC service-mode copy/paste both directions is the release evidence (record in launch ledger session log).

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

## BND-NEXT-25 (P1): Transport event log must stay readable under runtime wake noise

### Context and evidence

Confirmed live 2026-07-07: `kind=input_runtime_wake` (`detail=source=safety_tick`) is recorded ~20 times per second, so `boundlessctl transport events --limit 15` covers under one second of history and `--limit 500` covered ~25 seconds. During the 5.0.8 debugging this noise buried the transport evidence that would have shortened diagnosis by days. Wake ticks are periodic runtime bookkeeping, not transport events.

### Scope

- Stop recording periodic wake ticks in the transport event ring by default (or sample/aggregate them: e.g. one summary event per interval with a tick count). Preserve non-periodic wake reasons if any carry diagnostic value.
- Add `--kind <substring>` / `--exclude-kind <substring>` filters to `boundlessctl transport events` for tray/CLI parity with diagnostics needs.
- Keep event-buffer capacity semantics documented (how much wall-clock history a full buffer represents must be diagnosable).

### Likely files

- `crates/daemon/src/state/diagnostics_ops.rs` (event recording), whatever emits `input_runtime_wake` in `crates/daemon/src/input/runtime.rs`
- `crates/cli/src/commands.rs` (filter flags)

### Acceptance criteria

- After an idle minute with a connected peer, `transport events --limit 100` shows meaningful history (connections, clipboard, anti-idle) rather than wake ticks.
- Filters work and are covered by CLI tests; recording change covered by state tests.

### Validation

`scripts/dev/check.ps1 -Area workspace`; manual `transport events` inspection on an installed build.

---

## BND-NEXT-26 (P1): CI coverage for packaging scripts and CLI output contracts

### Context and evidence

`Boundless-Reset.ps1` shipped broken in two consecutive releases (5.0.8 never reset service trust; 5.0.9's rotate-trust call never parsed `machine_id` from `boundlessctl daemon status` single-line output — fixed in c2e1509). Root cause both times: nothing executes the packaging scripts against real CLI output. `-SelfTest` modes now exist in `Boundless-Reset.ps1` and `Boundless-ConnectivityDiagnostics.ps1` but no workflow runs them.

### Scope

- A Windows CI job that runs every packaging-script `-SelfTest` and fails red.
- A contract test binding the CLI's `daemon status` output format to the script parsers: either (a) a fixture file with the canonical status line, asserted by both a Rust CLI test and the script self-tests, or (b) a CI step that runs the built `boundlessctl daemon status` against a stub/fake daemon and pipes the real output through `Get-MachineIdFromStatusOutput`. Option (b) is stronger; take it if the daemon test harness allows cheap startup.
- Add `Boundless-Install.ps1 -ResolveOnly` smoke to the same job (no elevation needed).

### Likely files

- `.github/workflows/` (new or existing Windows job)
- `packaging/windows/*.ps1` (self-test entry points already exist)
- `crates/cli` (status output format test)

### Acceptance criteria

- Reintroducing the c2e1509 parser bug (anchor `^machine_id=`) fails CI.
- The job runs on PRs touching `packaging/` or `crates/cli`, and in the release workflow.

### Validation

Green run on a no-op PR; red run demonstrated with the deliberate regression locally (do not merge the red state).

---

## BND-NEXT-27 (P1): Trust rotation and reset as a first-class product flow

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

## BND-NEXT-22 (P2): MWB side-by-side and port-collision productization

Scoped in [architecture/one-sided-reachability.md](architecture/one-sided-reachability.md) §BND-NEXT-22. Dogfood reality (2026-07-07): MWB runs on both PCs between Boundless test windows as the user's working fallback, and owns TCP 15100 (IPv6) while Boundless holds IPv4 — a silent split. Diagnostics already detect and classify this; the product does not.

Scope: surface the collision in the tray (peer health or a dedicated warning), and provide a guided alternate-`network_port` flow that applies the same port on all trusted peers (pairing port = network_port + 100). Acceptance: with MWB listening on 15100, the tray shows the collision and the guided flow moves both machines to a working alternate port without breaking trust. Files: diagnostics surface already exists in `crates/app-services/src/diagnostics.rs`; add tray workflow + config propagation.

---

## BND-NEXT-28 (P2): `boundlessctl --json`

Machine-readable output (`--json`) for `daemon status`, `peer list`, `transport events`, `feature list`. Motivation: the c2e1509 bug class — scripts regex-scraping single-line prose — plus future CI contract tests (BND-NEXT-26) and support tooling. Keep the human format the default and unchanged. Acceptance: the four commands emit stable JSON with a `schema_version` field; `Boundless-Reset.ps1` machine-id lookup prefers JSON with regex fallback; CLI tests snapshot the JSON shape. Files: `crates/cli/src/commands.rs`, `crates/cli/src/console.rs`, `packaging/windows/Boundless-Reset.ps1`.

## BND-NEXT-29 (P2): Install/packaging paper cuts (bundle)

Independent small items, one PR each or one sweep; all observed 2026-07-07:

1. `Boundless-Install.ps1` printed `boundless_install_exit_code=0` on a run where nothing was installed (wedged-mutex session). After msiexec returns, verify Windows Installer product registration and `package-manifest.json` version match the MSI being installed; exit nonzero with a clear message otherwise.
2. `scripts/release/package-windows.ps1` must clean `packaging/windows/installer/obj` and `bin` before `dotnet build` (stale obj + renamed output ⇒ MSB3030).
3. Same-version dogfood upgrades silently no-op (`MajorUpgrade` without `AllowSameVersionUpgrades`). Either add `AllowSameVersionUpgrades="yes"` or make the packaging script refuse to build an MSI whose version equals an already-published dogfood artifact. Decide and document in `packaging/windows/README.txt`.
4. `boundlessctl daemon status` reports `daemon_version=5.0.0` on 5.0.10 installs — wire the real package/workspace version through.
5. `boundlesstray` allows multiple instances in one session (two observed live); add a single-instance guard (named mutex) with focus-existing behavior.

Acceptance: each item has a targeted test or self-test where the surface allows (1, 2, 4, 5 are testable; 3 is a policy + doc change).

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
- File-transfer UX productization: next after P0/P1 above; write its brief when clipboard brokerage (BND-NEXT-24) settles the session-boundary pattern it will reuse.
