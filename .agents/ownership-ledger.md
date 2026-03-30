# Ownership Ledger

| Owner | Scope | Files / Surfaces | Status | Notes |
| --- | --- | --- | --- | --- |
| orchestrator | Integration, validation, commits, `.agents` | shared crate graph, validation, final branch synthesis | completed | kept shared seams single-owned |
| contracts-owner | Control-plane v2 | `crates/app-services`, `crates/adapter-ipc-grpc`, daemon app adapter | completed | unary daemon services removed |
| state-owner | daemon state decomposition | `crates/daemon/src/state*` | completed | facade remains, monolith file no longer exists |
| platform-owner | Windows extraction | `crates/platform-windows`, daemon input/clipboard integration points | completed | low-level helper/runtime ownership moved |
| transport-owner | transport runtime extraction | `crates/peer-transport`, daemon network integration points | completed | TLS/session orchestration intentionally remained daemon-owned |
| adapter-owner | CLI and tray thinning | `crates/cli`, `crates/tray` | completed | shared desktop helper layer now consumed by both shells |
| release-owner | release evidence and launch verdict | `.agents`, smoke/install validation | completed | installer bug and unrun trace/recovery gates remain explicit blockers |
