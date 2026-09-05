# Release Readiness Packet

The automatic release workflow is a **preview channel**. Future artifacts from that lane are published as GitHub prereleases with `--latest=false`, including plain numeric version tags. Its build/unit/installer evidence does not certify the physical Windows desktop. Existing release objects are not changed by this policy. Windows signing remains optional unless the configured signing policy requires it.

Stable promotion is a separate claim-scoped review of exact installed candidate artifacts, physical input/clipboard/recovery behavior, and unattended host safety. Passing this packet generator—even with `-Policy stable`—is necessary automated evidence where applicable, not a certificate for the whole product. See the [product acceptance scorecard](../performance/product-scorecard.md).

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

The product-performance interpretation layer lives in [docs/performance/product-scorecard.md](../performance/product-scorecard.md). Release-readiness gates answer whether required evidence is present and fresh enough for the selected policy; the scorecard answers what the measured product behavior means for dogfood, beta, and supported claims.

## Risk Classification

- `ready`: no gate failed and no gate was skipped.
- `at-risk`: no gate failed, but one or more gates were skipped.
- `blocked`: one or more gates failed.

Skipped gates are never hidden. A release reviewer must either provide the missing evidence, accept an explicit deferral, or keep the release blocked outside the script.

## Policy

- `-Policy prerelease` records skipped gates as `at-risk` evidence and exits non-zero only for failed gates unless `-RequireReady` is also supplied.
- `-Policy stable` is the release-blocking policy. It exits non-zero for failed, skipped, missing, or stale evidence that this packet can evaluate.
- `layout_topology_validation` is a unit result covered by the workspace suite. The former `four_node_topology` runtime label was removed: validating a layout matrix does not execute four PCs.
- `edge_handoff_latency` runs the trace profile with budget enforcement. It requires fresh samples for every measured stage and rejects missing samples or suspected clock skew instead of substituting a receiver-only metric.
- `paired_transport_contract` validates a saved report from the actual paired-test controller. It checks raw sample math, integrity counts, endpoint/process/session identity, and executable hashes. Stable policy additionally requires `-ExpectedDaemonSha256` from the selected candidate. This gate is transport evidence, not physical keyboard/mouse, clipboard, or hardware attestation.
- Installer-smoke summary freshness is checked from the evidence file timestamp by default. Evidence older than `-MaxEvidenceAgeHours` fails stable policy.
- Service update evidence is MSI-owned by default with `-ServiceUpdateMode msi-owned`. `service-self-update` and `tray-self-update` are accepted only as explicit unsupported/deferred modes and fail readiness when selected.
- Full-service installer coverage requires installer-smoke evidence for MSI-owned service registration, repair recovery, and uninstall cleanup. Missing repair or stale-service cleanup evidence blocks readiness.
- When the package manifest declares `executables.input_injector`, readiness requires the installed helper at `%ProgramFiles%\Boundless\<declared-file>`, matching PE product version, `requireAdministrator`, `uiAccess=false`, and zero helper processes after tray startup, repair, and uninstall.
- Input-injector signatures default to `-InputInjectorSignaturePolicy signed`, which accepts only `Valid`. A deliberately unsigned one-user build must opt in with `-InputInjectorSignaturePolicy unsigned-dogfood`; that exception accepts only `NotSigned` and is recorded in the packet.
- N-1 MSI upgrade coverage requires a prior MSI path passed to installer smoke. The summary must prove both app payload and service payload replacement, Program Files ownership of the current payloads, and the active service path after upgrade. Missing prior-MSI coverage is recorded as skipped evidence, which fails `-Policy stable`.

Physical two-machine performance labs are release evidence, not default PR gates. Keep them out of ordinary PR validation until the scenarios are stable, fast, non-disruptive, and supported by repeated real runs. Fixture packets may validate artifact shape, but real product scorecard thresholds stay provisional until at least two real two-PC runs exist for the scenario.

Metadata fixture packets can no longer be promoted with `perf-two-machine-evidence.ps1 -ReleaseEvidence`; that option fails explicitly. Use [functional measurements](../performance/two-machine-evidence-harness.md) for the actual paired controller, runtime/logging benchmarks, and evidence boundaries.

Release reviewers should classify readiness in three separate levels:

| level | minimum evidence posture |
| --- | --- |
| ready for dogfood | Current release-readiness packet plus scorecard evidence for the intended dogfood path, with no fail classifications for install/startup, discovery/pairing, clipboard text, or input handoff. Warnings and unmeasured categories must have written next actions or explicit release-scope deferrals. |
| ready for beta | Current release-readiness packet plus at least two real two-PC scorecard runs for every category, with no fail classifications and no privacy violations in evidence. Fixture-only rows are not enough. |
| parity claim supported | Current parity-matrix and release-readiness evidence for the exact claim, plus matching scorecard evidence. The scorecard alone is not claim evidence for desktop security boundaries, elevated contexts, prompts, self-update behavior, or broad third-party behavior. |

## Common Commands

Fast measurement-contract fixtures (no installed product or paired PCs):

```powershell
./scripts/dev/functional-validation-fixtures.ps1
./scripts/dev/perf-two-machine-fixtures.ps1
```

Packet using an already collected authenticated transport report:

```powershell
./scripts/dev/release-readiness.ps1 `
  -PairedTestReportPath artifacts/paired-test.json `
  -ExpectedDaemonSha256 <candidate-executable-sha256> `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json
```

The generator does not contact the remote PC to consume that saved report. A hash identifies the actual executable hosting each daemon (`boundless-service.exe` in service mode or `boundlessd.exe` in direct mode); do not substitute the CLI, MSI, or current checkout hash. Exploratory prerelease packets may omit the expected hash, and their transport validation explicitly records that the candidate is unbound. An optional `-ExpectedSourceRevision <full-commit-sha>` also requires matching build-time revisions, when present.

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

Runtime smoke starts same-host processes and is not a two-physical-PC acceptance run. The trace step needs live supported input samples; invoking it on idle or unreachable endpoints fails its measurement contract. Use the separate saved paired report for transport RTT, which measures on one monotonic clock.

Release-blocking packet:

```powershell
./scripts/dev/release-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json `
  -Policy stable
```

Explicit unsigned dogfood packet:

```powershell
./scripts/dev/release-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json `
  -InputInjectorSignaturePolicy unsigned-dogfood `
  -Policy stable
```

The unsigned-dogfood option is an auditable exception for the current private dogfood lane. It does not accept invalid, hash-mismatched, revoked, or unknown signature states, and it is not evidence for a signed distribution claim.

N-1 MSI upgrade evidence:

```powershell
./scripts/dev/installer-smoke.ps1 `
  -InstallerPath <current-msi> `
  -PreviousInstallerPath <prior-msi> `
  -KeepArtifacts

./scripts/dev/release-readiness.ps1 `
  -InstallerSmokeSummaryPath artifacts/installer-validation/installer-smoke.json `
  -Policy stable
```

`-RequireReady` remains available as a compatibility switch. `-Policy stable` is the preferred version-neutral stable-release gate and exits non-zero when any gate is skipped. Stable release readiness also fails when installer smoke does not include matching `boundless-service.exe --version` evidence.

The service-version gate parses `boundless-service.exe --version` strictly as `boundless-service <version>`. For a stable release such as `v5.0.0`, the parsed service version must exactly equal `5.0.0`; substring matches, prerelease suffixes, empty output, and malformed output fail. If no installer-smoke summary is supplied, `service_version_parity` is recorded as skipped and `-RequireReady` blocks the packet.

The `service_lifecycle_evidence` gate passes only when installer smoke summary evidence shows MSI-owned service registration, AutoStart LocalSystem config, a repair run that restores a deleted `BoundlessService` registration and daemon health, and uninstall cleanup that removes the service registration, Program Files install root, and Program Files service binary.

The `input_injector_evidence` gate is present whenever `packaging/windows/package-manifest.json` declares `executables.input_injector`. It requires all of these installer-smoke summary fields:

- `input_injector_path`: exact canonical Program Files path using the manifest file name.
- `input_injector_signature`: `Valid`, or exactly `NotSigned` when the packet explicitly selects `unsigned-dogfood`.
- `input_injector_product_version`: exact release version from the PE version resource.
- `input_injector_execution_level`: exactly `requireAdministrator`.
- `input_injector_ui_access`: exactly `false` (boolean or case-insensitive text).
- `input_injector_count_after_tray_launch`, `input_injector_count_after_repair`, and `input_injector_count_after_uninstall`: integer zero.

Missing or malformed fields fail closed. The process-count fields prove that install/startup/repair/uninstall do not launch or strand the elevated helper; they do not replace a focused, user-initiated elevated-input runtime smoke.

The `n_minus_1_msi_upgrade` gate passes only when installer smoke summary evidence includes `upgraded_from`, `previous_install_exit_code = 0`, and `upgrade_payload_replacement` booleans proving app payload replacement, service payload replacement, Program Files ownership, and active service use of the current Program Files service binary. The supported prior artifact source is a GitHub Release MSI asset named `Boundless-<version>-windows-x64.msi`; the release workflow also stages the current Windows MSI under the `boundless-windows-x64` artifact before publish.

## Local-Subnet Firewall Policy Evidence

BND-NEXT-21 is a human-gated policy decision. The current release path must continue to treat Windows Firewall mutation as not implemented unless a later approved implementation supplies matching evidence.

A future installer-owned local-subnet firewall rule can contribute to release readiness only when the evidence packet proves all of the following:

- The rule is created only through an explicit user-visible installer/helper option, not silently during pairing, diagnostics, reset, role reversal, daemon startup, or service startup.
- The rule is program-scoped to `%ProgramFiles%\Boundless\boundless-service.exe` and fail-closed when that Program Files service binary, service registration, intended user SID, or MSI ownership cannot be verified.
- The rule is scoped to Private profile plus local-subnet remote scope, or a narrower user-approved remote scope. Evidence must prove no Public-profile rule and no `remoteip=any profile=any` fallback are created.
- The default approved ports are TCP `15100` and TCP `15200`. TCP `15101` remains a side-by-side diagnostics probe and must not be opened unless a future alternate-port implementation explicitly asks for the selected transport port and derived pairing port.
- Repair recreates only the MSI-owned approved rule, upgrade preserves ownership for the current Program Files service path, and uninstall removes the MSI-owned Boundless rule without deleting unrelated user-created firewall rules.
- Static inspection and Windows installer lab evidence both show the expected program path, ports, profile, remote scope, and rollback behavior.
- Real two-PC Private-network evidence shows pairing and transport success without manual firewall edits, with diagnostics confirming the expected rule shape on both machines.

Until that evidence exists, release packets and parity docs may say Boundless has diagnostics and a proposed firewall policy only. They must not claim frictionless Mouse Without Borders-like install connectivity, automatic firewall setup, lock-screen behavior, secure desktop behavior, UAC prompt behavior, elevated-app behavior, or broad Mouse Without Borders parity.

Run the targeted fixture matrix with:

```powershell
./scripts/dev/release-readiness-fixtures.ps1
```

Installer user-selection helper evidence:

```powershell
./scripts/dev/installer-helper-fixtures.ps1
```

The helper fixture does not invoke UAC or install the MSI. It verifies that the
preferred helper accepts explicit SIDs, resolves an explicit account, rejects
malformed SIDs, captures the current user only from a non-elevated shell, and
fails closed from an already-elevated shell unless the current elevated account
is explicitly allowed.
