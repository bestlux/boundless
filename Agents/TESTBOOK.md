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
  - explicit per-peer connected/disconnected state assertions during restart
  - input ownership claim/release control-plane behavior
  - synthetic input frame transport + router processing path
  - reconnect behavior (drop + recover) with queued payload delivery after reconnect
  - queued clipboard text/file payload transfer and inbox materialization
- The smoke harness runs daemon API in TCP mode (`--api-transport tcp`) even though Windows default is named pipe, to keep two-node automation deterministic
- mDNS discovery is enabled during daemon runtime; transport workers prefer discovered endpoints and fall back to configured/manual peer addresses
- Runtime clipboard text sync is active in daemon; diagnostics `transport send-text` remains available as an explicit test hook
- Runtime clipboard image sync is active in daemon for BMP payloads; diagnostics `transport send-image <peer_id> <path.bmp>` is available as an explicit test hook
- Runtime input injection queue is active in daemon; on Windows, queued input events are applied via `SendInput`
- Runtime input capture target control-plane is active in daemon; on Windows, low-level keyboard/mouse hooks (including wheel/hwheel) enqueue outbound input frames for the selected target peer (polling fallback retained)
- Runtime input capture now supports layout-driven edge handoff (easy mouse + wrap mouse policy aware) when layout tokens resolve local + connected peer neighbors
- Runtime hotkey loop is active on Windows and executes configured actions on combo press edge (`toggle_easy_mouse`, `reconnect`, `lock_machine`; `switch_all` reserved)
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
- On Machine A: `input capture-target` (expect none)
- On Machine A: `input capture-start <peer_id-for-machine-b>`
- On Machine A: `input capture-target` (expect peer_id-for-machine-b)
- On Machine A: `input capture-stop`
- On Machine A: `input capture-target` (expect none)
- On Machine B: `input claim <peer_id-for-machine-a>`
- On Machine A: `input send-move <peer_id-for-machine-b> 3 2`
- On Machine A: `input send-key <peer_id-for-machine-b> 30 down`
- On both machines: `transport events --limit 100`
- Confirm outgoing/incoming `kind=input_frame` events are present, local runtime emits `kind=input_inject_*` events, and no daemon errors are reported

10. Reconnect and queued-delivery behavior
- On Machine A/B: establish connected state and capture peer IDs
- Stop daemon on Machine B
- On Machine A: verify `peer list` shows the known peer as `connected=false`
- On Machine A: enqueue `transport send-text <peer_id> "<queued-text>"`
- Restart daemon on Machine B
- Verify both sides return to `connected=true`
- On Machine B: verify incoming `kind=clipboard_text` event includes the queued text token after reconnect
- On Machine B: verify `input owner` is `none` after reconnect

11. Edge handoff behavior
- On Machine A: set layout using local token plus peer display names, for example `layout set "left,self,right"` (or machine id/device name token for local cell)
- On Machine A: set capture target to one neighbor (`input capture-start <left-peer-id>`)
- With `easy_mouse=on`, move cursor to the configured opposite edge and confirm capture target transitions to that edge neighbor (`input capture-target`)
- Toggle `feature set easy_mouse off`, repeat edge movement, and confirm capture target no longer changes via edge movement

12. Hotkey runtime behavior
- On Machine A: set `hotkey toggle_easy_mouse Ctrl+Alt+Shift+E` and verify value persists after daemon restart
- Press the configured combo once and verify `feature list` flips `easy_mouse` state exactly once per key press edge
- On Machine A with connected peers: press configured reconnect combo (default `Ctrl+Alt+Shift+R`) and verify peers transition to reconnect cycle (`connected=false` then back to `connected=true`)
- Validate lock-machine combo manually in a controlled session (it should invoke local workstation lock on Windows)

## Exit criteria for this stage

- CLI commands are stable and deterministic
- Persistence survives restart
- Diagnostics bundle generated successfully
- No daemon crashes during command workflows
- Reconnect cycle preserves transport health and drains queued payloads
- Clipboard/file payload smoke path completes reliably
