# Implementation Tracker

## Completed

- [x] Created Rust workspace and planned crate topology
- [x] Added protocol capability negotiation primitives
- [x] Added transfer policy module with 100MB cap and rename-on-conflict helper
- [x] Added security primitives (pairing code, local secret, trust-store bootstrap)
- [x] Added discovery helpers and manual target parsing
- [x] Added input-edge decision helper model
- [x] Added clipboard policy validation primitives
- [x] Added protobuf contract and generated gRPC API crate
- [x] Implemented `boundlessd` daemon skeleton with persisted state/config
- [x] Implemented `boundlessctl` command surface for alpha operations
- [x] Added structured logging and diagnostics dump/safe-reset handlers
- [x] Added baseline tests and passing workspace test suite
- [x] Implemented TLS identity bootstrap (local CA + device cert/key generation)
- [x] Added trust bundle export/import workflow through CLI and local API
- [x] Implemented initial inter-machine transport listener/session loop
- [x] Added heartbeat and reconnect worker scaffolding for peer links
- [x] Added two-node scripted smoke harness (`scripts/dev/two-node-smoke.ps1`)
- [x] Implemented queued clipboard-text and file payload wire frames over TLS sessions
- [x] Added transport diagnostics API/CLI for payload enqueue and event inspection
- [x] Extended two-node smoke harness to validate clipboard/file payload transfer
- [x] Added local validation helper script (`scripts/dev/validate.ps1`)
- [x] Added input control-plane primitives (owner claim/release/query) for deterministic routing prep
- [x] Expanded smoke harness to validate input owner claim/release flow
- [x] Wired core-input frame routing into transport/runtime with no-op sink backend
- [x] Added synthetic input frame enqueue API/CLI path for transport validation
- [x] Added Windows named-pipe control-plane adapter with TCP fallback + CLI `npipe://` endpoint support
- [x] Added mDNS discovery runtime with discovered-endpoint override + manual address fallback
- [x] Added real clipboard text runtime sync path (OS clipboard watcher + inbound apply + echo suppression)
- [x] Added clipboard image sync path (wire transport + runtime apply/watch + diagnostics CLI enqueue helper)
- [x] Added input injection runtime queue + backend abstraction and synthetic key diagnostics helper
- [x] Implemented Windows `SendInput` runtime injection backend for mouse/key events
- [x] Added runtime input capture target control-plane + Windows polling capture backend (cursor/buttons/keys)
- [x] Upgraded Windows capture to low-level keyboard/mouse hooks with wheel + broader key coverage (polling fallback retained)
- [x] Expanded two-node smoke harness with reconnect/disconnect assertions, queued-delivery checks, and stricter checked CLI helpers
- [x] Implemented layout-driven edge switching semantics for capture target handoff (easy mouse + wrap mouse policy aware)

## Next (priority order)

- [ ] Implement core hotkey runtime actions (lock machine, reconnect, easy mouse toggle) with persistence assertions
- [ ] Add explicit multi-machine edge-switch testbook flows on real displays
