# Security Policy

Boundless is alpha software. Please report suspected vulnerabilities responsibly.

For the product trust boundaries, see [docs/security-trust-model.md](docs/security-trust-model.md).

## Supported versions

Security fixes are best-effort and are most likely to land on:

| Version | Supported |
| --- | --- |
| latest release | yes |
| `main` | yes |
| older releases | no |
| untagged forks | no |

## How to report a vulnerability

Do not open a public GitHub issue for a suspected vulnerability.

Use one of these private routes:

1. GitHub Private Vulnerability Reporting for the canonical repository, if it is enabled.
2. If that option is unavailable, contact the maintainer privately through the contact route on [github.com/bestlux](https://github.com/bestlux).

Please include:

- affected version, commit, or branch
- component or crate involved
- clear reproduction steps or proof of concept
- expected impact and attacker assumptions
- any logs, traces, or screenshots that help confirm the issue

## What to expect

- Initial triage target: within 5 business days
- Follow-up updates: best effort, depending on severity and reproduction quality
- Fixes may land on `main` before a release is cut

Please avoid public disclosure until the maintainer has had a reasonable chance to investigate and ship a fix.

## Non-security bugs

If the report does not create a confidentiality, integrity, authentication, authorization, or remote-execution risk, use the regular issue templates instead.
