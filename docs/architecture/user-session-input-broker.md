# User-Session Broker

Status: MVP for normal unlocked-desktop input and clipboard in service mode;
the v5.0.15 BND-NEXT-44 ordinary elevated-application path is code-complete but
still requires installed UAC and two-PC evidence. Lock screen, secure desktop,
UAC consent/credential screens, and other users' sessions remain unsupported.

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

BND-NEXT-44 is the deliberate exception for ordinary elevated windows. It uses
a separate Program Files injector because that process has a different Windows
token/security shape. The existing tray broker remains concurrently responsible
for physical capture, edge lock/emergency detection, clipboard, and normal
service exchange. Only incoming injection records and held-input cleanup cross
the new narrow channel. The tray-broker lease and injector attachment are
distinct; the privileged side authenticates the actual connecting PID, token,
session, canonical MSI-owned image path, and per-launch handshake rather than
client-reported identity. A signed build additionally authenticates the trusted
image/publisher.

The LocalSystem service remains the trust, pairing, routing, layout, clipboard
sync, and network authority. The broker is deliberately dumb: capture hands,
inject hands, read clipboard, write clipboard, no policy.

### v5.0.15 one-user dogfood exception

The first elevated-input dogfood build may use an unsigned
`requireAdministrator` injector only under this bounded exception:

- the canonical MSI installs and owns the executable under
  `%ProgramFiles%\Boundless`;
- the same split-token administrator is both the configured allowed user and
  the account whose elevated applications receive input;
- the user explicitly enables or launches the injector and Windows displays the
  expected **Unknown Publisher** UAC prompt;
- tray, CLI, and diagnostics report `unsigned dogfood`, never signed, trusted,
  UIAccess, or production-ready; and
- cancellation, sign-in, tray relaunch, injector crash, service restart, and
  automatic retry do not request elevation again. Only another explicit user
  action may produce another UAC prompt.

The exception changes only whether this narrow `requireAdministrator` binary
may be used for one-user dogfood. It does not weaken the PID, token, user,
session, canonical-path, attachment, or minimal-command checks. Because an
unsigned image has no authenticated publisher, path and MSI ownership are not a
substitute for a trusted-publisher claim. Trusted Authenticode signing remains
mandatory for UIAccess and for any polished/trusted-publisher capability.

UAC consent and credential desktops, lock screen, Winlogon, other user sessions,
and standard-user-to-alternate-admin scenarios remain unsupported. The injector
must fail open to normal local control at those desktop or identity boundaries.

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
   When the user explicitly enables administrator-app control, an authenticated
   `requireAdministrator` helper owns only this final injection lane. The tray
   keeps capture, policy, routing, clipboard, and emergency unlock unelevated.
   A partial native send retains the exact uncommitted event suffix and applies
   request-side backpressure, so later frames cannot overtake it. Before any
   suffix retry, another successful exchange revalidates every retained frame's
   owner/share-input authority. Revocation latches a whole-batch cancellation
   under the same ID; the daemon repeats it across response loss until the tray
   drops its local remainder, releases any locally held keys/buttons, and
   acknowledges that ID. A batch is otherwise acknowledged only after every
   frame completes.
4. The daemon assigns a random delivery epoch for its in-memory relay and
   retains an in-flight batch under the same ID across stale re-attach only
   when the transport verifies the same process incarnation (PID, creation
   time, account SID, and Windows session). The tray keeps completed receipts, partial suffix state, and the
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
   An uncertain elevated receipt never replays a possible committed prefix.
   The tray retains conservative key/button releases, the detach atomically
   discards the uncertain batch, and the daemon quarantines the affected peer
   against automatic owner claim until a fresh explicit handoff. A helper crash
   permits direct cleanup only after its per-user/session lane mutex disappears.
   A replacement process cannot prove any previous receipt. Before touching
   configuration or queueing peer releases, the daemon clears local lock and
   capture state. The previous capture target and exact release set survive
   queue failure and capture-stream reset, so retry cannot silently lose them.
   It then revokes incoming ownership, requires a fresh explicit
   handoff, discards pending and uncertain interactive payload, and rotates the
   delivery epoch. The daemon remembers acknowledged held keys/buttons and
   conservatively adds every possibly committed Down from the unacknowledged
   batch; an unacknowledged Up cannot prove a release. The replacement receives
   only synthesized Ups until that cleanup is acknowledged. Cleanup bypasses
   remote-owner validation only inside the relay's private release constructor;
   it cannot contain peer-supplied Downs, motion, or wheel actions. Each cleanup
   batch is capped at 256 events and retains normal exact-receipt/backpressure
   handling. No old payload is restored across the process boundary.
5. Cooperative shutdown detach carries the tray's latest completed batch ID
   and delivery epoch. Under the capture-transition lock, the daemon validates
   the broker token and epoch, acknowledges that exact batch, and only then
   returns any still-unacknowledged, non-cancelled batch to the front of the
   pending queue; a mismatched receipt fails closed and a latched cancellation
   is never resurrected. A transient exchange failure skips detach while an
   active suffix, held-state restore, or local cleanup remains. An elevated
   delivery uncertainty instead sets `reset_input_session`: detach discards the
   retained batch, releases only an affected current owner, and records bounded
   batch/frame evidence before a new handoff can be accepted. Otherwise this
   preserves the daemon owner/generation and retained ID until the same tray supervisor
   completes local cleanup, re-attaches, submits its receipt, revalidates held
   authority, and restores before resuming payload. A completed exchange with
   no recovery state may still use bounded detach cleanup.
6. The dashboard's local pause control latches pause and synchronously releases
   the active hook lock before any control-plane request. The supervisor cancels
   a stalled exchange, releases held input before detach IPC, discards retained
   payload through a session reset, and stays paused until explicit resume.
   Resume is allowed by the UI only after enabling daemon policy succeeds.
   A failed detach never authorizes resuming the paused payload suffix.
   After that bounded cleanup, a no-hook broker session keeps clipboard service
   alive while paused, including after reconnection. Local broker protocol
   revision 7 adds a volatile `input_paused` heartbeat: it revokes input ownership
   and ordinary delivery under the existing authorization lock without writing
   configuration. This works even when saving the sharing preference fails.
   Paused replies can contain only conservative key/button releases; the tray
   validates that restriction before native application and acknowledges only
   completed cleanup. Resume permits a fresh handoff and cannot revive the old
   owner, captured payload, or key-down suffix. Clipboard freshness still depends
   on broker heartbeats; clipboard activity alone never sustains input authority.

The BND-NEXT-44 candidate changes step 3 only when the user explicitly enables
administrator-app control. It remains an experimental dogfood capability until
the installed MSI, UAC, elevated Terminal/IDE, crash cleanup, and two-PC checks
are recorded.

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
  (`GetNamedPipeClientProcessId` + process token), process creation time
  (`GetProcessTimes` on that process handle), and Windows session
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
- The broker adds a per-attachment token bound to its verified process
  incarnation. Missing creation-time evidence, a stale/replaced token, or a
  different process presenting that token fails closed. A copied token cannot
  refresh the original process's input lease.
- Clipboard broker exchange uses the same attachment token and verified-client
  gate as input exchange; clipboard payload contents never authorize broker
  access.
- A broker that stops exchanging for ~3 seconds is treated as detached: backend
  mode reverts to `service_session_unsupported`, capture gates close, and
  pending inject frames fall back to the truthful unsupported-drop path.
- Keys/buttons the broker reported as held get synthetic release events on
  target change or broker loss so remote peers are not left with stuck input.
- The injector derives PID, token integrity, user SID, Windows
  session, image path, and attachment identity from the actual connection. The
  experimental unsigned exception permits only the canonical MSI-owned Program
  Files image for the same split-token administrator; it does not permit
  client-reported identity or arbitrary elevated executables.
- A signed/UIAccess build additionally requires the expected trusted
  Authenticode chain and publisher. An unsigned build must never advertise or
  negotiate UIAccess.
- Cancellation or attachment loss latches elevated-app input unavailable until
  the user explicitly enables it again. No broker, service, sign-in task, crash
  recovery, or retry loop may launch the `requireAdministrator` injector or
  create a UAC prompt automatically.

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
- The v5.0.15 BND-NEXT-34 slice adds bounded injector capability/stage reasons
  for unavailable, rejected, wrong-session, and injection-failed states without
  retaining individual input content. The full generic telemetry story remains
  open.
- The tray Settings tab states the broker scope explicitly. The experimental
  enabled build says `unsigned dogfood` and never claims UAC-desktop,
  lock-screen, or trusted-publisher support.

## Known Limits / Follow-Ups

The pure delivery-state benchmark runs without sockets, disk writes, or native
input. It exercises 64-frame / 192-event batches through exact acknowledgment
and through uncertain-delivery release recovery:

```powershell
cargo test -p boundless-daemon broker_delivery_state_benchmark --lib -- --ignored --nocapture
```

It emits JSON with profile, iterations, and p50/p95/p99 nanoseconds. A local debug
run on 2026-09-04 (10,000 iterations each) measured p95 of 25.5 microseconds for
stage/ack and 20.0 microseconds for replacement/release recovery. These are
in-memory implementation measurements, not end-to-end input-latency budgets;
compare the same build profile and host, and retain physical latency evidence
separately.

- Poll-based exchange (8 ms active / 40 ms idle) adds up to one poll interval
  of latency per direction; a streaming exchange is a candidate follow-up if
  two-PC latency evidence warrants it.
- Input queues and delivery evidence remain in memory. Same-process transient
  recovery preserves exact receipts/suffixes; a replacement process deliberately
  loses uncertain payload and requires fresh handoff. Hard daemon death also
  loses the conservative held-state evidence. Persisting receipts alone cannot
  atomically commit Windows input side effects and their acknowledgements.
- The supervisor runs independently of dashboard rendering on its own thread,
  but still shares the tray process lifetime. Full separation into a user-session
  engine remains future work. Hard death can lose platform-native cleanup details
  (including synthetic modifier/toggle bookkeeping); the new relay release set
  is conservative logical-input recovery, not a claim of exactly-once native
  recovery. Keys can remain held until a replacement broker reaches cleanup;
  outgoing holds depend on daemon release synthesis reaching the previous peer.
- Local fault tests cover PID reuse, lost receipts, partial sends, stale
  generations, helper uncertainty, a stalled control call during pause, and
  config-lock contention during replacement. They do not invoke host input APIs
  or substitute for physical key-up, elevated-app, and two-PC fault evidence.
- Real two-PC dogfood evidence is still required before the parity matrix rows
  can move. The implemented one-user unsigned exception does not itself prove
  BND-NEXT-44 and does not upgrade any BND-NEXT-9C secure-desktop, lock-screen,
  Winlogon, alternate-admin, or cross-session claim.
