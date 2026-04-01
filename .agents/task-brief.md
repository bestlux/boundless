# Final Seam Cleanup

## Goal

Finish the current structural cleanup round by resolving four linked issues without changing external behavior:

- make `platform-windows` the sole owner of Windows capture/runtime primitives
- simplify daemon input state/routing boundaries
- build coherent control-plane snapshots from one daemon read path
- split the tray dashboard into focused modules with centralized task execution

## Scope

- `crates/platform-windows/src/input*`
- `crates/daemon/src/input*`
- `crates/daemon/src/state/*input*`
- `crates/daemon/src/control_plane_app.rs`
- `crates/tray/src/dashboard.rs` and new `crates/tray/src/dashboard/*`
- related tests that must move with the new seams

## Constraints

- Preserve current external behavior, DTOs, and proto/query shapes
- Keep `watch_ui` polling-based
- Do not revert unrelated user changes, including the existing `Cargo.lock` modification
- Treat these as shared integration files owned only by seam or integration workers:
  - `crates/daemon/src/input.rs`
  - `crates/daemon/src/control_plane_app.rs`
  - `crates/tray/src/dashboard.rs`
- Use one integration branch; workers may use isolated workspaces but not freestyle across ownership boundaries

## Done When

- `platform-windows` exposes an owned capture runtime and daemon-local stale hook code is gone
- daemon input control state and inject pipeline are decomposed into smaller modules with stable behavior
- status/UI/console snapshots are assembled from one coherent daemon snapshot bundle
- tray dashboard logic is split into shell, workflow, and layout modules with no raw thread spawning from feature modules
- workspace validation passes and any missing external smoke coverage is called out explicitly
