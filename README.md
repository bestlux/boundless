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

## Build and test

```bash
cargo fmt
cargo test
```

Local full validation (PowerShell):

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

On Windows, the daemon now defaults to a local named pipe control endpoint (`npipe://./pipe/boundlessd-api`) and the CLI default endpoint matches that. Use `--endpoint http://127.0.0.1:50051` to target loopback TCP explicitly.

Generate pairing code:

```bash
cargo run -p boundless-cli -- pair create-code --ttl 300
```

Export/import trust bundles:

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
cargo run -p boundless-cli -- transport send-file <peer_id> ./path/to/file.txt
cargo run -p boundless-cli -- transport events --limit 100
```

Manage input ownership control-plane:

```bash
cargo run -p boundless-cli -- input owner
cargo run -p boundless-cli -- input send-move <peer_id> 3 2
cargo run -p boundless-cli -- input claim <peer_id>
cargo run -p boundless-cli -- input release <peer_id>
```

## Release model

- Conventional Commits drive semver intent
- `release-please` prepares version/tag releases
- Tag pushes like `v1.2.3` trigger binary build + GitHub Release publishing
- If `release-please` cannot open PRs with `GITHUB_TOKEN`, either:
  - enable repository setting `Allow GitHub Actions to create and approve pull requests`, or
  - add a `RELEASE_PLEASE_TOKEN` secret (PAT with `contents` + `pull_requests` write access)

## Notes

Alpha scope emphasizes reliability primitives and now includes TLS transport with heartbeat/reconnect, trust-bundle pairing, real clipboard text runtime sync (watch/apply with echo suppression), queued file payload transfer primitives, input routing groundwork (ownership control-plane + synthetic input frame transport routed through a no-op sink), and mDNS runtime discovery with manual address fallback. Windows input injection and clipboard image sync remain upcoming slices.
