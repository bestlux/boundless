# V5 Pairing, Trust Rotation, And Network Safety

Boundless v5 should be safer than shared-key pairing without pretending DNS, local administrator boundaries, or LAN reachability are hard security boundaries.

## Goals

- Keep guided nearby pairing as the default user path.
- Preserve manual trust-bundle import/export as a recovery path when discovery fails.
- Provide a "new key" equivalent that revokes peer trust and forces explicit re-pairing.
- Keep peer removal deterministic: remove the peer, revoke its trust record, clear reconnect state, and stop stale sessions.
- Add endpoint policy, subnet policy, firewall diagnostics, and protocol mismatch evidence before public v5 readiness.

## Current V5 Contract

`boundlessctl pair rotate-trust --confirm rotate-trust:<machine-id>` is the CLI trust-rotation path. It requires typed confirmation, clears peer config, clears peer trust, rebuilds the trust store with only the rotated local self-trust record, regenerates local device trust material on disk, aborts registered transport sessions, clears runtime transport/pairing/discovery/input state, and reports `restart_required=true` because the current daemon process still holds the pre-rotation TLS identity in memory. Until restart, trust export is blocked so peers cannot receive a stale in-memory trust bundle.

The typed confirmation is an operator accident-prevention guard, not an authorization boundary. Any caller that can reach the local control plane can read the machine ID from status, so V5 still needs current-user named-pipe ACL and localhost fallback validation before treating destructive local operations as protected from other local users.

While restart is pending after trust rotation, Boundless rejects trust export, trust import, manual join, and nearby pairing mutations so the running daemon cannot mix a rotated on-disk trust epoch with its stale in-memory TLS identity.

`boundlessctl peer remove <peer-id>` revokes the peer trust record and clears runtime state for that peer, including reconnect generation, capture target, input queues, clipboard replay, anti-idle state, discovery endpoint, and active transport sessions.

Guided nearby pairing already includes challenge confirmation, retry behavior, and lockout diagnostics. V5 release readiness still needs end-to-end two-machine pairing recovery evidence after trust rotation, peer removal, stale trust, invalid code, invalid nonce, lockout, and protocol mismatch.

## Network Policy Gaps

The following remain release-blocking V5 work:

- same-subnet-only policy and enforcement,
- remote endpoint validation or warning behavior,
- reverse-DNS warning behavior that does not imply DNS is a trust boundary,
- firewall rule check/install/remove diagnostics,
- protocol mismatch UX beyond transport rejection,
- local control-plane ACL validation for current-user named pipes and service mode.

Until those land, the parity matrix must keep `Validate remote machine IP`, `Same subnet only`, `Add firewall rule`, and `Local control endpoint security` below `validated`.

## Validation Requirements

Before this workstream can be marked `validated`, the readiness packet must include:

- trust rotation test evidence,
- peer removal and reimport recovery evidence,
- guided nearby pairing happy-path evidence,
- invalid code, invalid nonce, replay, and lockout evidence,
- protocol mismatch evidence,
- subnet policy inbound and outbound enforcement evidence,
- endpoint validation warning/enforcement evidence,
- firewall diagnostic evidence,
- local control endpoint ACL evidence.
