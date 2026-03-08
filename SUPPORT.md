# Support

Boundless is maintained as a small public project. There is no guaranteed SLA, private support channel, or real-time help desk.

## Where to ask

- Bug reports: open a GitHub issue with the bug template
- Feature requests: open a GitHub issue with the feature template
- Security concerns: follow [SECURITY.md](SECURITY.md) and keep the report private

## Before opening an issue

Please check the current documentation first:

- [README.md](README.md)
- relevant scripts under `scripts/dev`

If you are reporting a bug, gather the smallest useful set of evidence:

```powershell
cargo test --workspace
./scripts/dev/test-suite.ps1 -Profile quick
```

Include these details when relevant:

- Windows version or other OS details
- whether you are running from `main`, a tagged release, or a local fork
- which component is affected (`boundlessd`, `boundlessctl`, `boundlesstray`, packaging, pairing, input routing, clipboard, transfer)
- exact commands you ran
- logs, diagnostics, screenshots, or reproduction steps

## What issue tracker support is for

GitHub issues work best for:

- reproducible bugs
- narrowly scoped feature requests
- documentation gaps tied to a concrete workflow

Issues may be closed when they are incomplete, out of scope, duplicates, or general support requests without actionable reproduction details.
