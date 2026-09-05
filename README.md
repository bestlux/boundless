# Boundless

Share a mouse and keyboard between Windows PCs. Boundless is a Rust desktop app with explicit peer pairing, screen-edge switching, clipboard sharing, file transfer, and local diagnostics.

**Development preview.** The current work focuses on safe unattended operation and a simpler two-PC experience. A versioned release or a passing automated test suite does not establish public product readiness. See the [current status](docs/project-status.md) and [Windows roadmap](docs/v5-roadmap.md).

## Use Boundless

For Windows installation, download the release's `Boundless-<version>-windows-x64.zip`, extract it, and double-click **Install.cmd**. Use the same preview build on both PCs.

- [Install, pair, and arrange two PCs](docs/user/quickstart.md)
- [Clipboard and files](docs/user/clipboard-file-workflows.md)
- [Windows service and elevated input](docs/user/service-mode.md)
- [Troubleshooting and diagnostic exports](docs/user/troubleshooting.md)
- [Upgrade and migration](docs/user/migration.md)

The tray dashboard is the main interface. The CLI provides the same local control surface for diagnostics and automation. Windows-to-Windows is the product target; Linux builds exercise portable code and do not imply desktop feature parity.

## Develop

Use Rust stable and the Windows MSVC build tools, matching [CI setup](.github/actions/setup-rust/action.yml). From a normal PowerShell session:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

The repository wrapper is `./scripts/dev/test-suite.ps1 -Profile quick`. Runtime tests need additional setup; see the [development guide](docs/development.md). Installer tests change Windows services and installed files and belong on a disposable Windows test machine.

To query an existing local daemon:

```powershell
cargo run --locked -p boundless-cli -- --json daemon status
```

For a source-only session, start `cargo run --locked -p boundless-daemon` and `cargo run --locked -p boundless-tray` in separate terminals. Do this only when an installed Boundless service is not already managing the endpoint. Windows defaults to `npipe://./pipe/boundlessd-api`.

## Understand and contribute

The [documentation index](docs/README.md) separates user guides, architecture, validation, and historical decisions. Start with the [component map](docs/architecture/component-map.md), [transport ownership](docs/architecture/network-v1.md), and [security model](docs/security-trust-model.md) before changing runtime behavior.

Keep changes focused on user outcomes and test failure behavior at the boundary being changed. See [CONTRIBUTING.md](CONTRIBUTING.md). Report bugs through GitHub Issues and suspected vulnerabilities through [SECURITY.md](SECURITY.md). Support expectations are in [SUPPORT.md](SUPPORT.md).

Boundless is MIT licensed; see [LICENSE](LICENSE).
