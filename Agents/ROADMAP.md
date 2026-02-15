# Boundless Roadmap

## Vision
Deliver MWB-style cross-machine control in Rust with stronger reliability, diagnostics, and secure defaults.

## Milestones

1. Phase 0: Foundation (in progress)
- Rust workspace + crate boundaries
- Daemon/CLI skeleton and local API contract
- Config, logging, diagnostics baseline
- CI + release automation baseline

2. Phase 1: Secure pairing and machine connectivity
- mTLS trust bootstrap
- Pair/join handshake state machine
- Peer health + reconnect strategy
- mDNS discovery runtime with manual fallback integration (in progress)

3. Phase 2: Input switching core
- Keyboard/mouse capture + injection (in progress: input ownership control-plane + capture-target control API + Windows low-level hook capture backend with polling fallback + transport frame routing + runtime injection queue + Windows SendInput backend in place)
- Edge switching + layout matrix semantics
- Core hotkeys and easy mouse behavior

4. Phase 3: Clipboard and file parity
- Text/image clipboard sync
- File transfer with 100MB cap and conflict rename (in progress: chunked wire path + inbox persistence complete)

5. Phase 4: Hardening and alpha release
- Failure-mode tests and diagnostics coverage
- Packaging/signing path refinement
- First public alpha

6. Phase 5: Thin GUI
- Onboarding flow
- Topology + feature dashboard
