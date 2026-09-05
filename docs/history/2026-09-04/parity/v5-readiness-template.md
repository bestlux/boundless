# Superseded snapshot — 2026-09-04

This is preserved historical source from commit 9bad1b5c36e31aa81f8764e72fed66785f85c05a. It is not current implementation guidance, release policy, or evidence of later fixes. The original body follows unchanged; its relative links retain their original locations. Use the [original source](https://github.com/bestlux/boundless/blob/9bad1b5c36e31aa81f8764e72fed66785f85c05a/docs/parity/v5-readiness-template.md) to follow historical references, or the current docs index for the active contract.

---

# V5 Readiness Packet Template

The generated packet from `scripts/dev/release-readiness.ps1` is the canonical release readiness packet for v5. Release reviewers should treat this template as the required shape when reading or extending the generated JSON and Markdown.

## Required Sections

- Release metadata: version, branch, commit, generation time, environment, and release-manager signoff.
- Gate table: each unit, runtime, installer, and service gate marked `passed`, `failed`, or `skipped`.
- Evidence: log path or copied artifact path for every passed or failed gate.
- Skip rationale: reason and release impact for every skipped gate.
- Parity release blockers: snapshot of each `yes` row from `docs/parity/mouse-without-borders.md`.
- Installer evidence: copied `installer-smoke.json` with binary signature status for MSI, tray, daemon, service host, and CLI, plus matching service-host version output.

## Release Rule

`ready` means no failed and no skipped gates. `at-risk` means no failures but at least one skip. `blocked` means at least one failed gate.

Release automation must not publish from an `at-risk` or `blocked` packet unless a separate human release process explicitly changes the release policy and records that deferral outside the packet.
