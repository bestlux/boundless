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

## Run locally

Start daemon:

```bash
cargo run -p boundless-daemon
```

Query status:

```bash
cargo run -p boundless-cli -- daemon status
```

Generate pairing code:

```bash
cargo run -p boundless-cli -- pair create-code --ttl 300
```

## Release model

- Conventional Commits drive semver intent
- `release-please` prepares version/tag releases
- Tag pushes like `v1.2.3` trigger binary build + GitHub Release publishing

## Notes

Alpha scope currently emphasizes architecture and reliability primitives. Windows input injection, TLS transport for inter-machine traffic, and mDNS runtime discovery are planned next implementation slices.
