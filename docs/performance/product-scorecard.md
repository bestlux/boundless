# Product Performance Scorecard

Status: provisional release-evidence policy. This scorecard turns the current two-machine lab prep into a product decision record, not a default PR gate.

## Policy

Use this scorecard to decide what to improve next and what evidence a release claim still needs. It should identify regressions, measurement gaps, and follow-up actions alongside the latest result. It is not a substitute for the release-readiness packet, installer smoke, service smoke, or workspace checks.

Physical two-machine labs are release evidence, not default PR gates. They stay out of default PR validation until the lab runs are stable, fast, non-disruptive, and backed by repeated real runs. A PR may update fixtures, scripts, or docs with local validation only; a release reviewer decides when real lab evidence is required for the claim being made.

All thresholds in this file start provisional. A threshold can become binding only after at least two real two-PC runs on different days or materially different machine/network conditions show that the band is realistic. Until then, use the bands to classify evidence and choose the next action, not to claim product parity or broad Windows desktop behavior.

Evidence source classes:

| class | meaning |
| --- | --- |
| CI | deterministic unit, fixture, build, lint, or release metadata checks that can run in ordinary automation |
| fixture | synthetic metadata-only lab rows that validate artifact shape and policy labels without exercising product runtime behavior |
| local-lab | one-machine or same-host runtime check useful for diagnostics, but not enough for product release claims |
| manual-lab | real two-PC Windows run with sanitized coordinator and peer packets, known build provenance, and human-controlled disruptive steps where applicable |

## Scorecard

Each release review should record one result per category, the evidence source used, whether the threshold is still provisional, and the next action when the result is warning, fail, or not measured.

| category | evidence type | default scenario | acceptable threshold | warning threshold | fail threshold | source class | threshold state |
| --- | --- | --- | --- | --- | --- | --- | --- |
| install/startup | MSI/tray/daemon health plus process and named-pipe checks | Fresh Windows install or repair install, then tray starts and daemon health is queried through the default Windows endpoint | Install succeeds; exactly one intended tray and daemon/service owner path is active; API health succeeds within a coarse startup window such as under 60s; no stale service or duplicate daemon ownership | Install succeeds only after one manual retry, health is slow but under 120s, or process ownership is explainable but needs release-note treatment | Install fails, daemon/API health is unavailable, stale service owns the pipe, duplicate daemon path is active, or uninstall/repair leaves unsupported state | CI plus manual-lab | provisional |
| discovery/pairing | Pairing and trust recovery result | Two fresh Windows PCs pair, reconnect, and show expected trusted peer status from tray or CLI diagnostics | Pairing completes in both directions without trust reset, peer identity confusion, or repeated manual restarts | Pairing works after one recovery action, diagnostics are unclear, or one direction is noticeably slower | Pairing cannot complete, requires trust deletion without explanation, pairs the wrong peer, or diagnostics hide the failure | fixture plus manual-lab | provisional |
| clipboard text | Latency, success rate, and policy compliance | Synthetic 128 B, 8 KiB, and 255 KiB text payloads A-to-B and B-to-A using sanitized observations | No failed rows; p95 latency remains in a human-immediate band for small/medium payloads and large text remains usable; payload contents are never recorded | One transient failure that recovers, p95 moves into a noticeable delay band, or direction asymmetry needs investigation | Any payload content is recorded, repeated failures occur, current policy rejects an expected text row, or large text becomes effectively unusable | fixture plus manual-lab | provisional |
| clipboard image | Latency, size-policy behavior, memory-safety notes, and success rate | Screenshot-scale and 1080p synthetic BMP rows pass; 4K policy-bound raw BMP row remains a rejected or skipped policy row unless code limits change | Accepted image rows complete without failed observations; policy-rejected rows are classified as no-op; memory notes do not show renewed full-buffer pressure beyond the known deferred inbound/apply gap | Accepted rows are slow enough to feel disruptive, one direction degrades, or memory trend deserves follow-up before wider rollout | Image bytes are captured in evidence, accepted rows fail repeatedly, 4K policy rows are treated as passed without a verified policy change, or memory growth suggests unsafe behavior | fixture plus manual-lab | provisional |
| file transfer | End-to-end duration, throughput, hash/integrity status, cleanup, retry/reconnect counts | Single small file, many small files, and 100 MiB synthetic payload in both directions; 1 GiB remains opt-in | Every enabled row completes with matched hash labels, expected receive-path class, clean temp cleanup, no partial files, and throughput high enough for dogfood use | Transfer succeeds but throughput is low, retries/reconnects appear, cleanup requires follow-up, or many-small-files behavior is materially worse than single-file behavior | Hash mismatch, partial file remains, unexpected receive path, stale temp cleanup, repeated failed rows, or payload data/private paths are recorded | fixture plus manual-lab | provisional |
| reconnect | Recovery latency and state consistency after service/tray/network interruption | Manual service restart and tray restart rows; network-loss row only when explicitly opted in | Peer returns to reachable/stable state without trust reset; tray-visible state matches daemon state; reconnect count is explainable | Recovery requires one manual action, takes long enough to interrupt work, or state mismatch resolves but needs diagnosis | Peer does not recover, trust is lost, tray and daemon disagree persistently, or disruptive steps run without explicit operator choice | fixture plus manual-lab | provisional |
| input handoff | Handoff latency, capture state, active peer class, and failure subsystem | Repeated edge handoff attempts A-to-B and B-to-A with sanitized capture-state observations | Handoff is repeatable in both directions; failure count is zero for supported desktop state; active peer and input capture state agree after each attempt | Occasional missed handoff, asymmetry, or delayed state cleanup that does not strand input | Input is stranded, wrong peer becomes active, capture state remains locked unexpectedly, or unsupported desktop-state claims are implied by the evidence | fixture plus manual-lab | provisional |
| soak stability | Longer-run failure count, reconnect count, and bounded resource trend | 30-minute synthetic fixture row for shape; 2-hour real manual run only when explicitly scheduled | No failed rows, bounded CPU/memory trend, no unexplained reconnect loop, and no stale tray/daemon state after the run | Isolated recoverable failure, mild resource drift, or one unexplained reconnect that needs a follow-up issue | Repeated failures, resource growth trend, daemon/tray crash, stale pipe ownership, or lost peer state after soak | fixture plus manual-lab | provisional |

## Release Interpretation

Ready for dogfood means a release reviewer has current install/startup, discovery/pairing, clipboard text, and input handoff evidence with no fail classifications for the intended dogfood path. Other categories may be warning or not measured only when the gap is written down with a practical next action and the release notes do not imply that missing behavior is ready.

Ready for beta means every scorecard category has at least two real two-PC runs, no fail classifications, no hidden privacy violations in evidence, and every warning has an owner or explicit deferral. Fixture-only rows are useful regression guards, but they do not make a category beta-ready.

Parity claim supported means the specific claim has separate current evidence in the parity matrix and release-readiness packet, plus scorecard evidence for the matching scenario. This scorecard alone does not support claims about service desktop boundaries, lock-screen behavior, secure desktop behavior, elevated applications, UAC prompts, self-update behavior, or broad third-party parity.

## Review Notes

When updating the scorecard for a release:

1. Link the release-readiness packet and any two-machine performance packets.
2. Mark each category as acceptable, warning, fail, or not measured.
3. Record the evidence source class and whether the threshold is still provisional.
4. Turn every warning, fail, and not-measured category into a next action or an explicit release-scope deferral.
5. Keep synthetic fixture results separate from real two-PC results.

Do not convert manual-lab rows into default CI or PR gates until the runs are fast enough for routine development, do not disrupt operator input/network state, and have repeated real-run evidence showing low flake risk.
