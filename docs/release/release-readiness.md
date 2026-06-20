# Release Readiness Packet

`scripts/dev/release-readiness.ps1` writes a release evidence packet under `artifacts/release-readiness/` by default.

The packet contains:

- exact command logs under `logs/`,
- `release-readiness.json` for automation,
- `release-readiness.md` for release review,
- git branch and commit,
- release policy,
- a risk classification,
- a pointer back to `docs/parity/mouse-without-borders.md`,
- every passed, failed, or skipped gate with a reason and impact.

## Risk Classification

- `ready`: no gate failed and no gate was skipped.
- `at-risk`: no gate failed, but one or more gates were skipped.
- `blocked`: one or more gates failed.

Skipped gates are never hidden. A release reviewer must either provide the missing evidence, accept an explicit deferral, or keep the release blocked outside the script.

## Policy

- `-Policy prerelease` records skipped gates as `at-risk` evidence and exits non-zero only for failed gates unless `-RequireReady` is also supplied.
- `-Policy stable` is the release-blocking policy. It exits non-zero for failed, skipped, missing, or stale evidence that this packet can evaluate.
- Installer-smoke summary freshness is checked from the evidence file timestamp by default. Evidence older than `-MaxEvidenceAgeHours` fails stable policy.
- Full N-1 interactive installer upgrade and service-upgrade coverage still require a prior MSI path, service smoke, and Windows lab/runtime evidence; missing coverage must remain visible as skipped evidence or a documented release-review deferral.

## Common Commands

Local unit and release metadata packet:

```powershell
./scripts/dev/release-readiness.ps1
```

Packet from an already completed installer smoke:

```powershell
./scripts/dev/release-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json
```

Runtime candidate packet:

```powershell
./scripts/dev/release-readiness.ps1 `
  -IncludeRuntimeGates `
  -EndpointA http://127.0.0.1:50051 `
  -EndpointB http://127.0.0.1:50052
```

The runtime packet still records service smoke as skipped unless `-IncludeServiceSmoke` is supplied from an elevated Windows shell.

Release-blocking packet:

```powershell
./scripts/dev/release-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json `
  -Policy stable
```

`-RequireReady` remains available as a compatibility switch. `-Policy stable` is the preferred version-neutral stable-release gate and exits non-zero when any gate is skipped. Stable release readiness also fails when installer smoke does not include matching `boundless-service.exe --version` evidence.

The service-version gate parses `boundless-service.exe --version` strictly as `boundless-service <version>`. For a stable release such as `v5.0.0`, the parsed service version must exactly equal `5.0.0`; substring matches, prerelease suffixes, empty output, and malformed output fail. If no installer-smoke summary is supplied, `service_version_parity` is recorded as skipped and `-RequireReady` blocks the packet.

Run the targeted fixture matrix with:

```powershell
./scripts/dev/release-readiness-fixtures.ps1
```
