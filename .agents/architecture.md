# Architecture

## Delivered shape

- `boundlessd` is the runtime core.
- `app-services` owns shared control-plane contracts plus cross-adapter helper logic that used to be duplicated at the edges.
- `adapter-ipc-grpc` serves the live local control-plane surface.
- `daemon-host` owns host bootstrap/lifecycle seams.
- `platform-windows` owns extracted Windows clipboard and input helper/runtime code.
- `peer-transport` owns transport runtime state, queue/credit bookkeeping, wake/backoff helpers, and transport-local policy constants.
- `boundless-cli` and `boundless-tray` are thinner adapters that primarily handle prompts, presentation, watch wiring, and local process startup.

## Control-plane rules now in force

- Local surfaces talk to the local daemon through the v2 control plane.
- Only the daemon talks to remote peers.
- Nearby-pairing remote socket workflows no longer live in tray or CLI.
- Query/watch state is shared across adapters instead of being rebuilt independently at the edge.

## Runtime ownership state

- `AppState` still exists, but the old monolith was decomposed into focused state/ops modules.
- Queue/session/transport bookkeeping moved materially into `peer-transport`.
- Windows-specific backend logic moved materially into `platform-windows`.
- Remaining architectural debt is now concentrated in:
  - `AppState` still acting as a dominant facade
  - daemon-owned pairing wire logic
  - some adapter-local workflow shaping around prompts and view-model flows

## Remaining non-goals for this branch

- No transport rewrite beyond current TLS/TCP cleanup.
- No MCP adapter delivery.
- No attempt to preserve old local IPC compatibility.
