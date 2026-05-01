# Mouse Without Borders Parity Matrix

This matrix is the v5 release contract for matching and exceeding Microsoft PowerToys Mouse Without Borders. The source parity target is the current Microsoft Learn page for [PowerToys Mouse Without Borders](https://learn.microsoft.com/en-us/windows/powertoys/mouse-without-borders).

The matrix is intentionally stricter than a feature inventory. A row is not `validated` until the user-facing tray path, CLI/diagnostic fallback, and release validation evidence all exist where that row requires them.

Topology-specific layout constraints live in [v5-topology.md](v5-topology.md). Service-mode constraints live in [v5-service-mode.md](v5-service-mode.md). Input handoff constraints live in [v5-input-handoff.md](v5-input-handoff.md). Clipboard and file workflow constraints live in [v5-clipboard-file-workflows.md](v5-clipboard-file-workflows.md). Pairing and trust constraints live in [v5-pairing-network-safety.md](v5-pairing-network-safety.md). Tray settings constraints live in [v5-tray-settings.md](v5-tray-settings.md).

## Status Values

- `not-started`: No meaningful Boundless implementation exists.
- `plumbing`: Protocol, config, state, or tests exist, but the feature is not user-ready.
- `cli-ready`: CLI or script workflow exists and is usable by a power user.
- `tray-ready`: Tray workflow exists and matches the user-facing promise.
- `validated`: Release-blocking validation proves the required behavior.
- `deferred`: Explicitly postponed beyond v5.
- `out-of-scope`: Intentionally excluded from Boundless.

## Release Blocking Rule

Rows marked `yes` in the release blocker column must be `validated`, `deferred`, or `out-of-scope` with a concrete rationale before v5 can ship. Required rows cannot remain `not-started`, `plumbing`, `cli-ready`, or `tray-ready` in the v5 readiness packet.

## Matrix

| Feature or Setting | Mouse Without Borders Behavior | Boundless v5 Target | Current Boundless Status | Owner Surface | CLI Surface | Tray Surface | Validation Evidence | Release Blocker |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Control up to four computers | Control up to four computers from one machine. | Validate two-, three-, and four-machine Windows topologies with predictable layout and reconnect behavior; the stored layout validator allows This PC plus up to four peers, but five-device runtime support must not be claimed without separate release evidence. | tray-ready | `daemon` layout/input state, `core-input`, `tray` layout UI | `layout show`, `layout set`, `layout orient`, `input capture-start` | Layout Manager | `cargo test -p app-services desktop::tests`; `cargo test -p boundless-daemon state::tests::layout_and_validation`; four-node runtime validation still required | yes |
| Shared keyboard and mouse | Use one keyboard/mouse across connected machines. | Low-latency edge handoff, explicit capture target, release/unlock path, diagnostics for hook/polling mode. | tray-ready | `platform-windows`, `daemon::input`, `core-input`, `app-services` snapshots | `input status`, `input owner`, `input capture-target`, `input capture-start`, `input capture-stop` | Settings input controls and runtime summary | input runtime/config tests; edge handoff trace matrix; two-node runtime smoke still required | yes |
| Device layout | Drag machines in layout. | Tray visual layout editor with one-row/grid/freeform cardinal layout for up to four peers plus local machine. | tray-ready | `app-services::desktop`, `daemon` layout validation, `tray` layout | `layout set`, `layout preview`, `layout wizard` | Layout Manager | shared layout validation tests; tray layout tests; topology smoke | yes |
| Refresh connections | Refresh/reconnect connected machines. | Manual reconnect, automatic reconnect with reason classification, per-peer health states. | tray-ready | `daemon` transport runtime/state, `tray` Settings | `diagnostics run-action reconnect`, hotkey `reconnect` | Settings reconnect action; peer health labels | reconnect generation tests; reconnect smoke | yes |
| Security key / new key | Generate new key and reset current connections. | Trust rotation command that revokes existing peer trust, clears stale sessions, and forces explicit re-pairing. | cli-ready | `core-security`, `daemon` trust/peer state | `pair export-trust`, `pair import-trust`, `pair rotate-trust`, `peer remove` | not complete | `cargo test -p core-security`; `cargo test -p boundless-daemon rotate_trust`; pairing recovery matrix still required | yes |
| Connect by name/key | Connect with remote machine name and shared key. | Safer nearby challenge-confirm pairing plus manual host fallback and clear trust identity. | tray-ready | `daemon::pairing_wire`, `core-security`, `tray` pairing | `pair request`, `pair nearby-join`, `pair create-code`, `pair approve` | Status & Pairing guided dialog | pairing recovery matrix; lockout/replay tests | yes |
| Local host name | Show current host name. | Show local display name, machine ID, endpoint, and service/control health. | cli-ready | `app-services::queries`, `ipc-api`, `daemon` status | `daemon status`, `ui snapshot`, `console` | Settings | status snapshot tests; manual tray check | yes |
| Devices in a single row | Arrange devices as one row or 2x2 matrix. | Provide one-row, grid, and explicit cardinal arrangement with validation. | tray-ready | layout parser/resolver, tray layout | `layout orient`, `layout set` | Layout Manager | shared layout validation tests; tray layout tests | yes |
| Use service | Install service to control elevated apps/lock screen. | Optional Windows service mode with explicit status, IPC ACLs, install/uninstall, elevated-app validation, and lock-screen claim classification. | plumbing | `boundless-service`, `platform-windows`, installer | `service install` blocked by default until explicit pipe ACLs land; `service status`, `service start`, `service stop` | future Settings service panel | compile validation; service smoke, explicit IPC ACLs, and elevated-app validation still required | yes |
| Uninstall service | Remove service from computer. | Service uninstall path removes service, stops daemon, and preserves or migrates user config safely. | plumbing | `boundless-service`, installer/service host | `service uninstall` | future Settings service panel | compile validation; service uninstall validation still required | yes |
| Wrap mouse | Wrap across first/last machine edges. | Configurable wrap behavior with tray setting and layout-aware validation. | tray-ready | `daemon` config/input handoff | `feature set wrap_mouse on/off`, `input status` | Settings toggle | input handoff tests; topology smoke | yes |
| Share clipboard | Share clipboard between machines. | Tray-visible text/image clipboard sharing with enable/disable, status, echo suppression, and failure reporting. | tray-ready | `daemon::clipboard`, `core-clipboard`, transport | `feature set share_clipboard`, `transport send-text`, `transport send-image` | Settings toggle; toast/history panel still pending | clipboard matrix; runtime smoke | yes |
| Clipboard file transfer | Copy files via clipboard, 100 MB limit in MWB. | Clipboard-file workflow plus explicit send-file action, progress, receive-folder policy, interruption recovery, safe receive directory enforcement, explicit consent or per-peer auto-accept opt-in, and configurable size policy. | plumbing | `core-transfer`, `daemon` network/file state, transport | explicit `transport send-file`, `file-transfer config`, `file-transfer set-receive-dir --auto-accept-trusted-peers <bool> --max-file-bytes <bytes>` | Settings receive-folder and receive-policy controls; progress/history panel still pending; transfer enable toggle not enforced yet | configurable size tests; default-deny receive-policy tests; two-node smoke explicitly opts into receive; clipboard-file and per-peer consent still required | yes |
| Drag/drop file transfer | Drag/drop can transfer one file in MWB, with known limitations around folders, multiple files, and network files. | Treat current tray drop-to-peer behavior as preview until Windows source-path, folder, multiple-file, network-path, cancellation, and receive-policy validation exist. Keep explicit send-file as the supported fallback. | plumbing | `tray`, `core-transfer`, `daemon` network/file state | `transport send-file` fallback | preview drop-to-peer path; not release-validated | validation/deferral rationale in `v5-clipboard-file-workflows.md`; readiness packet must either validate or explicitly defer public drag/drop claims | yes |
| Hide mouse at screen edge | Position cursor at edge when switching to another machine. | Setting that avoids focus traps and fullscreen/RDP issues; explicit unsupported states. | tray-ready | `platform-windows` input/cursor helpers, `daemon::input` config | `input config`, `input set-config --hide-cursor-at-edge <bool>` | Settings toggle; runtime enforcement still pending | input handoff trace; fullscreen scenario validation | yes |
| Draw mouse cursor | Draw cursor on machines without physical peripheral. | Cursor marker or remote cursor aid when Windows reports invisible/no hardware cursor state. | tray-ready | `platform-windows`, tray/status overlay if needed, `daemon::input` config | `input config`, `input set-config --draw-cursor-marker <bool>` | Settings toggle; overlay enforcement still pending | cursor visibility validation | no |
| Validate remote machine IP | Reverse DNS validation of remote machine IPs. | Optional peer endpoint validation with warnings when reverse DNS is unreliable; v5 must prove warning/enforcement behavior and avoid implying DNS is a trust boundary. | not-started | `daemon` discovery/network validation | unsupported feature setting is rejected | disabled Settings toggle with unsupported reason | endpoint validation tests; unreliable-DNS warning tests | yes |
| Same subnet only | Only connect on same intranet/subnet. | Optional subnet policy enforced before outbound connection and inbound trust acceptance, with clear diagnostics when disabled or bypassed by manual endpoint policy. | not-started | `daemon` network runtime/config | unsupported feature setting is rejected | disabled Settings toggle with unsupported reason | subnet policy tests; inbound/outbound enforcement tests | yes |
| Local control endpoint security | Not an MWB user setting; MWB service/control behavior is local to Windows. | Current-user named-pipe ACLs, service-to-user privilege separation, localhost TCP fallback warnings, and tests that unauthorized local users cannot invoke privileged actions. | plumbing | `platform-windows`, `adapter-ipc-grpc`, `daemon` control plane | `daemon status`, `service status`, future diagnostics checks | Settings service/control health | IPC ACL tests; service privilege-boundary tests; localhost fallback warning tests | yes |
| Block screen saver on other machines | Prevent screen saver on other machines. | Anti-idle/block-screen-saver policy with local and remote pulse behavior, display-required option, and battery safeguards. | plumbing | `daemon::anti_idle`, `platform-windows::runtime` | `anti-idle show`, `anti-idle set` | future Settings toggle | anti-idle tests; Windows runtime validation | no |
| Move mouse relatively | Help across different resolutions or multi-display scenarios. | Absolute/relative movement modes with mixed-DPI and multi-display validation. | tray-ready | `core-input`, `platform-windows`, `daemon::input` config | `input config`, `input set-config --relative-mouse <bool>` | Settings toggle | mixed-DPI/multi-display trace | yes |
| Block mouse at screen corners | Avoid accidental switching at corners. | Configurable corner blocking with tests for edge-start and remote handoff behavior. | tray-ready | `daemon::input`, `core-input`, `app-services` snapshots | `input config`, `input set-config --block-screen-corners <bool> --corner-block-px <n>` | Settings toggle and corner threshold selector | `cargo test -p core-input`; `cargo test -p boundless-daemon corner`; runtime trace still required | yes |
| Clipboard/network status messages | Show clipboard and network status in tray notifications. | Local bounded event stream, tray toasts/history, and CLI support bundle with default redaction for trust material, pairing artifacts, peer IDs, machine IDs, fingerprints, endpoints, local paths, request IDs, and lockout IPs. | plumbing | `daemon` events, `app-services`, `tray` toasts | `transport events`, future diagnostics export | receive-folder/progress status; toast/history panel still pending | diagnostics redaction tests; UI smoke | yes |
| Easy Mouse | Switch by moving pointer past screen edge; optional modifier requirement. | Edge switching with optional modifier policy and fullscreen suppression. | tray-ready | `daemon::input`, `platform-windows` hooks | `feature set easy_mouse`, `input status`, hotkey toggle | Settings toggle | edge handoff tests; fullscreen validation | yes |
| Disable Easy Mouse in fullscreen | Prevent switching when fullscreen app is focused. | Fullscreen detection with allowlist/ignore list and visible fallback when unavailable. | not-started | `platform-windows`, `daemon::input` | future feature setting | future Settings toggle/list | fullscreen app validation | no |
| Ignored fullscreen applications | Allow Easy Mouse for listed fullscreen executables. | Executable allowlist for fullscreen suppression exceptions. | not-started | config, `platform-windows` foreground app detection | future config command | future Settings list | allowlist tests | no |
| Shortcut to toggle Easy Mouse | Configurable `Ctrl`+`Alt`+letter shortcut. | Existing hotkey action remains configurable in CLI and tray. | tray-ready | `daemon::hotkeys`, `platform-windows` | `hotkey toggle_easy_mouse <combo>` | Settings hotkey editor | hotkey unit/runtime tests | yes |
| Shortcut to lock all machines | Shortcut pressed twice locks all machines with same setting. | Lock local/paired machines where supported, with service-mode and trust constraints visible. | plumbing | `daemon::hotkeys`, transport anti-idle/input control, `platform-windows` | `hotkey lock_machine <combo>` | Settings hotkey editor for local lock; paired-machine lock pending | lock behavior tests; Windows validation | no |
| Shortcut to reconnect | Configurable reconnect shortcut. | Existing reconnect hotkey remains configurable and reports reconnect reason. | tray-ready | `daemon::hotkeys`, transport runtime | `hotkey reconnect <combo>` | Settings hotkey editor | reconnect hotkey tests | yes |
| Shortcut for multi-machine mode | Send same input to all machines. | Explicit multi-cast input mode with strong visual state, easy escape, and off-by-default policy. | not-started | `daemon::input`, `core-input`, tray state | future command | future status/control | multi-cast input tests | no |
| Shortcut to switch to specific machine | `Ctrl`+`Alt`+number or `F1`-`F4` switch. | Direct capture-target shortcuts for up to four peers with layout-aware labels. | not-started | `daemon::hotkeys`, layout resolver | future hotkey command | future Settings hotkey editor | hotkey target tests | no |
| Add firewall rule | Install firewall rule for MWB. | Optional firewall rule install/check/remove with admin prompts and diagnostics, not silent mutation. | not-started | Windows installer/scripts/platform glue | future firewall command | future Settings action | firewall rule validation | yes |
| Status colors | Color-coded connection states. | Tray peer health states with accessible labels and exact reason text. | tray-ready | `app-services::queries`, `tray` status UI | `console`, `daemon status` | Status & Pairing peer health labels | UI/state tests | yes |
| Troubleshooting guidance | Same network, key/host, firewall, refresh connections. | Built-in diagnostics and docs start distinguishing pairing, transport, TLS trust, protocol, and firewall-suspect issues; stale daemon, named pipe, and service diagnostics remain release follow-ups. | cli-ready | diagnostics ops, docs, tray health | `diagnostics dump`, `console` | peer health labels; full diagnostics panel pending | redacted diagnostics test; support bundle validation still required | yes |
| Original MWB UI | Show legacy original UI. | Not applicable; Boundless has its own tray UX. | out-of-scope | none | none | none | rationale in docs | no |

## V5 Readiness Snapshot Requirements

The v5 readiness packet must include a copy of this matrix with each release-blocking row set to `validated`, `deferred`, or `out-of-scope`. For each `validated` row, the packet must include:

- the commit SHA,
- validation command,
- validation result,
- artifact path when applicable,
- remaining risk,
- and any manual environment prerequisite.

For each `deferred` row, the packet must include:

- reason for deferral,
- user-facing claim removed or softened,
- target follow-up release,
- and known workaround if one exists.

## Workstream Completion Checklist

Each milestone commit must update this checklist when it moves a matrix row closer to `validated`.

| Workstream | Matrix Rows It Must Complete Or Explicitly Defer |
| --- | --- |
| V5-1 Parity contract | All rows classified with status, owner surface, CLI/tray surface, validation evidence, and release-blocker status. |
| V5-2 Four-machine topology and layout UX | Control up to four computers; Device layout; Devices in a single row. |
| V5-3 Windows service mode | Use service; Uninstall service; Local control endpoint security. |
| V5-4 Input handoff excellence | Shared keyboard and mouse; Wrap mouse; Hide mouse at screen edge; Draw mouse cursor; Move mouse relatively; Block mouse at screen corners; Easy Mouse; Disable Easy Mouse in fullscreen; Ignored fullscreen applications; Shortcut to toggle Easy Mouse; Shortcut to lock all machines; Shortcut to reconnect; Shortcut for multi-machine mode; Shortcut to switch to specific machine. |
| V5-5 Clipboard and file workflows | Share clipboard; Clipboard file transfer; Drag/drop file transfer; Clipboard/network status messages. |
| V5-6 Pairing, trust rotation, and network safety | Security key / new key; Connect by name/key; Validate remote machine IP; Same subnet only; Add firewall rule; Troubleshooting guidance. |
| V5-7 Complete tray settings surface | Every row with a future or incomplete tray surface, including settings, hotkeys, receive folder, network policy, service status, and diagnostics controls. |
| V5-8 Reliability and observability | Refresh connections; Clipboard/network status messages; Status colors; Troubleshooting guidance; Local control endpoint security. |
| V5-9 Installer and release hardening | Use service; Uninstall service; Add firewall rule; Local host name; Troubleshooting guidance. |
| V5-10 Validation harness and readiness packet | Every release-blocking row must have command output, artifact evidence, skip rationale, or deferral rationale in the readiness packet. |
| V5-11 Documentation, support, and migration | Every deferred/out-of-scope row must have user-facing rationale; all validated rows must link to user docs and troubleshooting guidance. |

## Current Required Follow-Ups

- Build a `docs/parity/v5-readiness-template.md` packet template before release hardening begins.
- Add one issue or work item per release-blocking row that is not yet `validated`.
- Update this matrix after every v5 milestone commit.
