# Component map

Use this map to choose an owner before changing behavior. Keep component IDs stable; the detailed runtime contracts live beside it.

| Component ID | Owner | Durable state | Live state and responsibility | Verification boundary |
| --- | --- | --- | --- | --- |
| `core_policy_crates` | `crates/core-*` | Protocol/security formats and policy definitions | Validation and pure clipboard/input/transfer policy; no task ownership | Malformed input, limits, trust, and policy outcomes |
| `daemon_config_identity` | `crates/daemon/src/config.rs`, `state.rs`, `core-security` | Identity, trust, preferences, peer addresses and names | Runtime observations are reconstructed; saved configuration is not connection health | Migration, round-trip preferences, no restored live connection |
| `daemon_runtime_tasks` | `crates/daemon/src/host.rs`, `runtime_tasks.rs` | None | Start, supervise, report health, cancel and join runtime work | Shutdown, failed startup, child cleanup and bounded recovery |
| `peer_transport_network` | `crates/peer-transport`, `crates/daemon/src/network` | None | Per-peer ownership/retry deadlines, authenticated sessions, bounded queues, continuously serviced reads | Actual sessions under refusal, wake storms, stalled writes, early close, malformed input and cancellation |
| `clipboard_file_runtime` | `crates/daemon/src/clipboard.rs`, `state/clipboard_*.rs`, transfer operations | Receive policy and completed received files | Clipboard replay/dedupe, transfer cursors, temporary files and current progress; user-session file authority | Real payload delivery and rejected operations without filesystem side effects |
| `input_runtime_windows` | `crates/daemon/src/input.rs`, `state/input_*.rs`, `crates/platform-windows/src/input*`, tray broker | Input preferences, arrangement and hotkeys | Capture/injection, owner/epoch, pending events, exact delivery receipts, held-input recovery | Partial native send, lost receipt, process replacement, stale epoch, emergency release; physical desktop evidence separately |
| `pairing_discovery_trust` | Daemon pairing/discovery modules, `core-security` | Trust records and local identity | Discovery hints, expiring verification requests, rate limits and recovery | Explicit approval, expired/rejected requests, stale trust and network recovery |
| `control_plane_ipc` | `crates/ipc-api`, `crates/adapter-ipc-grpc`, `crates/control-plane-client`, `crates/app-services` | Protobuf source/API contracts | Verified caller identity, DTOs, local commands and snapshots | Authorization, actual API outcomes, serialization and client behavior |
| `cli_automation` | `crates/cli` | No primary application state | Scriptable local controls, structured results, diagnostics and explicit administration | CLI parse/error/output contracts and daemon-backed operations |
| `tray_dashboard` | `crates/tray/src/dashboard*` | Preferences are saved through daemon APIs | Home, pairing, arrangement, files, sharing, Support; local drafts and truthful snapshot freshness | Functional UI actions with a recording task sink, keyboard interaction, rendered layouts; native Windows accessibility separately |
| `installer_packaging_release` | `packaging/windows`, `scripts/release`, release workflow | MSI payload, package/version manifests and release assets | Machine-wide install, selected-user contract, service lifecycle and optional signing | Candidate metadata/hash identity; installed upgrade, repair, restart and uninstall on disposable Windows |
| `diagnostics_support` | Daemon diagnostics operations, CLI and tray Support, daemon logging | User-requested redacted reports and bounded disk logs | Health projections, bounded event histories, producer queue and rotation | Sensitive-value redaction, denied export path, log byte/queue bounds, restart and storage failure |
| `validation_scripts` | `scripts/dev`, CI workflows | Evidence artifacts under `artifacts/` | Run bounded tests and measurements; classify what was actually exercised | Functional parser/rejection fixtures plus actual workloads; fixtures never qualify physical behavior |

## Boundaries that should stay separate

- **Configuration versus observation:** pairing and preferences survive restart; connected state, active sessions, input ownership, outstanding probes and permission leases do not.
- **One peer versus another:** a blocked peer's I/O cannot hold global ownership or prevent another peer's input. See [network ownership](network-v1.md).
- **Service authority versus user data:** the service runs as LocalSystem, but user-selected paths require the selected user's unelevated filesystem authority. This is not yet whole-process isolation. See [security and trust](../security-trust-model.md).
- **Network ownership versus applied input:** delivery acknowledgment and a Windows key/button being held are different facts. Broker replacement must recover conservatively. See [input broker lifetime](user-session-input-broker.md).
- **Product action versus presentation:** keep shared workflows in daemon/app-services/IPC helpers. The tray owns visible choices, local draft fields and immediate local input release; it should not duplicate transport policy.
- **Diagnostic access versus remote control:** [paired testing](../performance/paired-testing.md) uses a temporary peer-specific permission and bounded transport probes. It grants no remote shell, arbitrary file path, clipboard injection, or physical-input test authority.

The [roadmap](../v5-roadmap.md) owns future boundaries. The [project status](../project-status.md) owns what has been validated. A crate name or architecture diagram alone is not evidence that a feature is ready for users.
