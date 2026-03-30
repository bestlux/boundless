# Spec

## Goal

Turn Boundless into a daemon-first product where `boundlessd` owns runtime workflow, remote peer interaction, lifecycle, and diagnostics, while tray and CLI become thin adapters over one local control plane.

## Required outcomes

- Keep the Rust workspace and add boundary crates now: `app-services`, `daemon-host`, `platform-windows`, `peer-transport`, `adapter-ipc-grpc`.
- Replace the unary-only local IPC shape with a breaking v2 control plane built around shared command, query, and event contracts.
- Expose daemon-owned query and watch surfaces for status, peers, topology, diagnostics, nearby pairing, reconnect state, and transfer progress.
- Move daemon startup and shutdown policy behind a host seam so `main.rs` only parses args, builds the host, and runs it.
- Keep tray as the default Windows UX for now, but make the daemon fully usable headlessly and administrable without the tray.
- Make `.agents/` the single source of truth for refactor scope, architecture, execution order, validation, and launch guidance.

## Explicit constraints

- This is one breaking alpha branch, not a compatibility migration on `main`.
- Tray, CLI, and future MCP must talk only to the local daemon; only the daemon talks to remote peers.
- Runtime state must move toward subsystem ownership; a temporary facade is acceptable during migration, but the end state is not a new god object.
- Strict control vs realtime-input vs bulk-transfer separation is mandatory even if dedicated peer sessions ship later.

## Non-goals in this program phase

- Full MCP adapter delivery
- QUIC or transport-stack replacement
- Cross-platform UI expansion
- Perfect backward compatibility for pre-refactor local IPC or alpha state
