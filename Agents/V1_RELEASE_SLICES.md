# V1 Release Slices

This file is the ordered execution plan for shipping `v1.0.0`.

## Ordered Slices

1. [x] 3-node automation harness
- Add deterministic local 3-node smoke flow to validate real `switch_all` rotation order and multi-peer handoff behavior.

2. [ ] Real multi-display validation pass
- Run testbook flows on real two-machine, multi-monitor setups and close layout/edge/DPI behavior gaps.
- Use `Agents/MULTI_DISPLAY_VALIDATION_RUNBOOK.md` and `Agents/MULTI_DISPLAY_VALIDATION_RESULTS_TEMPLATE.md`.

3. [ ] Input reliability hardening
- Stress reconnect + high event-rate paths and tighten queue/backpressure behavior to prevent stuck input or lag spikes.

4. [ ] Protocol compatibility hardening
- Lock capability/version gates for newer frame types and ensure mixed-version peers degrade safely.

5. [ ] Discovery and connect UX simplification
- Reduce first-run friction around discovery/trust/connect defaults and improve onboarding reliability.

6. [ ] Security lifecycle completeness
- Complete trust lifecycle operations (revoke/remove/refresh) and improve security diagnostics clarity.

7. [ ] Service and launch ergonomics
- Harden daemon run modes (startup/service behavior, clean shutdown, restart semantics).

8. [ ] Packaging and install
- Produce installable artifacts with sane defaults and config/data migration behavior.

9. [ ] Release pipeline finalization
- Finalize semver/tag-driven build and release automation, including changelog and notes flow.

10. [ ] V1 docs and operator guide
- Publish setup, pairing, topology, hotkeys, troubleshooting, and recovery documentation.

11. [ ] V1 stabilization window
- Run regression matrix and bug-bash with bugfix-only merges.

12. [ ] Cut `v1.0.0`
- Tag, publish artifacts, publish release notes, and execute post-release validation.

## Active Slice

- Current: `2) Real multi-display validation pass`
