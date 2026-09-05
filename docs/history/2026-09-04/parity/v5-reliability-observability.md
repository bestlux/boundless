# Superseded snapshot — 2026-09-04

This is preserved historical source from commit 9bad1b5c36e31aa81f8764e72fed66785f85c05a. It is not current implementation guidance, release policy, or evidence of later fixes. The original body follows unchanged; its relative links retain their original locations. Use the [original source](https://github.com/bestlux/boundless/blob/9bad1b5c36e31aa81f8764e72fed66785f85c05a/docs/parity/v5-reliability-observability.md) to follow historical references, or the current docs index for the active contract.

---

# V5 Reliability And Observability Contract

Boundless v5 should explain failures without requiring raw log spelunking.

## Implemented In This Milestone

- Paired peer snapshots now include a derived `health_state` and `health_reason`.
- The tray Status tab shows per-peer health labels with hoverable reason text.
- Diagnostics dumps redact machine IDs, fingerprints, API endpoints, peer IDs, request IDs, lockout IPs, local paths, and trust material by default.
- Diagnostics dumps write a sidecar redaction manifest that states default redaction is enabled.
- Existing reconnect generation events remain in the bounded transport event store and now feed peer health classification.

## Honest Limitations

- Peer health is derived from current config plus recent bounded events; it is not a durable incident log.
- Firewall, trust, and protocol labels depend on emitted event details. More transport paths should move to structured reason codes before final release validation.
- Full opt-in diagnostics are not enabled yet. Default support bundles remain redacted.
- Installer/service state is not yet included in diagnostics; V5-9 owns release/install hardening.

## Validation Targets

Before V5 can mark reliability validated, the readiness packet must include:

- reconnect reason tests,
- redaction tests for machine IDs, request IDs, endpoints, paths, trust material, and lockout IPs,
- tray health-state smoke evidence,
- support-bundle artifact examples,
- and explicit skip rationale for installer/service state until V5-9 lands.
