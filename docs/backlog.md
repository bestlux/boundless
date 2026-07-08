# Boundless Backlog

Prioritized, implementation-ready story briefs for the path to a real release. Each brief is written to be handed to a coding agent as a standalone task: context and evidence first, then scope, likely files, acceptance criteria, and validation. Keep IDs stable; mark stories done here and in [release/launch-ledger.md](release/launch-ledger.md) when they land.

Ordering rationale: P0 stories are things a launch user would hit in their first hour. P1 stories are the reliability/trust surface that made dogfood expensive. P2 stories are hardening and paper cuts that compound.

Evidence base: the 2026-07-07 two-PC dogfood sessions recorded in [release/launch-ledger.md](release/launch-ledger.md). Transport, pairing, layout propagation, and input latency are PROVEN GOOD on real asymmetric-reachability hardware. The 2026-07-08 v5.0.11 release landed the P0 slice in code; converting those to dogfood-proven is now the top of the queue.

---

## Landed in v5.0.11 (2026-07-08) — code complete, evidence status below

Full implementation briefs live in git history (`git show ac4d4d0:docs/backlog.md`).

| story | commit | remaining evidence before marking proven |
| --- | --- | --- |
| BND-NEXT-23 (P0) — service honors SCM stop | 0828513 | The 5.0.11 install over a running 5.0.10 service is the first real-binary upgrade this fix will face. Capture a verbose msiexec log showing no multi-minute `ServiceControl` stall, and `Stop-Service` completing in seconds with a broker attached and a peer connected. |
| BND-NEXT-24 (P0) — broker-routed clipboard | 9bd45dd | Two-PC service-mode copy/paste both directions (text, and image within size limits); echo suppression holds; diagnostics show the active clipboard backend and a visible degraded reason when no broker is attached. |
| BND-NEXT-25 (P1) — transport events readable | a96613b | On an installed build after an idle minute with a peer: `transport events --limit 100` shows real history (no safety-tick flood); `--kind` / `--exclude-kind` filters work. |
| BND-NEXT-26 (P1) — packaging-script CI | 0305022 | Done — self-tests + CLI daemon-status output contract wired into `ci.yml` and release validation, green on main. Optional: demonstrate a deliberate `^machine_id=` regression fails CI locally. |

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

---

## BND-NEXT-28 (P2): `boundlessctl --json`

Machine-readable output (`--json`) for `daemon status`, `peer list`, `transport events`, `feature list`. Motivation: the c2e1509 bug class — scripts regex-scraping single-line prose — plus the CI contract tests (BND-NEXT-26, landed with a text fixture that JSON would make sturdier) and support tooling. Keep the human format the default and unchanged. Acceptance: the four commands emit stable JSON with a `schema_version` field; `Boundless-Reset.ps1` machine-id lookup prefers JSON with regex fallback; CLI tests snapshot the JSON shape. Files: `crates/cli/src/commands.rs`, `crates/cli/src/console.rs`, `packaging/windows/Boundless-Reset.ps1`.

## BND-NEXT-29 (P2): Install/packaging paper cuts (bundle)

Independent small items, one PR each or one sweep; all observed 2026-07-07:

1. `Boundless-Install.ps1` printed `boundless_install_exit_code=0` on a run where nothing was installed (wedged-mutex session). After msiexec returns, verify Windows Installer product registration and `package-manifest.json` version match the MSI being installed; exit nonzero with a clear message otherwise.
2. `scripts/release/package-windows.ps1` must clean `packaging/windows/installer/obj` and `bin` before `dotnet build` (stale obj + renamed output ⇒ MSB3030). Still open as of 0305022 (that commit only added fixture staging).
3. Same-version dogfood upgrades silently no-op (`MajorUpgrade` without `AllowSameVersionUpgrades`). Either add `AllowSameVersionUpgrades="yes"` or make the packaging script refuse to build an MSI whose version equals an already-published dogfood artifact. Decide and document in `packaging/windows/README.txt`.
4. `boundlessctl daemon status` reported `daemon_version=5.0.0` on 5.0.10 installs. The workspace version bump in v5.0.11 (55c5f6b) likely resolves this — verify on the installed 5.0.11 build, and if the value still lags, wire the real package version through.
5. `boundlesstray` allows multiple instances in one session (two observed live); add a single-instance guard (named mutex) with focus-existing behavior.

Acceptance: each item has a targeted test or self-test where the surface allows (1, 2, 5 are testable; 3 is a policy + doc change; 4 is verify-then-fix).

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
- File-transfer UX productization: next after P0/P1 above; write its brief when the 5.0.11 clipboard brokerage (BND-NEXT-24) is dogfood-proven, since it reuses the same session-boundary pattern.
