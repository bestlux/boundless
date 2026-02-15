# Two-Machine Alpha Testbook (Draft)

## Prerequisites

- Two Windows machines on same LAN
- Same build version of `boundlessd` and `boundlessctl`
- Firewall allows selected daemon ports

## Fast automation path

- Run `scripts/dev/two-node-smoke.ps1` to validate:
  - daemon startup on two isolated node configs
  - trust bundle export/import
  - TLS transport session establishment
  - heartbeat-driven connected state reporting
  - input ownership claim/release control-plane behavior
  - synthetic input frame transport + router processing path
  - queued clipboard text/file payload transfer and inbox materialization
- The smoke harness runs daemon API in TCP mode (`--api-transport tcp`) even though Windows default is named pipe, to keep two-node automation deterministic
- mDNS discovery is enabled during daemon runtime; transport workers prefer discovered endpoints and fall back to configured/manual peer addresses
- Runtime clipboard text sync is active in daemon; diagnostics `transport send-text` remains available as an explicit test hook
- Runtime clipboard image sync is active in daemon for BMP payloads; diagnostics `transport send-image <peer_id> <path.bmp>` is available as an explicit test hook
- Run `scripts/dev/validate.ps1` for fmt/test/clippy plus smoke in one command

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

7. Transport payload checks
- On Machine A: `transport send-text <peer_id> "sample text"`
- On Machine A: `transport send-image <peer_id> <path-to-bmp>`
- On Machine A: `transport send-file <peer_id> <path>`
- On both machines: `transport events --limit 100`
- Confirm outgoing/incoming events are present for text/image/file and file appears in receiver inbox root

8. Input owner arbitration
- On Machine A: `input owner` (expect none)
- On Machine A: `input claim <peer_id>`
- On Machine A: `input owner` (expect claimed peer)
- On Machine A: `input release <peer_id>`
- On Machine A: `input owner` (expect none)

9. Input frame transport path
- On Machine B: `input claim <peer_id-for-machine-a>`
- On Machine A: `input send-move <peer_id-for-machine-b> 3 2`
- On Machine A: `input send-key <peer_id-for-machine-b> 30 down`
- On both machines: `transport events --limit 100`
- Confirm outgoing/incoming `kind=input_frame` events are present, local runtime emits `kind=input_inject_*` events, and no daemon errors are reported

## Exit criteria for this stage

- CLI commands are stable and deterministic
- Persistence survives restart
- Diagnostics bundle generated successfully
- No daemon crashes during command workflows
- Clipboard/file payload smoke path completes reliably
