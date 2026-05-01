# V5 Readiness Packet

`scripts/dev/v5-readiness.ps1` writes a release evidence packet under `artifacts/v5-readiness/` by default.

The packet contains:

- exact command logs under `logs/`,
- `v5-readiness.json` for automation,
- `v5-readiness.md` for release review,
- git branch and commit,
- a risk classification,
- a pointer back to `docs/parity/mouse-without-borders.md`,
- every passed, failed, or skipped gate with a reason and impact.

## Risk Classification

- `ready`: no gate failed and no gate was skipped.
- `at-risk`: no gate failed, but one or more gates were skipped.
- `blocked`: one or more gates failed.

Skipped gates are never hidden. A release reviewer must either provide the missing evidence, accept an explicit deferral, or keep the release blocked outside the script.

## Common Commands

Local unit and release metadata packet:

```powershell
./scripts/dev/v5-readiness.ps1
```

Packet from an already completed installer smoke:

```powershell
./scripts/dev/v5-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json
```

Runtime candidate packet:

```powershell
./scripts/dev/v5-readiness.ps1 `
  -IncludeRuntimeGates `
  -EndpointA http://127.0.0.1:50051 `
  -EndpointB http://127.0.0.1:50052
```

The runtime packet still records service smoke as skipped until a dedicated service validation exists.

Release-blocking packet:

```powershell
./scripts/dev/v5-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json `
  -RequireReady
```

`-RequireReady` exits non-zero when any gate is skipped. Release automation uses this mode so an `at-risk` packet is uploaded for inspection but does not publish.
