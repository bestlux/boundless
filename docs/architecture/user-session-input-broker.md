# User-Session Input Broker

Status: MVP for normal unlocked-desktop input in service mode. This is not a
BND-NEXT-9C claim: lock screen, secure desktop, UAC prompts, elevated apps, and
other users' sessions remain unsupported and unvalidated.

## Problem

The MSI-owned `BoundlessService` daemon runs as LocalSystem in session 0.
Windows session isolation means it can never observe or inject interactive
desktop input, so the service input runtime truthfully reports
`service_session_unsupported`. Real mouse/keyboard handoff needs a process in
the intended interactive user session.

## Decision

Host the broker in the existing tray process (`boundlesstray.exe`) instead of a
new helper binary:

- The tray already runs at sign-in in the intended user's interactive session
  as the allowed user, and already speaks the service control pipe.
- The service named pipe ACL (SYSTEM, Administrators, and exactly one allowed
  user SID) is the trust boundary to preserve; the broker rides it and adds no
  new socket, ACL surface, firewall rule, or transport.
- A separate helper would need installer, lifecycle, and packaging work without
  changing the security shape.

The LocalSystem service remains the trust, pairing, routing, layout, and
network authority. The broker is deliberately dumb: capture hands and inject
hands, no policy.

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
   alone; each `ExchangeInputBroker` drains up to 64 frames, re-checking
   `input_injection_allowed_for_peer` per frame.
3. The broker injects returned frames with `SendInput` in the user session and
   reports applied/failed counts on the next exchange.

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
- The broker adds a per-attachment token, and exchanges with a stale or
  replaced token are rejected.
- A broker that stops exchanging for ~3 seconds is treated as detached: backend
  mode reverts to `service_session_unsupported`, capture gates close, and
  pending inject frames fall back to the truthful unsupported-drop path.
- Keys/buttons the broker reported as held get synthetic release events on
  target change or broker loss so remote peers are not left with stuck input.

## Diagnostics

- `input_capture_backend_mode` is `user_session_broker` only while a fresh
  broker is attached; otherwise `service_session_unsupported`.
- Transport events record `input_broker_attached`, `input_broker_attach_rejected`,
  `input_broker_detached`, `input_broker_inject_dispatched`, and
  `input_broker_inject_report` (failures only).
- The tray Settings tab states the broker scope explicitly and never claims
  lock-screen/UAC/elevated-app control.

## Known Limits / Follow-Ups

- Poll-based exchange (8 ms active / 40 ms idle) adds up to one poll interval
  of latency per direction; a streaming exchange is a candidate follow-up if
  two-PC latency evidence warrants it.
- Abrupt broker death (kill -9 of the tray) can leave keys held on a remote
  peer until the release synthesis runs on the next capture-target transition.
- Real two-PC dogfood evidence is still required before the parity matrix rows
  can move; nothing here upgrades BND-NEXT-9C claims.
