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
