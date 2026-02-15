# Two-Machine Alpha Testbook (Draft)

## Prerequisites

- Two Windows machines on same LAN
- Same build version of `boundlessd` and `boundlessctl`
- Firewall allows selected daemon ports

## Test cases

1. Daemon lifecycle
- Start daemon on both machines
- Verify `boundlessctl daemon status` returns running and machine IDs

2. Pairing flow
- On Machine A: create code (`pair create-code`)
- On Machine B: join using code and host (`pair join`)
- Verify peer appears in `peer list`

3. Topology edits
- Set matrix with `layout set`
- Verify matrix round-trips with `layout show`

4. Feature toggles
- Toggle clipboard/file/easy mouse features with `feature set`
- Verify updated values in `feature list`

5. Hotkey configuration
- Set each core hotkey command
- Verify persisted values after daemon restart

6. Diagnostics and recovery
- Run `diagnostics dump`
- Run `safe-reset --network`
- Confirm peers were cleared while core config remains

## Exit criteria for this stage

- CLI commands are stable and deterministic
- Persistence survives restart
- Diagnostics bundle generated successfully
- No daemon crashes during command workflows
