# One-Sided Reachability Pairing Design

Status: proposed for BND-NEXT-20A. This document is design-only and does not approve implementation, firewall mutation, relay/cloud operation, or a transport dependency change.

## Decision

Boundless should beat Mouse Without Borders for the current dogfood failure by evolving the existing Rust/Tokio TCP design first:

1. Keep the BND-NEXT-18 dual-stack listeners and endpoint-candidate model.
2. Add deterministic candidate exchange, provenance, and Happy-Eyeballs-style racing for pairing and transport.
3. Add explicit role reversal: when peer A discovers peer B but cannot reach B inbound, A can ask B, through the existing pairing/control plane where trust permits, to initiate the TCP session back to A. The first authenticated session that proves the expected peer identity wins.
4. Keep BND-NEXT-21 as a separate user-visible decision for installer-owned Private/local-subnet Windows Firewall policy.
5. Defer QUIC, iroh, rust-libp2p, and any relay/cloud dependency until real two-PC evidence shows the hardened direct TCP route still cannot satisfy local-network dogfood.

This is the smallest route that can beat MWB for Boundless specifically: it matches MWB's practical tolerance for one-sided inbound reachability without copying broad firewall behavior or legacy trust semantics. It preserves Boundless's explicit trust ceremony, TLS peer identity checks, redacted diagnostics, and installer/service safety boundaries.

## Problem Statement

The 2026-06-21 dogfood result showed both trays discovering each other over mDNS, while TCP pairing and transport reachability were asymmetric across UniFi network routes. Discovery was not the only failure. One side could see the other as a peer, but a TCP connect to the selected pairing or transport endpoint could still time out or be refused.

Mouse Without Borders works in the same environment often enough to be the baseline. Local PowerToys MWB source shows why: MWB installs or offers firewall rules, opens dual-family sockets, tries multiple resolved addresses, sends repeated handshakes, cleans up duplicate sockets, and keeps reconnecting after reset or timer signals. Boundless needs the useful reliability ideas, but with stricter trust, clearer diagnostics, narrower firewall posture, and no lock-screen/UAC/elevated-app parity claims.

## Current Boundless Baseline

BND-NEXT-18:

- Transport and nearby-pairing listeners can accept IPv4 and IPv6 through dual-stack sockets or separate family listeners.
- Discovery preserves endpoint candidates rather than collapsing to one IPv4 endpoint.
- Pairing-port derivation handles bracketed IPv6 endpoints.

BND-NEXT-19:

- CLI/tray diagnostics separate mDNS discovery from TCP pairing and transport reachability.
- Pairing tries candidates in order and can succeed on a later reachable candidate.
- Manual-host failure copy no longer claims mDNS discovery.
- Transport reachability diagnostics redact raw endpoint details to family plus port.

The remaining gap is one-sided reachability. If only peer B can initiate an outbound TCP connection to peer A, Boundless still needs a protocol path that lets B initiate without weakening trust or accepting the wrong peer.

## Local MWB Baseline

Verified local source root: `D:/Source/PowerToys`.

| MWB behavior | Verified source | Boundless translation |
| --- | --- | --- |
| Installer-owned local-subnet firewall exception for the MWB executable. | `installer/PowerToysSetupVNext/MouseWithoutBorders.wxs` defines a WiX firewall exception named `PowerToys.MouseWithoutBorders` with `Scope="localSubnet"` and `IgnoreFailure="yes"`. | Worth considering only in BND-NEXT-21. Boundless should use explicit user-visible policy, Private/local-subnet scope, exact service binary ownership, repair/remove behavior, and release docs. |
| Elevated fallback that shells out to `netsh advfirewall`. | `src/modules/MouseWithoutBorders/ModuleInterface/dllmain.cpp` launches an elevated `cmd.exe` that deletes inbound rules for the program and adds a TCP allow rule with `remoteip=any profile=any`. | Do not copy. This is too broad for Boundless's installer/service posture and should not be hidden in a pairing slice. |
| Dual-mode IPv6 clients. | `src/modules/MouseWithoutBorders/App/Class/SocketStuff.cs` creates `TcpClient(AddressFamily.InterNetworkV6)` and sets `Client.DualMode = true`; clipboard code does the same. | Already translated by BND-NEXT-18 with `socket2`/Tokio dual-stack listener support and candidate preservation. |
| Multiple address attempts. | `SocketStuff.cs` combines user-defined name-to-IP mappings with DNS results, keeps IPv4 and IPv6 addresses, optionally validates reverse lookup, then starts a client thread for each validated address. | Translate as deterministic endpoint-candidate racing, not unbounded thread fan-out. Preserve provenance: mDNS, manual host, configured address, cached route. |
| Repeated connect and handshake. | `SocketStuff.cs` retries `TcpClient.Connect` until a timeout, sends multiple handshake packets, and processes `Handshake`/`HandshakeAck`. | Translate as bounded probes and authenticated session handshakes. Boundless must rely on existing trust material and TLS peer identity, not repeated unauthenticated acceptance. |
| Duplicate socket cleanup. | `SocketStuff.cs` closes duplicate connected client sockets for the same remote machine and force-closes stale/error/timeout sockets. | Translate as single-winner session ownership: the first authenticated candidate pair for the expected peer wins; other candidate tasks are cancelled and recorded. |
| Reset-triggered reconnect. | `frmScreen.cs` responds to reopen flags and `REOPEN_WHEN_WSAECONNRESET` by reopening or updating client sockets. | Translate as existing reconnect generation plus fault-harness tests. Do not hide transport instability; surface reconnect reason and candidate outcomes. |

MWB is therefore a useful reliability baseline, but not the right safety baseline. Boundless should copy the concepts of multiple candidates, dual-stack, role tolerance, and duplicate cleanup. It should not copy broad profile-any firewall mutation, raw endpoint-heavy logs, or trust-by-shared-key UX.

## Recommended Protocol Shape

### Pairing

Pairing should remain explicit and consent-based:

1. The initiating tray displays the peer discovered by mDNS or entered manually.
2. The requester sends a bounded pairing probe to all pairing endpoint candidates using deterministic racing: group by provenance, interleave IPv6 and IPv4, stagger attempts by a short delay, and cancel losers after one success.
3. If direct requester-to-responder TCP pairing fails but discovery metadata says the responder has seen the requester, the requester records a pending role-reversal request in local state and shows "waiting for the other peer to connect back".
4. The responder, while still showing an explicit consent UI and code challenge, may initiate a pairing TCP connection back to requester candidates.
5. The pairing state machine accepts only the expected request id, nonce, code challenge, peer identity material, and remote machine id. Wrong-peer, stale nonce, expired code, and replayed approvals must remain rejected or idempotent as they are today.
6. Once trust is committed, reconnect/transport delivery remains separate from durable trust. If the post-commit transport session cannot be established, the user sees a connectivity-pending or reachability-failed state, not a failed trust commit.

Manual host fallback follows the same model but uses manual provenance. It must never claim mDNS discovery. If role reversal needs discovery metadata that a manual host does not provide, the UI should say so and ask the user to try the peer from discovery or enter a reachable host on the other machine.

### Transport

Transport should use the same candidate model after trust:

1. Each trusted peer maintains local candidates: discovered mDNS transport endpoints, configured/manual endpoint, last-known successful endpoint, and optional future firewall-policy provenance.
2. Each peer can initiate outbound transport sessions to the other peer. If both initiate concurrently, TLS peer identity and session ownership decide the winner.
3. Session ownership rule: accept only certificates matching the trusted peer machine id, reject topology/server-name mismatch as today, choose one active session per peer and direction policy, cancel duplicate candidates once the selected session reaches authenticated `Hello`/`HelloAck`, and preserve reconnect generation so stale sessions exit.
4. Endpoint racing is deterministic and bounded: start with last-known successful family/address if still fresh, interleave IPv6 and IPv4 candidates, race discovered and configured candidates with a small stagger, cap total attempts and wall-clock budget, and record redacted candidate outcomes.
5. If one side cannot accept inbound but can dial outbound, the peer that can dial should still establish the authenticated transport session. The product should describe this as "connected by reverse initiation" or similar neutral copy, not as relay or firewall bypass.

### Candidate Provenance Model

Every endpoint candidate should carry:

- `family`: ipv4, ipv6, hostname, or unknown.
- `port`: pairing or transport port.
- `source`: mdns, manual-host, configured-peer, last-success, role-reversal-request.
- `role`: local-dial, remote-dial-requested, inbound-observed.
- `freshness`: timestamp and TTL.
- `redacted_label`: family plus port for support surfaces.
- `sensitive_endpoint`: raw address stored only where local runtime needs to dial.

Support bundles and logs should default to `redacted_label`, source, outcome, and timing. Raw IPs, hostnames, machine ids, request ids, session ids, fingerprints, and trust material remain out of support surfaces unless a local-only diagnostic mode explicitly asks the user to include them.

## Option Evaluation

| Option | Reliability | Safety/privacy | Implementation risk | Recommendation |
| --- | --- | --- | --- | --- |
| Current Tokio/TCP plus role reversal and RFC 8305-style racing | High for local firewall/VLAN asymmetry where at least one direction is reachable. Uses existing TLS/trust/session code. | Best fit. No new cloud dependency. Redaction model already exists. | Moderate, because pairing and transport session ownership need careful state tests. | Recommended for BND-NEXT-20B through 20D. |
| Installer-owned Private/local-subnet firewall policy | High for Windows Defender Firewall blocking inbound on Private LAN. | Acceptable only if explicit, scoped, repairable, and documented. | Moderate installer/security policy work. | Defer to BND-NEXT-21 as separate approval. |
| QUIC/Quinn local-only transport | Potentially useful for streams, migration, and UDP behavior, but does not itself solve firewall policy or discovery. | No cloud by itself, but changes protocol surface and dependency footprint. | High migration cost from current TLS/TCP session model. | Defer until TCP role reversal evidence is insufficient. |
| iroh-style QUIC direct plus relay fallback | Strong direct/relay connectivity model and good Rust ergonomics. | Relay changes product privacy, operating cost, and threat model. Endpoint IDs and relay metadata need policy. | High dependency and product-surface change. | Not approved. Keep as future design input only. |
| rust-libp2p AutoNAT/DCUtR/Circuit Relay style | Mature P2P concepts: reachability probing, hole punching, relays, behavior composition. | Relay and public P2P assumptions do not match a local-first desktop tool by default. | Very high complexity and dependency weight. | Do not adopt now. Borrow concepts only. |
| Full RFC 8445 ICE | Comprehensive candidate exchange/check/nomination model. | Full STUN/TURN/relay semantics would expand privacy and infrastructure scope. | High complexity for TCP local LAN use. | Use as vocabulary and test inspiration, not full implementation. |

## BND-NEXT-21 Firewall Policy Recommendation

This section is policy-only. It does not approve installer changes, helper changes, firewall mutation, elevation, or a release claim. BND-NEXT-21 should remain human-gated until a follow-up implementation task explicitly approves the installer UX and validation packet.

### MWB Comparison

PowerToys Mouse Without Borders has two relevant firewall paths:

- The installer-owned WiX path creates a `PowerToys.MouseWithoutBorders` firewall exception for the MWB executable with `Scope="localSubnet"` and `IgnoreFailure="yes"`.
- The module fallback shells out through elevated `cmd.exe` and `netsh advfirewall`, deletes inbound rules for the MWB executable, then adds a TCP allow rule with `remoteip=any profile=any`.

Boundless should translate only the installer-owned, scoped, repairable idea. It should not copy the fallback shape that opens all profiles or any remote address, deletes rules by program as a side effect, or hides elevation inside a connectivity flow.

### Recommended Shape If Later Approved

The recommended Boundless policy is an explicit, user-visible installer/helper option, not silent mutation:

- Ownership: MSI/helper-owned Windows Defender Firewall rule for the installed Program Files service binary only: `%ProgramFiles%\Boundless\boundless-service.exe`.
- Scope: Private profile plus local-subnet remote scope, or a narrower user-approved remote scope if the implementation can verify it. No Public profile and no router-forwarding guidance.
- Ports: TCP `15100` for trusted transport and TCP `15200` for nearby pairing. Do not open TCP `15101`; that port is only a side-by-side diagnostic probe today. Alternate `network_port` support must be a separate explicit flow that opens only the selected transport port and derived pairing port.
- UX: installer/helper copy must say the rule allows inbound Boundless pairing and transport from the local private network. It must be opt-in or an explicit reviewed installer choice; do not make it an invisible side effect of pairing, diagnostics, reset, or role reversal.
- Repair/update: MSI repair must recreate the exact approved rule when the option is enabled. MSI upgrade must keep ownership tied to the current Program Files service path. Uninstall must remove the MSI-owned Boundless rule and must not delete unrelated user-created firewall rules.
- Observability: diagnostics should report whether the expected rule exists, its profile, remote scope, program path, and ports using local process/path redaction policy where needed.

### Fail-Closed Requirements

The implementation must fail closed and leave firewall state unchanged when any prerequisite cannot be verified:

- `%ProgramFiles%\Boundless\boundless-service.exe` is missing, not the active installed service binary, not MSI-owned, or resolves outside the expected install root.
- The intended desktop user SID, service registration, or LocalSystem AutoStart service boundary cannot be verified.
- The requested profile/scope cannot be represented as Private plus local-subnet or narrower.
- The implementation cannot prove the rule it will repair/remove is the MSI-owned Boundless rule.
- The user declines or the installer/helper property is absent.

Failure copy should point users back to diagnostics and manual Private-profile guidance. It must not fall back to `profile=any`, `remoteip=any`, broad `netsh` commands, firewall edits during pairing, or hidden elevation.

### Evidence Before Connectivity Claims

Before Boundless claims frictionless MWB-like install connectivity, release evidence must show:

- Static installer evidence for the rule's program path, TCP ports, profile, remote scope, and remove-on-uninstall behavior.
- Helper/installer fixture evidence that the option is explicit, fail-closed, and does not infer the wrong user or service path.
- Windows installer lab evidence that install creates the rule only when approved, repair restores it, upgrade preserves it for the current service binary, and uninstall removes it.
- Negative evidence that Public-profile and `remoteip=any profile=any` rules are not created.
- Real two-PC Private-network dogfood evidence showing pairing and transport without manual firewall edits, plus diagnostics proving the expected rule shape on both machines.

Until that evidence exists, Boundless can say it has a proposed local-subnet firewall policy. It must not claim MWB parity, lock-screen/UAC/elevated-app parity, or automatic firewall setup.

## Security, Trust, and Privacy Requirements

- Both peers must explicitly consent before trust is established.
- Pairing code, nonce, request id, and trust material must remain bound to the intended peer and expire as today.
- Role reversal must not allow "whoever connects first" acceptance. The connection must authenticate as the expected peer.
- Duplicate role-reversal attempts must be idempotent. Replayed approvals should return already-trusted/connectivity-pending states where appropriate.
- A failed reconnect must not roll back a durable trust commit.
- Candidate exchange must be local and bounded; do not broadcast raw trust material or peer secrets.
- Logs, tray messages, CLI output, and support bundles must use redacted family-plus-port labels by default.
- No lock-screen, secure desktop, UAC, elevated-app, or Mouse Without Borders parity claims are allowed from this design.

## Failure Copy Requirements

Diagnostics should preserve the distinctions introduced by BND-NEXT-19:

- `mdns=discovered`, `pairing_reachability=failed`: "The peer was discovered, but TCP pairing candidates were not reachable."
- `manual-host`, `pairing_reachability=failed`: "The manually entered host or port was not reachable."
- `role_reversal=requested`: "Waiting for the other peer to initiate a reachable pairing connection."
- `trust=committed`, `transport=connectivity_pending`: "Trust is established; transport is not yet connected."
- `transport=reverse_initiated`: "Connected after the other peer initiated the transport session."

Firewall/VLAN/asymmetric-route guidance should stay separate from trust/code/daemon failure. Firewall rules remain manual or BND-NEXT-21-managed, not automatic in this design.

## Implementation Roadmap

### BND-NEXT-20A: design/fixtures only

This PR. Add architecture decision, source appendix, and no runtime changes.

### BND-NEXT-20B: control-plane candidate exchange/provenance model

- Add DTOs/state fixtures for candidate provenance.
- Add redaction fixtures for family, port, source, role, and outcome.
- No role reversal yet.

### BND-NEXT-20C: pairing role-reversal prototype

- Add bounded reverse-initiation pairing flow behind focused daemon/tray tests.
- Preserve explicit code/approval ceremony.
- Add wrong-peer, stale-request, duplicate-request, timeout, and manual-host regressions.

### BND-NEXT-20D: transport reconnect/session ownership tests

- Extend the existing fault harness for simultaneous outbound attempts and duplicate session cleanup.
- Add one-sided reachability fixture: A cannot dial B; B can dial A; authenticated transport still converges to one session.
- Record redacted candidate outcomes.

### BND-NEXT-21: local-subnet firewall policy decision

- Separate security/product decision for Windows Firewall policy.
- Decide installer/helper behavior, UI copy, rollback, repair, and Public-network exclusion.

### BND-NEXT-22: MWB side-by-side/port collision dogfood support

- Detect MWB or other local process listeners on Boundless ports.
- Surface process owner and alternate-port strategy without corrupting trust.

## Acceptance Criteria for Implementation Approval

Implementation should stay blocked until this design is approved and the next slice has a precise task brief. Approval should answer:

- Is direct TCP role reversal acceptable for pairing and transport?
- What UI language should describe reverse initiation without implying relay or firewall bypass?
- Should BND-NEXT-21 firewall policy be default-on, prompted, diagnostics-only, or deferred?
- What two-PC evidence is required before considering QUIC, iroh, libp2p, or relay/cloud?

## Current-Source Appendix

### Local PowerToys / Mouse Without Borders

- `D:/Source/PowerToys/installer/PowerToysSetupVNext/MouseWithoutBorders.wxs`
  - WiX firewall extension creates a `PowerToys.MouseWithoutBorders` exception scoped to `localSubnet`.
  - Implication: installer-owned firewall policy is a major part of MWB's reliability story, but it needs a separate Boundless security decision.
- `D:/Source/PowerToys/src/modules/MouseWithoutBorders/ModuleInterface/dllmain.cpp`
  - Elevated fallback deletes existing inbound rules for the executable and adds a TCP allow rule with `remoteip=any profile=any`.
  - Implication: do not copy; this is broader than Boundless should silently apply.
- `D:/Source/PowerToys/src/modules/MouseWithoutBorders/App/Class/TcpServer.cs`
  - Uses `TcpListener.Create(port)` for listener setup.
  - Implication: MWB's server-side behavior is framework-managed; Boundless's explicit socket2/Tokio setup is more testable.
- `D:/Source/PowerToys/src/modules/MouseWithoutBorders/App/Class/SocketStuff.cs`
  - Uses dual-mode IPv6 `TcpClient`, user-defined name-to-IP mappings, DNS address lists, repeated connect attempts, repeated handshake packets, duplicate socket cleanup, invalid-key handling, and reconnect flags.
  - Implication: translate candidate fan-out, handshake recovery, and duplicate cleanup; do not translate broad logging or legacy trust UX.
- `D:/Source/PowerToys/src/modules/MouseWithoutBorders/App/Form/frmScreen.cs`
  - Timer/hotkey/reset paths call `ReopenSockets` or `UpdateClientSockets`.
  - Implication: Boundless should use reconnect generation and fault-harness tests for the same recovery pressure.

### Online / Primary Sources

- Microsoft Winsock dual-stack sockets: https://learn.microsoft.com/en-us/windows/win32/winsock/dual-stack-sockets
  - Windows Vista and later can use an IPv6 socket in dual-stack mode when `IPV6_V6ONLY` is set to zero before bind; accepted IPv4 peers appear as IPv4-mapped IPv6 addresses.
  - Implication: BND-NEXT-18's socket2 dual-stack setup is the right Windows-native baseline.
- RFC 8305 Happy Eyeballs v2: https://www.rfc-editor.org/rfc/rfc8305.html
  - Multiple addresses can differ in reachability; racing ordered connection attempts reduces user-visible delay while preserving deterministic policy.
  - Implication: Boundless should race endpoint candidates with short staggering rather than serially waiting through long failures.
- RFC 8445 ICE: https://www.rfc-editor.org/rfc/rfc8445.html
  - ICE sorts candidate pairs, sends connectivity checks in priority order, acknowledges checks from the other agent, and nominates selected pairs.
  - Implication: Borrow candidate/check/nomination vocabulary, but avoid full STUN/TURN unless the product approves relay/cloud scope.
- RFC 9000 QUIC: https://www.rfc-editor.org/rfc/rfc9000.html
  - QUIC combines cryptographic and transport handshakes, supports streams, and includes path/migration machinery.
  - Implication: Quinn/QUIC may help later reconnect and stream behavior, but it is a transport migration, not a small BND-NEXT-20 implementation.
- socket2 `Socket::set_only_v6` / `only_v6`: https://docs.rs/socket2/latest/socket2/struct.Socket.html
  - `set_only_v6(false)` allows IPv4-mapped IPv6 traffic; `true` restricts the socket to IPv6 and allows separate IPv4/IPv6 applications to bind the same port.
  - Implication: keep explicit socket configuration and collision tests.
- Tokio `TcpListener::from_std`: https://docs.rs/tokio/latest/tokio/net/struct.TcpListener.html
  - The caller must set the standard listener to nonblocking before handing it to Tokio.
  - Implication: custom socket setup must keep nonblocking conversion tests.
- Quinn: https://docs.rs/quinn/latest/quinn/
  - A QUIC endpoint can be both client and server and exposes bidirectional streams.
  - Implication: useful future option if TCP session ownership becomes too costly, but not needed for the smallest local-LAN fix.
- iroh: https://docs.rs/iroh/latest/iroh/
  - Provides endpoint-centric QUIC connections and exposes relay-related types/configuration.
  - Implication: strong connectivity layer, but relay defaults or assumptions would change Boundless's privacy and operating posture.
- rust-libp2p AutoNAT: https://docs.rs/libp2p/latest/libp2p/autonat/index.html
  - Provides NAT-status probing behavior.
  - Implication: useful concept for diagnostics, but heavy for a local desktop product.
- rust-libp2p DCUtR: https://docs.rs/libp2p/latest/libp2p/dcutr/index.html
  - Implements Direct Connection Upgrade through Relay.
  - Implication: depends on relay-style coordination; not approved for Boundless now.
- Tailscale NAT traversal notes: https://tailscale.com/blog/how-nat-traversal-works
  - NAT/firewall traversal often requires both peers to coordinate; relay is the fallback when direct tricks fail.
  - Implication: direct local role reversal is worth doing first; relay/cloud must be an explicit product decision.
- Liang et al., "Implementing NAT Hole Punching with QUIC": https://arxiv.org/abs/2408.01791
  - Recent research reports QUIC hole punching and connection migration can reduce restoration time in weak networks.
  - Implication: supports keeping QUIC on the future evaluation list, not adopting it before direct TCP evidence is exhausted.
- Trautwein et al., "Large-Scale Measurement of NAT Traversal for the Decentralized Web": https://arxiv.org/abs/2604.12484
  - Reports large-scale DCUtR/IPFS traversal measurements and notes relay reservation/public address discovery prerequisites.
  - Implication: relay-assisted P2P can work, but it brings prerequisite infrastructure and product-policy costs that Boundless has not approved.
