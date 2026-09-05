# Development and local validation

Boundless is a Rust workspace. Windows owns the product runtime, named pipes, tray, input capture/injection, service, and MSI. Portable tests and Linux CI remain useful but cannot qualify those Windows behaviors.

## Build and check

Use Rust stable, the Windows MSVC build tools, and a normal PowerShell session. The toolchain setup lives in [CI setup](../.github/actions/setup-rust/action.yml). Keep `Cargo.lock` in sync with workspace manifests; CI and repeatable validation use `--locked`.

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
./scripts/dev/check.ps1 -Area docs/status -Format json
```

`./scripts/dev/test-suite.ps1 -Profile quick` is the combined local gate. While iterating, run the focused crate or test first, then the workspace gate before handoff. A test should exercise an observable outcome or failure boundary: for example, a disconnected peer cannot bypass its retry deadline, a blocked peer cannot stall another, or a replacement input broker cannot replay uncertain key-downs.

## Run a development instance

First check whether an installed instance already owns the control endpoint:

```powershell
Get-Service BoundlessService -ErrorAction SilentlyContinue
cargo run --locked -p boundless-cli -- daemon status
```

For an unpackaged per-user session without an installed service, run these in separate terminals:

```powershell
cargo run --locked -p boundless-daemon
cargo run --locked -p boundless-tray
```

The tray handles pairing and arrangement. `boundlessctl setup` and `boundlessctl console` are developer/automation fallbacks. Use `boundlessctl --help` and subcommand help for the current command surface instead of copying historical command lists.

The default control endpoint is `npipe://./pipe/boundlessd-api`. Isolated local test harnesses can configure loopback TCP endpoints and temporary data roots. They must not reuse installed configuration, trust, service registration, or the user's input broker.

## Choose meaningful evidence

| Question | Evidence |
| --- | --- |
| Does policy handle a failure correctly? | Focused deterministic behavioral test, including the rejected operation's lack of side effects. |
| Do real sessions recover and keep making progress? | Actual daemon/transport integration tests with controlled failures, bounded deadlines, and independently observed delivery. |
| Does the app behave on this Windows desktop? | Installed or interactive Windows evidence for the exact input, privilege, tray, or lifecycle scenario. |
| Are two physical PCs usable together? | Two-PC testing with the current binaries and a recorded network/display/session setup; transport probes and actual input/clipboard/file observations have separate results. |
| Is an operation fast or bounded? | Measured workload, sample count, clock domain, build identity, resource limits, and artifact. A generated fixture is not a benchmark result. |

The existing `smoke` and `full` profile names identify legacy multi-daemon harnesses. They are useful integration tools, but their names and node counts do not prove a physical desktop workflow. See [paired testing](performance/paired-testing.md), the [performance scorecard](performance/product-scorecard.md), and [release readiness](release/release-readiness.md).

## Installer and release work

Build packaged binaries with:

```powershell
cargo build --locked --release -p boundless-daemon -p boundless-cli -p boundless-tray -p boundless-input-injector
./scripts/release/assert-release-consistency.ps1
```

Check the packaging script's help for candidate paths and version arguments. On a disposable Windows machine, validate install, running-app upgrade, repair, and uninstall with `scripts/dev/installer-smoke.ps1`. This harness modifies services and installed files; it is not an ordinary source test.

Conventional Commits feed release-please. Automatic packaging produces development/prerelease artifacts; public promotion requires the separate readiness evidence. Signing remains policy-driven. Building an MSI, passing source tests, or producing an unsigned dogfood injector does not establish elevated-input or public-release readiness.
