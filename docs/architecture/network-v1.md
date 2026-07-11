# Network Architecture Map (v1)

This document defines the canonical transport module boundaries after Slice 1 slimdown.

Related design notes:

- [One-sided reachability pairing design](one-sided-reachability.md) records the BND-NEXT-20 recommendation for asymmetric local-network reachability. It is design-only and does not approve relay/cloud transport, firewall mutation, or runtime behavior changes.

## Module responsibilities

- `crates/daemon/src/network/session.rs`
  - Validates post-TLS peer identity before entering the post-auth session loop.
  - Owns the authenticated session context and orchestrates read/write loop branches.
  - Owns per-session runtime state:
    - `SessionRuntime`
    - `inbound_transfers`
    - `inbound_clipboard_image_transfers`
    - `outbound_transfer_flow`
  - Names session exits with `SessionExitReason`.
  - Delegates message-specific behavior to specialized handlers.
- `crates/daemon/src/network/control.rs`
  - Handles `Hello`, `HelloAck`, and `Heartbeat` transport control frames.
  - Enforces canonical `PROTOCOL_CURRENT` handshake acceptance.
  - Flushes queued outbound payloads on successful handshake control transitions.
- `crates/daemon/src/network/outbound.rs`
  - Owns outbound queue drain/requeue and backpressure behavior.
  - Enforces file chunk credit flow (`FileChunkCredit`) and chunk credit caps.
  - Converts outbound state payloads to wire messages and writes framed bytes.
- `crates/daemon/src/network/inbound.rs`
  - Owns inbound file transfer lifecycle:
    - `FileStart`
    - `FileChunk`
    - `FileEnd`
  - Handles temporary-file staging, size validation, and store-finalization.
- `crates/daemon/src/network/inbound_payload.rs`
  - Handles inbound clipboard and input payload processing:
    - `ClipboardText`
    - `ClipboardImage`
    - `InputFrame`
  - Performs machine identity checks and payload-level rejection logging.
- `crates/daemon/src/network/runtime.rs`
  - Owns listener and outbound supervisor loops.
  - Handles endpoint selection/backoff/reconnect scheduling and preserves the outbound worker registration id through its authenticated sessions.
- `crates/daemon/src/network/tls.rs`
  - Owns TLS config, connector/acceptor construction, and server-name parsing.
- `crates/daemon/src/network/codec.rs`
  - Owns wire/input conversion helpers and clock helper utilities.

## Queue ownership and backpressure invariants

- App-level outbound queues remain owned by `AppState`.
- Only `outbound.rs` drains/requeues outgoing payload queues.
- Session-level chunk credit bookkeeping is isolated to `outbound_transfer_flow`.
- Session-level inbound transfer staging is isolated to `inbound_transfers`.
- Non-canonical protocol peers are rejected at handshake and guarded again in outbound send paths.
- Protocol 4.3 keyboard frames retain physical scan/E0 identity plus source Windows virtual-key and effective Num Lock semantics. The clean bincode shape change intentionally rejects 4.2 peers at handshake; local 4.2 runtime config is migrated to 4.3 on upgrade.

## Authenticated session ownership

- A trusted peer may reach the daemon through either a locally initiated outbound connection or a reverse-initiated inbound connection. A nonpreferred direction is accepted when it is the only authenticated route, preserving one-sided LAN reachability.
- When both physical directions race, both peers derive the same preferred connection: the lexicographically smaller machine id initiates it. The preferred authenticated session replaces and cancels an already claimed nonpreferred session; later nonpreferred duplicates cannot displace it.
- Outbound worker registration ids and inbound task registration ids are also the ownership ids used by the session registry, so replacement can cancel the exact displaced task instead of aborting an unrelated peer session.
- Session claim and close transitions are serialized. A superseded session may clean up its private transfer state, but only the session that still owns the registry claim can publish `connected=false`; stale teardown cannot disconnect or clear a replacement session.
- Input and bulk queues remain peer-owned rather than direction-owned. Either the preferred connection or a sole-reachable nonpreferred connection must flush payloads after `Hello` negotiation.

## Slice 1 regression focus

- Backpressure contract coverage:
  - `flush_applies_file_chunk_backpressure_contract`
- Session-control behavior coverage:
  - `hello_handler_rejects_machine_id_mismatch_with_error_frame`
  - `hello_handler_accepts_canonical_protocol_and_emits_ack_for_inbound`
  - `hello_ack_handler_flushes_pending_outgoing_payloads`

## BND-NEXT-7 post-auth reactor boundaries

PRs #90-#93 landed the behavior-neutral BND-NEXT-7 cleanup of the authenticated transport session loop. This section records the finalized architecture state: not a new public API or product feature, but a clearer internal reactor surface around the behavior that already existed.

Landed boundaries:

- TLS setup, certificate validation, and topology peer-identity mismatch checks stay outside the reactor boundary. `run_session` authenticates the peer first, then `run_authenticated_session` drives the post-auth loop.
- `AuthenticatedSession` owns immutable post-auth identity and local snapshot data: authenticated peer id, remote peer id, outbound/inbound direction, local machine id, and local device name.
- `SessionRuntime` owns mutable post-auth state: remote protocol, inbound file transfers, inbound clipboard-image transfers, outbound transfer flow, reconnect generation, frame buffers, and anti-idle pulse timing.
- `SessionExitReason` names clean session exits for reconnect requests, dropped state, peer close, invalid frames, and protocol rejection.
- Outgoing work has named private branch boundaries for heartbeat ticks, input flush ticks, bulk flush ticks, explicit flush signals, file chunk credit handling, and shared input/bulk flush helpers.
- Inbound work has named private boundaries for reading a frame result, decoding/recording rejected frames, and dispatching decoded `WireMessage` values.
- Session-close cleanup still discards inbound transfer staging after the loop exits. It marks the peer disconnected only when that session still owns the active registry claim.

Fault-harness status:

- PR #89 landed a narrow post-auth in-memory harness that can drive the real session loop after TLS authentication has already established peer identity.
- The harness covers read, write, flush, disconnect, delayed-frame, and reconnect-pair scenarios used by the BND-NEXT-7 behavior-preserving slices.
- Broader multi-peer, multi-process, and full runtime fault coverage remains deferred test infrastructure.

Deferred reactor work:

- A full `SessionEvent` enum and `SessionPhase` state machine are not implemented. They remain future options if later behavior changes need explicit transition rules beyond the current private branch helpers.
- Graceful per-session join/drain lifecycle is still separate reliability work; BND-NEXT-7 did not change task supervision, registration cleanup semantics, or transport shutdown policy.
- Retry/resume behavior, product UX, diagnostics expansion, TLS/auth changes, public APIs, and transport protocol semantics remain out of scope.

Next backlog step:

- After BND-NEXT-8A, the deferred clipboard image quality item is inbound/apply full-buffer streaming or spooling if future evidence warrants it; otherwise BND-NEXT-9 service updater and N-1 MSI planning is the next backlog item needing human decision.
