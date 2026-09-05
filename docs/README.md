# Boundless Documentation

Boundless is a pre-release Windows multi-PC control app. Start with the guide for the task you are doing; use the product matrix before relying on a feature or making a release claim.

## Use And Recover The App

| Need | Guide |
| --- | --- |
| Install and pair two PCs | [Quickstart](user/quickstart.md) |
| Arrange PCs | [Layouts](user/four-machine-layouts.md) |
| Share clipboard or send a file | [Clipboard and files](user/clipboard-file-workflows.md) |
| Understand service and elevation limits | [Service mode](user/service-mode.md) |
| Diagnose a failed connection or runtime problem | [Troubleshooting](user/troubleshooting.md) |
| Move from an older setup | [Migration](user/migration.md) |

## Understand Current Scope

- [Project status](project-status.md): release baseline, active work, and remaining evidence gaps.
- [Product capability matrix](parity/mouse-without-borders.md): implemented, unfinished, deferred, and unsupported behavior.
- [Windows product roadmap](v5-roadmap.md): intended outcomes and their acceptance bar.
- [Backlog](backlog.md): ordered implementation and verification work.
- [Changelog](../CHANGELOG.md): published engineering changes; it is not a readiness certificate.

## Implement And Verify Changes

- [Development and local validation](development.md): commands and meaningful evidence boundaries.
- [Component map](architecture/component-map.md): ownership across the Rust workspace.
- [Network architecture](architecture/network-v1.md): discovery, connection/session ownership, transport, and backpressure.
- [User-session input broker](architecture/user-session-input-broker.md): interactive input and service boundaries.
- [Installer architecture](architecture/single-elevated-installer.md): current constraints and intended simplification.
- [Trust model](security-trust-model.md): local/peer authority, sensitive data, and supported security boundaries.
- [Release readiness](release/release-readiness.md): automated gate and artifact rules.
- [Product scorecard](performance/product-scorecard.md) and [two-machine evidence](performance/two-machine-evidence-harness.md): distinguish source checks, actual transport measurements, and physical product evidence.
- [Paired testing](performance/paired-testing.md): temporary consent and one-controller transport checks.

## Historical Evidence

The [launch ledger](release/launch-ledger.md) records dated installed observations. The [history index](history/README.md) preserves superseded planning documents and milestone contracts. Historical results apply to their recorded builds and environments; they do not qualify a later candidate.
