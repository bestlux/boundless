# Contributing to Boundless

Thanks for considering a contribution.

Boundless is currently a single-maintainer, alpha-stage Rust workspace. Small, focused fixes are much easier to review and land than broad rewrites.

## Before you start

- Check existing issues and pull requests before starting work.
- Read the [development guide](docs/development.md) for build, test, packaging, and runtime commands.
- For security issues, use [SECURITY.md](SECURITY.md) instead of opening a public issue.

## Development workflow

Preferred local validation:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

Equivalent wrapper:

```powershell
./scripts/dev/test-suite.ps1 -Profile quick
```

Use the smallest validation set that matches your change:

- Docs-only changes usually do not need a full test run.
- Rust code changes should at least run `cargo fmt --all -- --check`, `cargo clippy`, and `cargo test`.
- Windows-specific runtime changes need the smallest matching functional Windows or trace test when feasible. A same-host multi-daemon test cannot prove a physical desktop workflow.
- Packaging, installer, or release workflow changes should run or explicitly defer the matching installer/release validation.

## Pull request expectations

- Keep changes scoped to one problem.
- Avoid unrelated refactors, formatting churn, or dependency updates unless they are required for the fix.
- Exercise the observable failure or user outcome, including side effects that must not occur. Prefer actual runtime components with controlled clocks, I/O, and peers over source-shape assertions or tests that mirror the implementation. Explain meaningful gaps; do not add a trivial test only to increase the count.
- Update docs when behavior, commands, or operator expectations change.
- Describe any Windows-specific behavior or manual verification that reviewers should know about.

## Commit and review guidance

- Conventional Commits are preferred. Releases are driven by `release-please`, so commit intent matters.
- Draft PRs are welcome for early feedback on larger changes.
- The maintainer may decline changes that significantly expand scope, add long-term maintenance burden, or conflict with the current architecture direction.

## Reporting bugs and proposing features

- Use the GitHub issue templates so reports include the details needed for triage.
- For bug reports, include reproduction steps, expected vs actual behavior, platform details, and relevant logs or diagnostics.
- For feature requests, describe the problem first, then the proposed solution and tradeoffs.

## Code of conduct

By participating in this project, you agree to follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
