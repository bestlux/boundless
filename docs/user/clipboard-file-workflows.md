# Clipboard And File Workflows

Boundless separates clipboard sharing from file transfer policy so users can see and control what crosses machines.

## Clipboard Text And Images

Use Settings to enable or disable clipboard sharing.

Supported v5 surfaces:

- clipboard text payloads,
- bitmap image payloads,
- status and diagnostics for recent clipboard transport events,
- echo suppression to avoid replay loops.

## Explicit File Send

CLI fallback:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl transport send-file <peer_id> <path>
& $BoundlessCtl transport events --limit 100
```

File receive behavior is governed by:

- receive directory,
- organize-by-peer behavior,
- max file size,
- default-deny receive policy,
- global trusted-peer auto-accept opt-in.

Boundless does not silently accept files from untrusted peers. Per-peer prompts and per-peer auto-accept remain follow-up work. Boundless also does not auto-open received files.

## Drag And Drop

Public drag/drop claims remain deferred until there is runtime validation for that exact workflow. The supported v5 path is clipboard/file transfer through documented tray and CLI controls.

## Failure Cases To Preserve

Support and release validation should keep evidence for:

- unsafe destination directory,
- path traversal,
- duplicate names,
- oversized files,
- interrupted transfer,
- hash mismatch,
- reconnect recovery,
- partial temp-file cleanup.
