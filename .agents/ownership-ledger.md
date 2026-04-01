# Ownership Ledger

| Owner | Scope | Files / Surfaces | Status | Notes |
| --- | --- | --- | --- | --- |
| orchestrator | Branch orchestration, `.agents`, checkpoints, final synthesis | `.agents/*`, merge order, final reporting | active | does not author implementation code unless integration requires it |
| input-seam-owner | Capture runtime seam | `crates/daemon/src/input.rs`, `crates/platform-windows/src/input.rs`, `crates/platform-windows/src/input/hook_capture.rs`, `crates/daemon/src/input/windows_hook_backend.rs`, `crates/platform-windows/Cargo.toml` | completed | frozen seam is `platform_windows::input::CaptureRuntime`; daemon backend now depends on it instead of direct hook globals |
| tray-seam-owner | Dashboard shell seam | `crates/tray/src/dashboard.rs` | pending | extracts module boundaries and shared model/task runner only |
| platform-capture-owner | Windows capture runtime ownership | `crates/platform-windows/src/input*`, stale daemon hook file cleanup | pending | depends on `input-seam-owner` handoff |
| daemon-input-owner | Daemon input decomposition | `crates/daemon/src/state/*input*`, `crates/daemon/src/input/runtime.rs`, input-related helpers moved from other state modules | pending | depends on `input-seam-owner` handoff |
| snapshot-owner | Coherent control-plane snapshot bundle | `crates/daemon/src/control_plane_app.rs`, new daemon snapshot helpers | pending | depends on `daemon-input-owner` handoff |
| tray-workflow-owner | Dashboard workflow extraction | `crates/tray/src/dashboard/*` except layout module | pending | depends on `tray-seam-owner` handoff |
| tray-layout-owner | Dashboard layout extraction | layout-specific `crates/tray/src/dashboard/*`, layout tests | pending | depends on `tray-seam-owner` handoff |
| integration-owner | Shared-file stitch-up and dead-code cleanup | `crates/daemon/src/input.rs`, `crates/daemon/src/control_plane_app.rs`, `crates/tray/src/dashboard.rs`, shared exports | pending | accepts worker handoffs in merge order |
| qa-owner | Validation and smoke evidence | tests, scripts, validation notes | pending | returns failures to owning worker |
| reviewer | Independent review | integrated branch only | pending | no self-review, no delegation |
