# User-Session Broker

Status: MVP for normal unlocked-desktop input and clipboard in service mode.
This is not a BND-NEXT-9C claim: lock screen, secure desktop, UAC prompts, and
other users' sessions remain unsupported and unvalidated. Ordinary elevated
applications are a narrower planned exception under BND-NEXT-44; they are not
supported by the current tray broker.

## Problem

The MSI-owned `BoundlessService` daemon runs as LocalSystem in session 0.
Windows session isolation means it can never observe or inject interactive
desktop input, and its session-0 clipboard is isolated from the user's
clipboard. Real mouse/keyboard handoff and user-visible clipboard sync need a
process in the intended interactive user session.

## Decision

Host the broker in the existing tray process (`boundlesstray.exe`) instead of a
new helper binary:

- The tray already runs at sign-in in the intended user's interactive session
  as the allowed user, and already speaks the service control pipe.
- The service named pipe ACL (SYSTEM, Administrators, and exactly one allowed
  user SID) is the trust boundary to preserve; the broker rides it and adds no
  new socket, ACL surface, firewall rule, or transport.
- For the normal desktop, a separate helper would add installer, lifecycle, and
  packaging work without improving this security shape.

BND-NEXT-44 is the deliberate exception for ordinary elevated windows. It plans
a separately signed Program Files injector because that process has a different
Windows token/security shape. The existing tray broker remains concurrently
responsible for physical capture, edge lock/emergency detection, clipboard, and
normal service exchange. Only incoming injection records and held-input cleanup
cross the new narrow channel. The tray-broker lease and injector attachment are
distinct; the privileged side authenticates the actual connecting PID, token,
session, canonical signed image/publisher, and per-launch handshake rather than
client-reported identity.

The LocalSystem service remains the trust, pairing, routing, layout, clipboard
sync, and network authority. The broker is deliberately dumb: capture hands,
inject hands, read clipboard, write clipboard, no policy.

## Control Path

Outgoing (local physical input -> peer), service mode with broker attached:

1. Tray broker captures input with the shared `platform-windows`
   `HookInputPump` (low-level hooks + raw input, pressed-state tracking,
   pending-move coalescing, double-Ctrl escape).
2. Broker calls `ExchangeInputBroker` on the control pipe with captured events,
   cursor position, virtual-screen bounds, escape actions, and applied lock
   state.
3. The daemon's `InputBrokerRelay` queues the observations; the existing input
   runtime drains them through the same `capture_and_queue_outgoing_frames`
   path as an in-session daemon: edge-switch handoff, layout resolution,
   release-event flush on target change, per-peer outbound queue, TLS peer
   transport.

Incoming (peer frame -> local injection):

1. Peer transport routes frames through the unchanged `InputRouter`
   owner/policy checks into the pending inject queue.
2. While a fresh broker is attached, the service inject loop leaves the queue
   alone. `ExchangeInputBroker` drains up to 64 frames only when no earlier
   batch is in flight, re-checks `input_injection_allowed_for_peer` per frame,
   assigns a batch ID, and retains that batch until its exact acknowledgment.
3. The broker injects frames in FIFO order with `SendInput` in the user session.
   A partial native send retains the exact uncommitted event suffix and applies
   request-side backpressure, so later frames cannot overtake it. Before any
   suffix retry, another successful exchange revalidates every retained frame's
   owner/share-input authority. Revocation latches a whole-batch cancellation
   under the same ID; the daemon repeats it across response loss until the tray
   drops its local remainder, releases any locally held keys/buttons, and
   acknowledges that ID. A batch is otherwise acknowledged only after every
   frame completes.
4. The daemon assigns one random delivery epoch for its in-memory relay and
   retains an in-flight batch under the same ID across replacement or stale
   re-attach. The tray keeps completed receipts, partial suffix state, and the
   intended held key/button state across its supervisor sessions, but accepts
   them only when the new attach reports the same epoch. Each injected batch
   also carries the daemon's input-authorization generation. The owner, sharing
   policy, owner-transition cooldown, and generation live under one router
   lock: owner transitions, `share_input` changes, and resets advance the
   generation while holding the write lock, while held-state validation and
   batch staging read one coherent snapshot. Pending input is stamped with the
   generation that accepted it and is rejected by both injection paths if that
   generation changes, including after an owner A-to-B-to-A or policy-off/on
   cycle. A completed Down-only batch therefore cannot be restored after
   authority changes even if the same peer later reclaims ownership. Session
   failure first releases committed holds locally so input fails open. A
   partial or zero-record native release failure retains the exact uncommitted
   Up suffix and the same Windows input authority, including any synthetic Num
   Lock key-up still owed after a partial toggle. Bounded cleanup retries run
   before connecting to the daemon again and gate every restore or new payload.
   After same-epoch re-attach, the tray waits for a successful exchange to
   revalidate the retained generation, restores completed or partial holds,
   and only then resumes any returned payload suffix. A partial restore retains
   its exact uncommitted suffix and makes at most one native restore attempt per
   authorized exchange. Cancellation, authorization-generation rejection, or a
   new delivery epoch discards the restore intent. Lost receipt requests and
   request-consumed/response-lost retries remain idempotent without applying a
   payload before its holds.
5. Cooperative shutdown detach carries the tray's latest completed batch ID
   and delivery epoch. Under the capture-transition lock, the daemon validates
   the broker token and epoch, acknowledges that exact batch, and only then
   returns any still-unacknowledged, non-cancelled batch to the front of the
   pending queue; a mismatched receipt fails closed and a latched cancellation
   is never resurrected. A transient exchange failure skips detach while an
   active suffix, held-state restore, or local cleanup remains. That preserves
   the daemon owner/generation and retained ID until the same tray supervisor
   completes local cleanup, re-attaches, submits its receipt, revalidates held
   authority, and restores before resuming payload. A completed exchange with
   no recovery state may still use bounded detach cleanup.

Clipboard (service mode with broker attached):

1. The daemon clipboard runtime does not touch the session-0 clipboard. It
   exposes `clipboard_backend=broker_unavailable` until a fresh authorized
   broker is attached, then `clipboard_backend=user_session_broker`.
2. The tray broker runs a separate `ExchangeClipboardBroker` loop from the
   8/40 ms input exchange, polls `GetClipboardSequenceNumber` in the user
   session, reads changed payloads with the shared Windows clipboard backend,
   and sends one local payload per exchange.
3. The daemon queues broker-observed local payloads through the existing
   clipboard validation, dedupe, echo suppression, replay, and peer outbound
   queue logic.
4. Remote clipboard payloads stay daemon-owned until the broker applies them.
   The daemon stages one payload in the reply, and the broker reports success
   or failure on the next exchange. Success marks the remote payload applied
   and arms echo suppression; failure requeues under the existing bounded retry
   budget.

## Fail-Closed Rules

- Broker authorization is verified server-side from the actual pipe client:
  at accept time the named-pipe server resolves the client's account SID
  (`GetNamedPipeClientProcessId` + process token) and Windows session
  (`GetNamedPipeClientSessionId`) and attaches them as tonic connect info.
  Attach, exchange, and detach are gated on that verified identity only — no
  client-supplied claim exists on the wire.
- The pipe ACL admits SYSTEM and Administrators for diagnostics, but broker
  attach/exchange/detach additionally require the verified client SID to equal
  the configured allowed-user SID (the same SID that scopes the pipe ACL) and
  the verified session to be interactive (session 0 rejected). Admin-only or
  SYSTEM callers are rejected and cannot replace a live allowed-user broker.
- Unverifiable identity (missing connect info, unresolvable SID or session,
  or no configured allowed-user SID) rejects the call outright.
- Attach is rejected unless the daemon was started in service-session mode
  (`InputRuntimeMode::ServiceSessionUnsupported`); a user-session daemon owns
  capture directly and a broker would double-capture.
- Attach also negotiates an exact broker protocol revision. An older tray is
  rejected before it can lock input; a newer tray rejects an unversioned older
  daemon and performs bounded cleanup if that daemon already issued a token.
- Held-input restore is separately authorized by an epoch-scoped daemon
  generation. The generation is issued with an inject batch and revalidated on
  every exchange while a key or button remains down; it is never inferred from
  a client-supplied peer identity. Owner, input-sharing policy, or reset changes
  reject the old generation and force local release without restore.
- The broker adds a per-attachment token, and exchanges with a stale or
  replaced token are rejected.
- Clipboard broker exchange uses the same attachment token and verified-client
  gate as input exchange; clipboard payload contents never authorize broker
  access.
- A broker that stops exchanging for ~3 seconds is treated as detached: backend
  mode reverts to `service_session_unsupported`, capture gates close, and
  pending inject frames fall back to the truthful unsupported-drop path.
- Keys/buttons the broker reported as held get synthetic release events on
  target change or broker loss so remote peers are not left with stuck input.

## Diagnostics

- `input_capture_backend_mode` is `user_session_broker` only while a fresh
  broker is attached; otherwise `service_session_unsupported`.
- `clipboard_runtime.backend_mode` is `direct` for a user-session daemon,
  `user_session_broker` while the service has a fresh authorized broker, and
  `broker_unavailable` while service-mode clipboard sync is degraded.
- Transport events record `input_broker_attached`, `input_broker_attach_rejected`,
  `input_broker_detached`, `input_broker_inject_dispatched`, and
  `input_broker_inject_report` (failures only). Clipboard broker diagnostics
  record rejected broker exchanges and unmatched apply reports.
- The tray Settings tab states the broker scope explicitly and never claims
  lock-screen/UAC/elevated-app control.

## Known Limits / Follow-Ups

- Poll-based exchange (8 ms active / 40 ms idle) adds up to one poll interval
  of latency per direction; a streaming exchange is a candidate follow-up if
  two-PC latency evidence warrants it.
- Pending and in-flight injection queues are intentionally in-memory. Normal
  detach/replacement/stale-recovery preserves or requeues unacknowledged work,
  but a hard daemon-process crash is the durability boundary and can lose those
  frames. A safe reset rotates the delivery epoch before accepting new broker
  receipts.
- Delivery dedupe is exact only while the tray supervisor process retains its
  epoch-scoped receipt. A hard tray crash erases that evidence; the daemon
  deliberately keeps and replays its unacknowledged in-flight batch on the next
  attach (at-least-once), so input that completed immediately before the crash
  can be applied twice. Persisting receipts would be required to close that
  boundary. The tray's locally injected held-state snapshot is process-local as
  well: a hard tray/process crash can erase both its intended hold snapshot and
  any pending exact release suffix before the bounded cleanup loop finishes.
  Abrupt broker death can also leave keys held on a remote
  peer until release synthesis runs on the next capture-target transition.
- Real two-PC dogfood evidence is still required before the parity matrix rows
  can move; nothing here upgrades BND-NEXT-9C claims.
