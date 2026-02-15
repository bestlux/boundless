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

## Next (priority order)

- [ ] Replace loopback gRPC transport with Windows named pipe transport adapter
- [ ] Implement true inter-machine transport and heartbeat/reconnect loop
- [ ] Implement TLS cert issuance + validation for peer sessions
- [ ] Implement mDNS discovery runtime with manual fallback integration
- [ ] Implement real clipboard/text-image sync pipeline
- [ ] Implement file transfer pipeline and chunking protocol
- [ ] Implement real input capture/injection pipeline
- [ ] Add multi-machine integration harness and scripted 2-machine test flow
