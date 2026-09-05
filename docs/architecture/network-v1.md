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
    - cancellation-safe `WireFrameReader` offsets
    - `inbound_transfers`
    - `inbound_clipboard_image_transfers`
    - `outbound_transfer_flow`
  - Names session exits with `SessionExitReason`.
  - Delegates message-specific behavior to specialized handlers.
- `crates/daemon/src/network/control.rs`
  - Handles `Hello`, `HelloAck`, and `Heartbeat` transport control frames.
  - Enforces canonical `PROTOCOL_CURRENT` handshake acceptance.
  - Flushes queued input on successful handshake control transitions; bulk waits for the session-owned startup turn.
- `crates/daemon/src/network/outbound.rs`
  - Owns outbound queue drain/requeue and backpressure behavior.
  - Enforces file and clipboard-image chunk credit flow and chunk credit caps.
  - Converts outbound state payloads to wire messages and writes framed bytes.
- `crates/daemon/src/network/inbound.rs`
  - Owns inbound file transfer lifecycle:
    - `FileStart`
    - `FileChunk`
    - `FileEnd`
  - Handles temporary-file staging, size validation, and store-finalization.
  - The user-file IO integration keeps OS file handles and user leases in daemon-owned inbound state; `peer-transport` retains protocol flow policy. See [user file IO authority](user-file-io.md) for impersonation and expired-lease cleanup constraints.
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
- Header and payload offsets survive cancellation. A reader future runs alongside the session reactor even while a branch awaits egress; its mailbox is bounded by 2 MiB of payloads and 256 frames, plus one frame being assembled. The reader shares the session lifetime rather than spawning a detached task.
- Startup bulk is deterministic and bounded: the transport initiator sends at most one ordinary four-payload bulk batch, hands the turn to the acceptor with `StartupSyncComplete`, and waits for the return marker before normal bulk ticks drain the remainder.
- Non-canonical protocol peers are rejected at handshake and guarded again in outbound send paths.
- Protocol 4.5 retains physical keyboard identity and credited clipboard-image chunks, and adds consented diagnostic probe/reply messages. Exact-version handshakes reject older peers; both PCs need compatible builds. Configuration schema 6 migrates durable settings while excluding live connection observations.

## Authenticated session ownership

- A trusted peer may reach the daemon through either a locally initiated outbound connection or a reverse-initiated inbound connection. A nonpreferred direction is accepted when it is the only authenticated route, preserving one-sided LAN reachability.
- When both physical directions race, both peers derive the same preferred connection: the lexicographically smaller machine id initiates it. The preferred authenticated session replaces and cancels an already claimed nonpreferred session; later nonpreferred duplicates cannot displace it.
- Outbound worker registration ids and inbound task registration ids are also the ownership ids used by the session registry, so replacement can cancel the exact displaced task instead of aborting an unrelated peer session.
- Session claim, close, and outbound-failure transitions are serialized per peer. Queue drain/write/flush holds that peer's ownership transition guard, so a replacement cannot drain later input or bulk payloads while the superseded lane still owns an earlier batch. A superseded session may clean up its private transfer state, but only the session that still owns the registry claim can publish `connected=false`; stale teardown or a delayed failed dial cannot disconnect or clear a replacement session.
- Input and bulk queues remain peer-owned rather than direction-owned. Either the preferred connection or a sole-reachable nonpreferred connection must flush payloads after `Hello` negotiation.
- Socket frame writes and flushes are individually bounded. A timed-out partial frame makes that connection unusable and returns the payload to peer-owned state before replacement can drain later work.
- Continuously serviced bounded reads now have an in-memory regression for repeated simultaneous post-startup maximum-size text over a 4 KiB duplex stream. This is a runtime contract test, not physical-network latency or complete file-transfer endurance evidence.

## Retry, admission, and durable state

- Each outbound worker owns an absolute retry deadline: 1, 2, 4 seconds up to 30 seconds. A failed dial or short authenticated-session exit schedules the next deadline; unrelated/shared reconciliation notifications cannot shorten it. A session lasting at least 10 seconds resets the next delay to one second. Explicit reconnect cancels and recreates the worker.
- Failed-connect warnings are emitted on the first failure and then at most once per minute per worker with the accumulated attempt count. Bounded in-memory diagnostics retain failed-attempt metadata; the disk-log sink must enforce its separate retention budget.
- At most 16 address candidates race for one peer, with 75 ms staggering and a configured-address slot preserved. TCP connection is limited to four seconds; TLS to five seconds; the entire outgoing establishment future to eight seconds.
- Both listener families share a 32-session admission budget. Incoming TLS expires after five seconds. Scoped task sets own accepted sessions, outbound workers and racing candidates. Registration guards clean up early errors and cancellation, including cancellation before a child first polls.
- Runtime peer `connected` and `last_seen` observations are excluded from config schema 6. Schema 5 migrates while preserving durable identity, endpoints, layout and features. Peer transitions do not write settings or wait for disk IO. Repeated disconnected observations retain idempotent safety cleanup but do not repeat lifecycle notifications unless input capture or ownership is actually released. Settings saves preserve concurrent in-memory peer observations when publishing their new snapshot.
- Downgrading across the schema/protocol boundary requires a pre-upgrade configuration backup or a compatible newer build. The migration does not create or claim a backup, and old runtime connectivity must never be restored as authority.

The opt-in `network::tests::transport_safety_benchmark` emits machine-readable synthetic measurements for worker retry cadence under unrelated wake traffic and healthy-peer input egress while another peer's bulk writer is stalled. It asserts its budgets and does not open an installed runtime, generate physical input, or claim a hardware latency measurement.

## Paired diagnostics integration (protocol 4.5)

`state/paired_testing.rs` owns volatile peer consent, lease deadlines, request and byte budgets, one outstanding local run/request, reply correlation, and source/version/session evidence. Session input ticks and flush signals may emit one `DiagnosticProbe` after ordinary input egress. Inbound dispatch performs an in-memory echo only through the active authenticated session and a local consent lease of at most 600 seconds. Late replies and replies from a mismatched session cannot satisfy pending requests.

The diagnostic budget is 64 KiB per payload, 256 requests/16 MiB per lease, 100 samples per workload and 30 seconds per run. Probes do not log per request or perform file, clipboard, or input actions. Actual socket provenance distinguishes loopback from `real_paired`; the in-memory harness remains `synthetic`. The combined implementation and acceptance contract are documented in `docs/performance/paired-testing.md`; the transport safety benchmark above does not substitute for those paired tests.

## Slice 1 regression focus

- Backpressure contract coverage:
  - `flush_applies_file_chunk_backpressure_contract`
- Session-control behavior coverage:
  - `hello_handler_rejects_machine_id_mismatch_with_error_frame`
  - `hello_handler_accepts_canonical_protocol_and_emits_ack_for_inbound`
  - `hello_ack_handler_flushes_input_only_and_defers_bulk_to_session_turn`

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
- The hardening regressions add two-peer stalled-egress isolation and simultaneous bounded-stream maximum-text exchange. Multi-process, physical-network, and complete file-transfer fault/endurance coverage remain acceptance work.

Deferred reactor work:

- A full `SessionEvent` enum and `SessionPhase` state machine are not implemented. They remain future options if later behavior changes need explicit transition rules beyond the current private branch helpers.
- The hardening changes add scoped task sets and cancellation-safe registration cleanup after BND-NEXT-7. Graceful draining of file-transfer state after abrupt cancellation remains separate reliability work.
- Retry/resume behavior, product UX, diagnostics expansion, TLS/auth changes, public APIs, and transport protocol semantics remain out of scope.

Current work priorities and physical acceptance requirements are maintained in `docs/backlog.md` and `docs/project-status.md`; this ownership map is not a release-readiness claim.
