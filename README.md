# Boundless

Boundless is a Windows-first, Rust-based alternative to Mouse Without Borders. V5 focuses on reliable multi-machine input sharing, explicit pairing trust, validated layouts, clipboard/file workflows, and supportable diagnostics.

Boundless is still pre-release software. Windows is the primary product target; cross-platform runtime coverage is not yet parity-grade.

## Start Here

- Install and pair two machines: [V5 Quickstart](docs/user/quickstart.md)
- Arrange up to four peers: [Four-Machine Layouts](docs/user/four-machine-layouts.md)
- Clipboard and file behavior: [Clipboard And File Workflows](docs/user/clipboard-file-workflows.md)
- Service mode limits and recovery: [Service Mode](docs/user/service-mode.md)
- Troubleshoot runtime issues: [Troubleshooting](docs/user/troubleshooting.md)
- Migrate from v4 or Mouse Without Borders: [Migration Guide](docs/user/migration.md)
- Understand trust boundaries: [Security And Trust Model](docs/security-trust-model.md)
- Track parity and release blockers: [Mouse Without Borders Parity](docs/parity/mouse-without-borders.md)
- Current repo status for agents/release workers: [Project Status](docs/project-status.md)

## Developer Quick Start

```bash
cargo fmt
cargo test
```

Run the Windows desktop flow from source:

```bash
cargo run -p boundless-daemon
cargo run -p boundless-tray
```

## Project scope

- Primary target: Windows
- First-run UX: tray dashboard + local daemon
- Public status: pre-release, APIs and runtime behavior may still change
- License: MIT; see [LICENSE](LICENSE)

## Community

- Contributions: see [CONTRIBUTING.md](CONTRIBUTING.md)
- Bug reports and feature requests: use GitHub Issues
- Security reports: see [SECURITY.md](SECURITY.md)
- Support and triage guidance: see [SUPPORT.md](SUPPORT.md)

## Current status

This repository now contains an alpha-oriented workspace scaffold with:

- `boundlessd`: daemon process exposing local control APIs over gRPC
- `boundlessctl`: CLI for pairing, topology, features, hotkeys, diagnostics, and safe reset
- Shared core crates for protocol, security, transfer policy, input switching logic, discovery helpers, and clipboard policy
- Versioned local config + structured rotating logs + diagnostics dump baseline

V5 planning tracks Mouse Without Borders parity and beyond in [docs/v5-roadmap.md](docs/v5-roadmap.md). The release contract for parity status lives in [docs/parity/mouse-without-borders.md](docs/parity/mouse-without-borders.md).

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
./scripts/dev/test-suite.ps1 -Profile quick
```

Machine-readable area checks:

```powershell
./scripts/dev/check.ps1 -Area workspace -Format json
./scripts/dev/check.ps1 -Area docs/status -Format json
```

Profiles:

```powershell
./scripts/dev/test-suite.ps1 -Profile quick   # fmt + test + clippy
./scripts/dev/test-suite.ps1 -Profile smoke   # quick + opt-in 2-node smoke
./scripts/dev/test-suite.ps1 -Profile full    # smoke + opt-in 3-node smoke
./scripts/dev/test-suite.ps1 -Profile trace -EndpointA http://127.0.0.1:50051 -EndpointB http://192.0.2.10:50051
./scripts/dev/test-suite.ps1 -Profile trace -TraceEnforceBudgets -TraceCaptureToApplyP95BudgetMs 45 -TraceCaptureToReceiveP95BudgetMs 20 -TraceCaptureToApplyJitterP95BudgetMs 18
./scripts/dev/test-suite.ps1 -Profile recovery -EndpointA http://127.0.0.1:50051 -EndpointB http://192.0.2.10:50051
```

Default validation is unit-focused. Runtime smoke profiles are diagnostics for changes that touch daemon process orchestration, transport reconnect behavior, clipboard runtime delivery, input handoff, or multi-node topology. Release publishing still runs installer validation for MSI install/startup/uninstall behavior.

`-Profile trace` now also exports matrix artifacts beside the trace log by default:
- `<trace>.matrix.csv`
- `<trace>.matrix.json`

Standalone matrix export for one or more trace logs:

```powershell
./scripts/dev/input-trace-matrix.ps1 -TraceDir ./artifacts/input-trace -Scenario edge_handoff -Topology topology_a
```

Automated pairing recovery matrix (reject + timeout + recovery success) with captures and diagnostics:

```powershell
./scripts/dev/s4-recovery-automation.ps1 -EndpointA http://127.0.0.1:50051 -EndpointB http://192.0.2.10:50051 -ResponderHost 192.0.2.10
```

If responder verification codes are hidden over remote API, the recovery script prompts once for the 6-digit success code shown on the responder tray. You can also pass `-RecoverySuccessCode <code>` / `-SuccessCode <code>` to avoid prompts.
Follow-up modes:
- `-Mode success-only`
- `-Mode lockout-only`
- `-Mode success-and-lockout`

Compatibility wrapper (legacy command still works):

```powershell
./scripts/dev/validate.ps1
```

The wrapper defaults to `-Profile quick`. Use `-IncludeSmoke` for the 2-node smoke path or `-IncludeThreeNodeSmoke` for the full 3-node path.

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

CLI setup wizard (automation/debug fallback):

```bash
cargo run -p boundless-cli -- setup
```

The setup wizard auto-checks daemon reachability, guides pairing (discovered peer or manual host fallback), and can apply initial left/right/up/down orientation for the newly paired peer. The tray dashboard is the canonical first-run UX on Windows.

Windows tray dashboard UI (Windows):

```bash
cargo run -p boundless-tray
```

`boundlesstray` provides:
- tray icon + dashboard window (`Dashboard` / `Quit` menu)
- close-to-tray behavior on window `X`; use tray `Quit` for full exit
- `Status & Pairing` tab for discovered peers, manual connect, paired peers, and pending requests
- first-run `Get Started` guidance for fresh installs
- guided challenge-confirm pairing dialog (request code, submit code, retry affordances)
- `Layout Manager` tab for orientation (`left/right/up/down`) and apply action
- `Settings` tab for machine/runtime diagnostics and reconnect action
- API-first background refresh + optional daemon auto-start attempts (`--start-daemon`, default `true`)

On Windows, the daemon defaults to a local named pipe control endpoint (`npipe://./pipe/boundlessd-api`) and the tray/CLI default endpoint matches that.

If your local daemon config is TCP (`"api_transport": "tcp"`), launch tray/CLI with explicit TCP endpoint:

```bash
cargo run -p boundless-tray -- --endpoint http://127.0.0.1:50051
cargo run -p boundless-cli -- --endpoint http://127.0.0.1:50051 daemon status
```

You can inspect daemon config path with:

```bash
cargo run -p boundless-daemon -- print-config-path
```

Windows installer smoke/validation:

```powershell
./scripts/dev/installer-smoke.ps1 -KeepArtifacts
```

Build a local Windows installer:

```powershell
cargo build --release -p boundless-daemon -p boundless-cli -p boundless-tray
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\release\package-windows.ps1 `
  -Version 1.0.0-local `
  -DaemonPath .\target\release\boundlessd.exe `
  -CliPath .\target\release\boundlessctl.exe `
  -TrayPath .\target\release\boundlesstray.exe `
  -OutputPath .\artifacts\package-validation\Boundless-1.0.0-local-windows-x64.msi
```

Install locally from the packaged MSI:

```powershell
Start-Process msiexec.exe -Wait -ArgumentList @(
  '/i',
  (Resolve-Path .\artifacts\package-validation\Boundless-1.0.0-local-windows-x64.msi),
  '/qn',
  '/norestart'
)
```

The Windows release artifact is an MSI that installs:
- `boundlesstray.exe`
- `boundlessd.exe`
- `boundless-service.exe`
- `boundlessctl.exe`
- `Boundless.ico`
- `Boundless-Reset.ps1`
- `README.txt`
- `LICENSE.txt`
- `CHANGELOG.md`
- `package-manifest.json`

Release signing policy:
- Windows signing is optional during the current alpha phase.
- Set release environment variable `WINDOWS_SIGN_REQUIRED=true` and configure the signing secrets/vars to enforce signed stable releases.
- If signing material is configured while `WINDOWS_SIGN_REQUIRED` is unset, the workflow still signs available Windows artifacts without making signing a release gate.

Install behavior:
- default per-user install root: `%LocalAppData%\Programs\Boundless`
- Startup shortcut: `%AppData%\Microsoft\Windows\Start Menu\Programs\Startup\Boundless.lnk`
- Start Menu shortcut: `%AppData%\Microsoft\Windows\Start Menu\Programs\Boundless.lnk`
- Desktop shortcut: `%UserProfile%\Desktop\Boundless.lnk`
- installed shortcuts and ARP metadata use the packaged Boundless icon asset
- tray launch is the primary entrypoint and auto-starts `boundlessd` when needed
- first MSI releases intentionally block over legacy script-installed layouts; remove the old script-based install before running the MSI

Recovery helpers:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Boundless\Boundless-Reset.ps1" -NetworkOnly
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\Programs\Boundless\Boundless-Reset.ps1" -All
Start-Process msiexec.exe -Wait -ArgumentList @(
  '/x',
  (Resolve-Path .\artifacts\package-validation\Boundless-1.0.0-local-windows-x64.msi),
  '/qn',
  '/norestart'
)
```

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

Prefer guided challenge-confirmation pairing. Import trust bundles only from an authenticated out-of-band channel after verifying the peer identity and fingerprint. Never import trust bundles received from untrusted chat, email, download links, or issue comments.

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
- manifest-mode `release-please` prepares version bumps, tags, and draft GitHub Releases
- merges to `main` build the Linux tarball and Windows MSI from the tagged release commit
- release assets are validated, checksummed, attached to the draft release, and then published automatically
- If `release-please` cannot open PRs with `GITHUB_TOKEN`, either:
  - enable repository setting `Allow GitHub Actions to create and approve pull requests`, or
  - add a `RELEASE_PLEASE_TOKEN` secret (PAT with `contents` + `pull_requests` write access)

## Notes

Alpha scope emphasizes reliability primitives and now includes TLS transport with heartbeat/reconnect, trust-bundle pairing, real clipboard runtime sync for text and bitmap image payloads (watch/apply with echo suppression), queued file payload transfer primitives, and input routing groundwork (ownership control-plane + runtime capture target + synthetic input frame transport + runtime injection queue with pluggable backend). Windows runtime injection uses `SendInput`, and Windows capture now uses low-level keyboard/mouse hooks (with polling fallback) to drive outbound input frames, including wheel/hwheel events. Layout-driven edge handoff for capture target switching is now wired behind `easy_mouse`/`wrap_mouse` policy, and a Windows hotkey runtime executes configured `toggle_easy_mouse`, `reconnect`, and `lock_machine` actions on combo press edges. Broader cross-platform capture remains in progress. mDNS runtime discovery with manual address fallback is also in place.
