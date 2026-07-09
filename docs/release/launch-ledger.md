# Launch Ledger

A running ledger of what real dogfood sessions hit, kept separate from the formal gate system in [release-readiness.md](release-readiness.md). Gates prove evidence exists; this ledger tracks the stories that must land, the flows that must be smooth, and the paper cuts observed along the way. Update it during every dogfood session — an entry here is cheaper than rediscovering the same wedge two weeks later.

Status legend: `open` / `in-progress` / `fixed (commit)` / `needs-evidence` (fix landed, real two-PC proof missing).

## P0 — must land before launch

| # | story | status | notes |
| --- | --- | --- | --- |
| 1 | BoundlessService honors SCM stop | needs-evidence (0828513, v5.0.11) | Fix landed: control handler reports `STOP_PENDING` and aborts daemon runtime work. Historic symptom: service stayed `Running`, blocked MSI `ServiceControl(Action=2,Wait=1)` ~2 min per install, wedged a full installer session (2026-07-07, CODY-PC). CHICKEN-AND-EGG: the fix ships in the 5.0.11 binary, but any upgrade FROM a pre-5.0.11 service is still stopping the OLD buggy binary, so 5.0.10→5.0.11 will still stall/lock files and must be done by manually stopping the old service first (elevated `Stop-Service -Force`, fall back to `Stop-Process`). The real test of the fix is the FIRST 5.0.11→later upgrade: capture a verbose msiexec log with no ServiceControl stall, plus `Stop-Service` finishing in seconds with a broker attached and a peer connected. Confirmed 2026-07-08 (CODY-PC): 5.0.10 service running as PID, un-elevated `Stop-Service` is Access Denied by the standard service DACL (IU lacks `SERVICE_STOP`; only SY/BA can stop) — expected, not a regression. |
| 2 | Installer-owned Private/local-subnet firewall rule ([BND-NEXT-21](../backlog.md)) | open | The single biggest reliability gap vs Mouse Without Borders. MWB's installer creates a program-scoped `localSubnet` exception; Boundless installs nothing, so two-PC reachability depends entirely on pre-existing rules and fails asymmetrically. Manual `New-NetFirewallRule` steps in a runbook are not a launch answer. Policy shape already designed in [one-sided-reachability.md](../architecture/one-sided-reachability.md). |
| 3 | Two-PC direct-TCP smoke passes with asymmetric reachability | fixed (evidence 2026-07-07) | PASSED on real hardware: after true trust rotation on both PCs, pairing + trusted transport established with one-sided reachability (second PC could not reach first; first initiated). Layout propagation, Easy Mouse edge handoff, and typing all confirmed smooth. Root cause of the historic blocker was stale service trust (P0 #4), not transport. |
| 7 | Clipboard share works in service mode | needs-evidence (9bd45dd, v5.0.11) | Fix landed: clipboard is broker-routed through the tray's user session when the daemon runs as a service (same attach/exchange pattern as input); direct backend preserved for user-session daemon mode. Root cause was session-0 window-station clipboard isolation, confirmed 2026-07-07. Evidence needed: two-PC service-mode copy/paste both directions (text + image within limits), echo suppression holds, degraded reason visible when no broker is attached. Service-mode clipboard has NEVER worked — this will be its first live proof. |
| 4 | Trust rotation is a product flow, not a script | open ([BND-NEXT-27](../backlog.md)) | Stale LocalSystem service-profile trust caused the entire 5.0.8 blocker, and both the 5.0.8 and 5.0.9 reset scripts failed to actually rotate it (5.0.9's parser bug fixed in c2e1509). Rotate/reset needs first-class tray + CLI UX with loud failure, not a PowerShell script whose fallback silently cleans the wrong profile. |
| 5 | Packaging scripts get CI coverage | fixed (0305022) | Packaging-script self-tests and the CLI `daemon status` output contract (shared fixture `packaging/windows/fixtures/daemon-status-single-line.txt`) now run in `ci.yml` and release validation. Reintroducing the c2e1509 parser bug class fails CI. |
| 6 | MWB coexistence / port-collision UX (BND-NEXT-22) | in-progress | Diagnostics detect MWB on 15100/15200 and emit guidance. Product-level alternate-port flow is still manual config. Dogfooders live with MWB installed; launch users may too. |

## Flows that must be smooth

Each of these should work first-try, with a clear next step printed when it can't. Current friction observed in real sessions:

| flow | current friction |
| --- | --- |
| Install / upgrade | SCM-stop fix landed in v5.0.11 (P0 #1) but is unproven against a real-binary upgrade — the 5.0.10→5.0.11 install is the test. Historic failure mode for reference: ~2-min stall at "configuring", wedged session leaving ghost state (files copied, product unregistered, invisible session-0 msiexec holding the mutex). |
| Reset / recover | Fixed in c2e1509 but the shape remains fragile: daemon-API failure degrades to a warning plus user-profile-only cleanup. A failed service-trust reset should be unmissable. |
| Pair | Proven on asymmetric hardware 2026-07-07 (P0 #3) and felt smooth end-to-end. Remaining polish: verify the reverse-initiation copy ("waiting for the other peer to connect back") appears in the real UI when pairing is started from the blocked side. |
| Diagnose in one command | Close. `Boundless-ConnectivityDiagnostics.ps1 -RemoteHost x` plus `transport events` `failure_reason` is nearly there. BND-NEXT-25 removes periodic `input_runtime_wake` safety ticks from the retained event ring and adds `transport events --kind <substring>` / `--exclude-kind <substring>` filters, so clipboard/anti-idle/transport evidence remains readable without manual log scraping. |
| Uninstall / reinstall | Now testable: with the SCM-stop fix in v5.0.11, run a real uninstall/reinstall cycle on dogfood hardware and record it here. Formal gate exists (installer-smoke) but had only run where the service stopped cleanly. |

## Nits and paper cuts

- `daemon status` reports `daemon_version=5.0.0` on a 5.0.10 install — the workspace version bump in v5.0.11 (55c5f6b) likely resolves this; verify on the installed 5.0.11 build.
- `boundlessctl` output is single-line `key=value` prose; machine consumers (scripts) have to regex it. A `--json` flag would have prevented the c2e1509 bug class outright.
- `Boundless-Install.ps1` printed `boundless_install_exit_code=0` on a run where nothing was installed (wedged-mutex session, 2026-07-07). The helper should verify installed version/registration after msiexec returns instead of trusting the exit code.
- `Package.wxs` `MajorUpgrade` lacks `AllowSameVersionUpgrades`, so dogfood rebuilds at the same semver silently no-op as upgrades. Either bump the patch version every dogfood artifact (current workaround) or allow same-version upgrades for dogfood builds.
- WiX incremental build: renaming the MSI output with a stale `packaging/windows/installer/obj/` fails with MSB3030. `package-windows.ps1` should clean `obj/`/`bin/` first.
- `input_lock_supported` reported `false` until the service was restarted after install — startup ordering between daemon and service-mode input leaves a degraded state that looks like a capability gap.
- Fresh-install service state observed `Stopped` with `StartType=Automatic` after an earlier session (2026-07-07 morning, likely fallout of the wedged install rather than a distinct bug — recheck after P0 #1).
- Smoke-zip hash in handoff notes was hand-transcribed and garbled (61 chars); handoffs should paste from `HANDOFF-SHA256SUMS.txt`, never retype.
- Two `boundlesstray` processes observed in session 1 after install/restart churn (2026-07-07). BND-NEXT-31 now has a per-user/session ownership guard and existing-window activation path; installed-build and MSI upgrade evidence remain pending.

## Session log

- **2026-07-08 (CODY-PC):** v5.0.11 released on GitHub with the P0/P1 top slice: SCM stop (0828513, BND-NEXT-23), broker-routed service-mode clipboard (9bd45dd, BND-NEXT-24), transport-event readability + `--kind`/`--exclude-kind` (a96613b, BND-NEXT-25), packaging-script/CLI-contract CI (0305022, BND-NEXT-26), plus release-smoke fixes (#132–#135). Two-PC smoke bundle built from the release: `dist/two-pc-smoke-v5.0.11/Boundless-5.0.11-release-v5.0.11-two-pc-smoke.zip`, SHA256 `A0D5AF02CE2CA81C7476E55F4EA6ACC3DFA66665A16CBC191576F8974C37F804`. Next dogfood converts P0 #1 and #7 from needs-evidence to fixed: upgrade both PCs 5.0.10→5.0.11 with verbose msiexec logs (first real-binary upgrade over a running service), then copy/paste both directions.
- **2026-07-07 evening (CODY-PC + CODY-ELITEBOOK 10.10.0.187):** TWO-PC SMOKE PASSED for transport/input/layout on 5.0.10-dogfood-c2e1509: trust rotation on both sides fixed the historic connect blocker; asymmetric reachability tolerated (first PC initiated, green both sides); layout propagated; Easy Mouse + typing smooth. Clipboard share FAILED — diagnosed as session-0 clipboard isolation (P0 #7): daemon-side clipboard runtime cannot see or write the user desktop clipboard in service mode. Also confirmed `input_runtime_wake` event flood (~20/s) and duplicate tray processes.
- **2026-07-07 (CODY-PC + 10.10.0.187):** Found/fixed reset-script machine_id parse bug (c2e1509). Diagnosed wedged Windows Installer session from BoundlessService SCM-stop hang. Built 5.0.10-dogfood-c2e1509 (binaries identical to 5.0.9, fixed script embedded). CODY-PC fully prepped. Two-PC pairing evidence pending on second PC prep.
- **2026-06-21 dogfood (from architecture doc):** mDNS discovery mutual, TCP pairing/transport asymmetric across UniFi routes. Motivated BND-NEXT-18/19/20 work.
