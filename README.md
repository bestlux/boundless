# boundless

Boundless is a Rust-first, performance-oriented alternative to Mouse Without Borders.

## Current status

This repository now contains an alpha-oriented workspace scaffold with:

- `boundlessd`: daemon process exposing local control APIs over gRPC
- `boundlessctl`: CLI for pairing, topology, features, hotkeys, diagnostics, and safe reset
- Shared core crates for protocol, security, transfer policy, input switching logic, discovery helpers, and clipboard policy
- Versioned local config + structured rotating logs + diagnostics dump baseline

## Workspace layout

- `crates/core-protocol`
- `crates/core-security`
- `crates/core-discovery`
- `crates/core-input`
- `crates/core-clipboard`
- `crates/core-transfer`
- `crates/ipc-api`
- `crates/daemon` (`boundlessd`)
- `crates/cli` (`boundlessctl`)
- `crates/tray` (`boundlesstray`, Windows)
- `docs/architecture` (v1 architecture maps and ownership boundaries)

## Build and test

```bash
cargo fmt
cargo test
```

Unified test suite (PowerShell):

```powershell
./scripts/dev/test-suite.ps1 -Profile smoke
```

Profiles:

```powershell
./scripts/dev/test-suite.ps1 -Profile quick   # fmt + test + clippy
./scripts/dev/test-suite.ps1 -Profile smoke   # quick + 2-node smoke
./scripts/dev/test-suite.ps1 -Profile full    # smoke + 3-node smoke
./scripts/dev/test-suite.ps1 -Profile trace -EndpointA http://127.0.0.1:50051 -EndpointB http://10.0.0.5:50051
./scripts/dev/test-suite.ps1 -Profile trace -TraceEnforceBudgets -TraceCaptureToApplyP95BudgetMs 45 -TraceCaptureToReceiveP95BudgetMs 20 -TraceCaptureToApplyJitterP95BudgetMs 18
./scripts/dev/test-suite.ps1 -Profile recovery -EndpointA http://127.0.0.1:50051 -EndpointB http://10.0.0.5:50051
```

`-Profile trace` now also exports matrix artifacts beside the trace log by default:
- `<trace>.matrix.csv`
- `<trace>.matrix.json`

Standalone matrix export for one or more trace logs:

```powershell
./scripts/dev/input-trace-matrix.ps1 -TraceDir ./artifacts/input-trace -Scenario edge_handoff -Topology topology_a
```

Automated pairing recovery matrix (reject + timeout + recovery success) with captures and diagnostics:

```powershell
./scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:50051 -EndpointB http://10.0.0.5:50051 -ResponderHost 10.0.0.5
```

If responder verification codes are hidden over remote API, the recovery script prompts once for the 6-digit success code shown on the responder tray. You can also pass `-RecoverySuccessCode <code>` / `-SuccessCode <code>` to avoid prompts.

Compatibility wrapper (legacy command still works):

```powershell
./scripts/dev/validate.ps1
```

## Run locally

Start daemon:

```bash
cargo run -p boundless-daemon
```

Query status:

```bash
cargo run -p boundless-cli -- daemon status
```

Interactive all-in-one terminal flow (auto-start daemon by default):

```bash
cargo run -p boundless-cli -- console
```

The `console` command shows daemon health, mDNS discovery status, discovered endpoints, trusted/connected peers, feature toggles, input owner/capture target, and pending pairing requests. It also provides quick commands for toggles and nearby pairing actions.

Inside console, use `pair request <index|machine_id>` to start guided nearby pairing for a discovered peer without manually typing host/port (pairing port is derived automatically from discovered transport endpoint).

First-run setup wizard (recommended for new installs):

```bash
cargo run -p boundless-cli -- setup
```

The setup wizard auto-checks daemon reachability, guides pairing (discovered peer or manual host fallback), and can apply initial left/right/up/down orientation for the newly paired peer.

Windows tray UI (minimal utilitarian control surface):

```bash
cargo run -p boundless-tray
```

`boundlesstray` provides:
- live discovered/paired/connected/pending visibility
- right-click pairing actions for discovered peers (guided request -> target shows code -> submit code)
- first-run setup dialog flow
- layout wizard dialog flow for left/right/up/down orientation

On Windows, the daemon now defaults to a local named pipe control endpoint (`npipe://./pipe/boundlessd-api`) and the CLI default endpoint matches that. Use `--endpoint http://127.0.0.1:50051` to target loopback TCP explicitly.

Nearby pairing (approval-based, no trust-bundle file copy):

```bash
cargo run -p boundless-cli -- pair create-code --ttl 120
cargo run -p boundless-cli -- pair nearby-join 123456 --host <target-host-or-ip> --port 15200
cargo run -p boundless-cli -- pair discover
cargo run -p boundless-cli -- pair request <index|machine_id|display-name>
cargo run -p boundless-cli -- pair request <index|machine_id|display-name> --request-id <request_id> --code 123456
cargo run -p boundless-cli -- pair pending
cargo run -p boundless-cli -- pair approve <request_id>
```

`pair request <selector>` starts a guided request-code flow and prints a `request_id`.
The target tray/CLI shows the generated 6-digit verification code.
Use `--request-id` and `--code` to submit and complete pairing.
`nearby-join` remains available and waits for remote approval before importing trust.  
The daemon nearby pairing listener defaults to `network_port + 100` (for example `15200` when transport network port is `15100`).

Export/import trust bundles (fallback or offline workflow):

```bash
cargo run -p boundless-cli -- pair export-trust --output node-a.json
cargo run -p boundless-cli -- pair import-trust --input node-b.json --alias node-b
```

Two-node smoke test (PowerShell):

```powershell
./scripts/dev/two-node-smoke.ps1
```

The smoke harness forces daemon control API transport to TCP for deterministic multi-node testing.

Queue transport payloads and inspect events:

```bash
cargo run -p boundless-cli -- transport send-text <peer_id> "hello"
cargo run -p boundless-cli -- transport send-image <peer_id> ./path/to/image.bmp
cargo run -p boundless-cli -- transport send-file <peer_id> ./path/to/file.txt
cargo run -p boundless-cli -- transport events --limit 100
```

Manage input ownership control-plane:

```bash
cargo run -p boundless-cli -- input owner
cargo run -p boundless-cli -- input capture-target
cargo run -p boundless-cli -- input capture-start <peer_id>
cargo run -p boundless-cli -- input capture-stop
cargo run -p boundless-cli -- input send-move <peer_id> 3 2
cargo run -p boundless-cli -- input send-key <peer_id> 30 down
cargo run -p boundless-cli -- input claim <peer_id>
cargo run -p boundless-cli -- input release <peer_id>
```

Configure hotkeys (examples):

```bash
cargo run -p boundless-cli -- hotkey toggle_easy_mouse Ctrl+Alt+Shift+E
cargo run -p boundless-cli -- hotkey reconnect Ctrl+Alt+Shift+R
cargo run -p boundless-cli -- hotkey lock_machine Ctrl+Alt+Shift+L
```

Configure topology-driven edge handoff (tokens can be `self`/`local`/`me`, machine id, device name, or connected peer display names / peer id tokens):

```bash
cargo run -p boundless-cli -- layout set "left,self,right"
cargo run -p boundless-cli -- layout preview
cargo run -p boundless-cli -- layout orient --left <peer> --right <peer>
cargo run -p boundless-cli -- layout wizard
```

## Release model

- Conventional Commits drive semver intent
- `release-please` prepares version/tag releases
- Tag pushes like `v1.2.3` trigger binary build + GitHub Release publishing
- If `release-please` cannot open PRs with `GITHUB_TOKEN`, either:
  - enable repository setting `Allow GitHub Actions to create and approve pull requests`, or
  - add a `RELEASE_PLEASE_TOKEN` secret (PAT with `contents` + `pull_requests` write access)

## Notes

Alpha scope emphasizes reliability primitives and now includes TLS transport with heartbeat/reconnect, trust-bundle pairing, real clipboard runtime sync for text and bitmap image payloads (watch/apply with echo suppression), queued file payload transfer primitives, and input routing groundwork (ownership control-plane + runtime capture target + synthetic input frame transport + runtime injection queue with pluggable backend). Windows runtime injection uses `SendInput`, and Windows capture now uses low-level keyboard/mouse hooks (with polling fallback) to drive outbound input frames, including wheel/hwheel events. Layout-driven edge handoff for capture target switching is now wired behind `easy_mouse`/`wrap_mouse` policy, and a Windows hotkey runtime executes configured `toggle_easy_mouse`, `reconnect`, and `lock_machine` actions on combo press edges. Broader cross-platform capture remains in progress. mDNS runtime discovery with manual address fallback is also in place.
