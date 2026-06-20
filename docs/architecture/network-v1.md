# Network Architecture Map (v1)

This document defines the canonical transport module boundaries after Slice 1 slimdown.

## Module responsibilities

- `crates/daemon/src/network/session.rs`
  - Owns TLS session lifecycle and orchestrates read/write loops.
  - Owns per-session transfer maps:
    - `inbound_transfers`
    - `outbound_transfer_flow`
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
  - Handles endpoint selection/backoff/reconnect scheduling.
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

## Slice 1 regression focus

- Backpressure contract coverage:
  - `flush_applies_file_chunk_backpressure_contract`
- Session-control behavior coverage:
  - `hello_handler_rejects_machine_id_mismatch_with_error_frame`
  - `hello_handler_accepts_canonical_protocol_and_emits_ack_for_inbound`
  - `hello_ack_handler_flushes_pending_outgoing_payloads`

## BND-NEXT-7 reactor rewrite planning

The transport session reactor rewrite should stay behavior-preserving until the existing fault harness proves the current behavior on the new boundaries.

Current reactor problems to address:

- `session.rs` still centralizes timers, outgoing flushes, incoming frame dispatch, reconnect checks, transfer cleanup, and session exit handling in one large post-auth loop.
- Reconnect checks are repeated across heartbeat, input flush, bulk flush, flush-signal, and read branches.
- Local session maps for inbound files, inbound clipboard images, and outbound transfer flow share control flow with wire I/O and protocol dispatch.
- Session exit reasons are mostly implicit `break` or error paths, which makes fault handling harder to review.

Preliminary target boundaries:

- Keep TLS authentication and peer identity mismatch checks before the reactor boundary.
- Introduce an internal `AuthenticatedSession` or equivalent context that owns immutable post-auth identity and local snapshot data.
- Move mutable per-session state into a `SessionRuntime` or equivalent internal struct.
- Name reactor inputs as explicit events such as heartbeat tick, input flush tick, bulk flush tick, outgoing flush signal, inbound frame, invalid frame, reconnect requested, state dropped, and I/O failure.
- Name session phases such as starting, awaiting remote hello, active, draining, and closing.
- Name session exits such as peer closed, reconnect requested, invalid frame, I/O failure, state dropped, and protocol rejected.

Fault-harness status:

- PR #89 landed a narrow post-auth in-memory harness that can drive the real session loop after TLS authentication has already established peer identity.
- The harness covers read, write, flush, disconnect, delayed-frame, and reconnect-pair scenarios.
- Broader multi-peer, multi-process, and full reactor-lifecycle fault coverage remains BND-NEXT-7 follow-up work.

Implementation guardrails:

- Do not change protocol semantics, retry policy, transfer resume behavior, or product UX inside the reactor rewrite.
- Land the rewrite in small slices: docs/status cleanup, state extraction, eventized reconnect/exit handling, separated read dispatch/outgoing flush helpers, then final cleanup.
- Each behavior-changing slice should run the PR #89 fault harness plus focused transfer/reconnect tests before workspace validation.
