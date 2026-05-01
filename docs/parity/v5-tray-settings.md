# V5 Tray Settings Contract

Boundless v5 should make the important Mouse Without Borders parity controls available from the tray without pretending every control is fully runtime-enforced.

## Implemented In This Milestone

- `UiSnapshot` now carries effective feature flags, hotkey bindings, input handoff config, and input runtime state.
- The tray Settings tab can update:
  - share input,
  - share clipboard,
- transfer file feature visibility; receive policy is configurable, but the transfer enable flag is not enforced yet,
  - easy mouse,
  - wrap mouse,
  - receive folder,
  - receive-folder organization,
  - trusted-peer file auto-accept,
  - anti-idle behavior,
  - input corner blocking and corner threshold,
  - relative mouse,
  - hide cursor at edge,
  - draw cursor marker,
  - the shipped hotkey actions.
- The tray exposes safe reset and network reset actions through the shared control plane.
- Same-subnet-only and validate-remote-IP controls are visible but disabled with explicit unsupported reasons.
- Service-mode guidance remains visible until Windows IPC ACL validation is complete.

## Honest Limitations

- Disabled same-subnet and remote-IP controls are not enforcement. V5-8/V5-9 must add daemon policy and validation before those rows can be marked validated.
- The tray shows service-mode guidance, but install/start/stop/uninstall actions remain CLI/installer owned until the service privilege boundary is validated.
- Cursor hiding, cursor marker, relative movement, clipboard-file, and drag/drop claims still depend on their feature-specific runtime validation.
- The tray hotkey editor persists daemon hotkey strings; it does not capture key chords interactively yet.
- Reset actions use a two-click tray confirmation before invoking the control plane. Future UX can replace this with typed confirmations if release testing shows the two-click guard is too weak for broader factory-reset flows.

## Validation Targets

Before V5 can mark the tray settings workstream validated, the readiness packet must include:

- tray unit or interaction coverage for settings snapshot hydration,
- IPC compatibility evidence for the new snapshot fields,
- restart persistence evidence for every setting changed through the tray,
- unsupported-state evidence for same-subnet, remote-IP, and service controls,
- and a manual Windows tray smoke showing no clipped settings text at the supported window size.
