# Support

Boundless is a small public project without a guaranteed response time. Use GitHub Issues for reproducible bugs, feature requests, and documentation problems. Report suspected vulnerabilities privately through [SECURITY.md](SECURITY.md).

## Report a problem

Start with [Troubleshooting](docs/user/troubleshooting.md). From PowerShell:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl --json daemon status
& $BoundlessCtl diagnostics dump --open-folder
```

If the daemon cannot be reached, use `diagnostics dump --offline --open-folder`. Offline exports cannot contain the daemon's live peer state. You do not need to build Boundless or run its developer test suite to report a bug.

Include the Windows version, Boundless version on each affected PC, installation/source-build mode, steps to reproduce, expected behavior, and what happened. For input problems, include display arrangement, DPI, and whether the affected app is elevated. For disk growth, include the log path, file size, timestamps, and a small relevant excerpt if available; avoid uploading an entire large log.

Diagnostic exports are redacted, but review any attachment before posting it publicly. Do not include private keys, trust stores, clipboard contents, pairing codes, or credentials. The [security model](docs/security-trust-model.md) describes the diagnostic boundary.

Release contributors should also attach the candidate's [readiness packet](docs/release/release-readiness.md), including missing or failed checks. Ordinary users do not need to produce one.
