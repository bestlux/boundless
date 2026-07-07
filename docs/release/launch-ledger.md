# Launch Ledger

A running ledger of what real dogfood sessions hit, kept separate from the formal gate system in [release-readiness.md](release-readiness.md). Gates prove evidence exists; this ledger tracks the stories that must land, the flows that must be smooth, and the paper cuts observed along the way. Update it during every dogfood session — an entry here is cheaper than rediscovering the same wedge two weeks later.

Status legend: `open` / `in-progress` / `fixed (commit)` / `needs-evidence` (fix landed, real two-PC proof missing).

## P0 — must land before launch

| # | story | status | notes |
| --- | --- | --- | --- |
| 1 | BoundlessService honors SCM stop | open ([BND-NEXT-23](../backlog.md)) | Service stays `Running` (never `StopPending`) on stop requests. Blocked MSI `ServiceControl(Action=2,Wait=1)` ~2 min per install, hung `Restart-Service`, and wedged a full installer session (2026-07-07, CODY-PC). Suspect service-mode input broker (51352a3) blocking the control handler. Every future upgrade and uninstall path goes through this. |
| 2 | Installer-owned Private/local-subnet firewall rule ([BND-NEXT-21](../backlog.md)) | open | The single biggest reliability gap vs Mouse Without Borders. MWB's installer creates a program-scoped `localSubnet` exception; Boundless installs nothing, so two-PC reachability depends entirely on pre-existing rules and fails asymmetrically. Manual `New-NetFirewallRule` steps in a runbook are not a launch answer. Policy shape already designed in [one-sided-reachability.md](../architecture/one-sided-reachability.md). |
| 3 | Two-PC direct-TCP smoke passes with asymmetric reachability | fixed (evidence 2026-07-07) | PASSED on real hardware: after true trust rotation on both PCs, pairing + trusted transport established with one-sided reachability (second PC could not reach first; first initiated). Layout propagation, Easy Mouse edge handoff, and typing all confirmed smooth. Root cause of the historic blocker was stale service trust (P0 #4), not transport. |
| 7 | Clipboard share works in service mode | open ([BND-NEXT-24](../backlog.md)) | Confirmed broken 2026-07-07 despite `share_clipboard=true` both sides. The clipboard runtime (`crates/daemon/src/clipboard.rs`) polls `WindowsClipboardBackend` inside the LocalSystem daemon in session 0, whose window-station clipboard is isolated from the user desktop — local copies are never seen, remote payloads are applied to an invisible clipboard. Input works because it is brokered through the tray (`crates/tray/src/input_broker.rs`); clipboard needs the same user-session broker routing (extend attach/exchange with clipboard read/apply ops). Service-mode clipboard has never worked and cannot without this. |
| 4 | Trust rotation is a product flow, not a script | open ([BND-NEXT-27](../backlog.md)) | Stale LocalSystem service-profile trust caused the entire 5.0.8 blocker, and both the 5.0.8 and 5.0.9 reset scripts failed to actually rotate it (5.0.9's parser bug fixed in c2e1509). Rotate/reset needs first-class tray + CLI UX with loud failure, not a PowerShell script whose fallback silently cleans the wrong profile. |
| 5 | Packaging scripts get CI coverage | open ([BND-NEXT-26](../backlog.md)) | `Boundless-Reset.ps1` shipped two releases in a row with a broken primary path because nothing exercises the scripts against real CLI output. `-SelfTest` conventions exist (`Boundless-Reset.ps1`, `Boundless-ConnectivityDiagnostics.ps1`) but are not wired into any workflow. |
| 6 | MWB coexistence / port-collision UX (BND-NEXT-22) | in-progress | Diagnostics detect MWB on 15100/15200 and emit guidance. Product-level alternate-port flow is still manual config. Dogfooders live with MWB installed; launch users may too. |

## Flows that must be smooth

Each of these should work first-try, with a clear next step printed when it can't. Current friction observed in real sessions:

| flow | current friction |
| --- | --- |
| Install / upgrade | Hidden ~2-min stall at "configuring" (P0 #1). A wedged session leaves ghost state: files copied, product unregistered, invisible session-0 msiexec holding the mutex, "another installation is in progress" with nothing visible to the user. 5.0.10 upgrade only avoided file-lock failure because its binaries were byte-identical to 5.0.9. |
| Reset / recover | Fixed in c2e1509 but the shape remains fragile: daemon-API failure degrades to a warning plus user-profile-only cleanup. A failed service-trust reset should be unmissable. |
| Pair | Proven on asymmetric hardware 2026-07-07 (P0 #3) and felt smooth end-to-end. Remaining polish: verify the reverse-initiation copy ("waiting for the other peer to connect back") appears in the real UI when pairing is started from the blocked side. |
| Diagnose in one command | Close. `Boundless-ConnectivityDiagnostics.ps1 -RemoteHost x` plus `transport events` `failure_reason` is nearly there; remaining gap is event noise — confirmed live 2026-07-07: `input_runtime_wake` (`source=safety_tick`) fires ~20/s, so `transport events --limit 15` covers <1s of history and clipboard/anti-idle/transport evidence needs manual filtering. Wake ticks should be excluded by default or the command needs a `--kind` filter. |
| Uninstall / reinstall | Untested against the SCM-stop bug; likely hits the same wedge. Formal gate exists (installer-smoke) but runs where the service stops cleanly. |

## Nits and paper cuts

- `daemon status` reports `daemon_version=5.0.0` on a 5.0.10 install — version string is stale/hardcoded somewhere.
- `boundlessctl` output is single-line `key=value` prose; machine consumers (scripts) have to regex it. A `--json` flag would have prevented the c2e1509 bug class outright.
- `Boundless-Install.ps1` printed `boundless_install_exit_code=0` on a run where nothing was installed (wedged-mutex session, 2026-07-07). The helper should verify installed version/registration after msiexec returns instead of trusting the exit code.
- `Package.wxs` `MajorUpgrade` lacks `AllowSameVersionUpgrades`, so dogfood rebuilds at the same semver silently no-op as upgrades. Either bump the patch version every dogfood artifact (current workaround) or allow same-version upgrades for dogfood builds.
- WiX incremental build: renaming the MSI output with a stale `packaging/windows/installer/obj/` fails with MSB3030. `package-windows.ps1` should clean `obj/`/`bin/` first.
- `input_lock_supported` reported `false` until the service was restarted after install — startup ordering between daemon and service-mode input leaves a degraded state that looks like a capability gap.
- Fresh-install service state observed `Stopped` with `StartType=Automatic` after an earlier session (2026-07-07 morning, likely fallout of the wedged install rather than a distinct bug — recheck after P0 #1).
- Smoke-zip hash in handoff notes was hand-transcribed and garbled (61 chars); handoffs should paste from `HANDOFF-SHA256SUMS.txt`, never retype.
- Two `boundlesstray` processes observed in session 1 after install/restart churn (2026-07-07); tray should enforce single-instance.

## Session log

- **2026-07-07 evening (CODY-PC + CODY-ELITEBOOK 10.10.0.187):** TWO-PC SMOKE PASSED for transport/input/layout on 5.0.10-dogfood-c2e1509: trust rotation on both sides fixed the historic connect blocker; asymmetric reachability tolerated (first PC initiated, green both sides); layout propagated; Easy Mouse + typing smooth. Clipboard share FAILED — diagnosed as session-0 clipboard isolation (P0 #7): daemon-side clipboard runtime cannot see or write the user desktop clipboard in service mode. Also confirmed `input_runtime_wake` event flood (~20/s) and duplicate tray processes.
- **2026-07-07 (CODY-PC + 10.10.0.187):** Found/fixed reset-script machine_id parse bug (c2e1509). Diagnosed wedged Windows Installer session from BoundlessService SCM-stop hang. Built 5.0.10-dogfood-c2e1509 (binaries identical to 5.0.9, fixed script embedded). CODY-PC fully prepped. Two-PC pairing evidence pending on second PC prep.
- **2026-06-21 dogfood (from architecture doc):** mDNS discovery mutual, TCP pairing/transport asymmetric across UniFi routes. Motivated BND-NEXT-18/19/20 work.
