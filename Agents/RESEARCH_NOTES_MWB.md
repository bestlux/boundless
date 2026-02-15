# MWB Research Notes (PowerToys Source)

## Feature baseline extracted

- Pairing via machine name + security key
- Multi-machine matrix/layout control (with wrap and one-row modes)
- Keyboard + mouse sharing with edge-based switching
- Easy Mouse modes and hotkeys
- Clipboard sharing (text/image) and file transfer (100MB guidance in UI)
- Reconnect flow and socket health management
- Optional service mode
- Firewall rule helper action
- Network restrictions/toggles (same subnet, remote IP validation, name-to-IP mapping)
- Policy (GPO) controls for enterprise restrictions

## Notable technical structures in PowerToys MWB

- Distinct app/module/service responsibilities
- Explicit package types for protocol frames (handshake, heartbeat, input, clipboard, matrix)
- Strong reliance on Win32 hooks and injection APIs
- Settings-managed behavior with dynamic reload and policy overrides

## Boundless direction

- Keep parity on core collaboration behaviors
- Improve reliability diagnostics and config recovery first
- Preserve constrained alpha scope to avoid design debt and overreach
