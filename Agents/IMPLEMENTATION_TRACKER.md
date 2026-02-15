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

## Next (priority order)

- [ ] Replace loopback gRPC transport with Windows named pipe transport adapter
- [ ] Implement mDNS discovery runtime with manual fallback integration
- [ ] Implement real clipboard/text-image sync pipeline
- [ ] Implement real input capture/injection pipeline
- [ ] Wire core-input frame routing into transport/runtime with no-op sink backend
- [ ] Add richer multi-machine integration assertions beyond payload smoke
