# Daemon-First Breaking Refactor

## Goal

Restructure Boundless around a daemon-first architecture with thin tray and CLI adapters, a breaking local control plane built on shared contracts, and explicit crate boundaries for application services, daemon hosting, Windows platform code, transport, and IPC mapping.

## Scope

- Add new crates: `app-services`, `daemon-host`, `platform-windows`, `peer-transport`, `adapter-ipc-grpc`
- Move daemon boot/composition out of `crates/daemon/src/main.rs`
- Introduce a breaking v2 local control plane with shared command/query/event semantics and a streaming watch feed
- Repoint CLI and tray toward the new control plane
- Refresh `.agents/` so it becomes the canonical planning and handoff workspace for this refactor

## Constraints

- Windows-first behavior must remain the primary supported path
- Do not revert unrelated user changes, including the existing `Cargo.lock` modification
- Keep worker ownership disjoint where possible; shared integration points must be owned by one worker at a time
- Preserve existing behavior where practical, but local IPC compatibility is not required
- A reset boundary for pre-refactor alpha installs is acceptable

## Done When

- Workspace contains the new boundary crates and they are wired into the build
- Daemon boot path runs through a host seam instead of inline orchestration
- A v2 control plane exists with unary command/query coverage plus a server-streamed watch API
- Tray and CLI consume the new control plane instead of duplicating workflow logic
- `.agents/` contains current spec, architecture, execution plan, test plan, review notes, launch notes, and ownership ledger
