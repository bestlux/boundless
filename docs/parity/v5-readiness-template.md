# V5 Readiness Packet Template

The generated packet from `scripts/dev/v5-readiness.ps1` is the canonical v5 readiness packet. Release reviewers should treat this template as the required shape when reading or extending the generated JSON and Markdown.

## Required Sections

- Release metadata: version, branch, commit, generation time, environment, and release-manager signoff.
- Gate table: each unit, runtime, installer, and service gate marked `passed`, `failed`, or `skipped`.
- Evidence: log path or copied artifact path for every passed or failed gate.
- Skip rationale: reason and release impact for every skipped gate.
- Parity release blockers: snapshot of each `yes` row from `docs/parity/mouse-without-borders.md`.
- Installer evidence: copied `installer-smoke.json` with binary signature status for MSI, tray, daemon, service host, and CLI.

## Release Rule

`ready` means no failed and no skipped gates. `at-risk` means no failures but at least one skip. `blocked` means at least one failed gate.

Release automation must not publish from an `at-risk` or `blocked` packet unless a separate human release process explicitly changes the release policy and records that deferral outside the packet.
