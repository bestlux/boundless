# V5 Clipboard And File Workflows

Boundless v5 should match Mouse Without Borders clipboard behavior where it is useful, and exceed it where stronger policy or diagnostics make the workflow safer.

## Goals

- Keep clipboard text and bitmap image sharing bounded, observable, and recoverable.
- Provide an explicit send-file workflow through CLI and tray plumbing.
- Keep existing tray drop-to-peer behavior classified as preview until it has release evidence.
- Default incoming file receipt to denied unless a receive decision exists.
- Preserve a conservative default file limit while allowing a configured limit for trusted environments.
- Record transfer start, progress, completion, and rejection events in the local transport event stream.
- Keep receive-folder policy shared between CLI, tray, daemon, and IPC surfaces.

## Non-Goals

- Do not auto-open received files.
- Do not silently accept files from untrusted peers.
- Do not claim native drag/drop support until Windows source-path capture and receive-policy behavior have release evidence.
- Do not treat trusted peer identity as consent to receive files without an explicit receive policy.
- Do not claim per-peer receive prompts until the tray owns that prompt and the daemon enforces the resulting decision.

## Current V5 Contract

Incoming file transfer is default-deny. A receiver must either accept a future explicit receive prompt or opt into auto-accept for trusted peers. The implementation currently supports the trusted-peer auto-accept policy as a global file-transfer setting; per-peer prompts and per-peer auto-accept remain V5 follow-ups inside the tray settings milestone.

The default file limit is `core-transfer::MAX_TRANSFER_BYTES`, which remains aligned with the Mouse Without Borders 100 MB compatibility point. Operators can raise or lower the limit through `file-transfer set-receive-dir --max-file-bytes <bytes>`. A zero-byte limit is rejected by config validation and daemon runtime updates.

The currently supported power-user workflow is explicit send-file:

```powershell
boundlessctl file-transfer config
boundlessctl file-transfer set-receive-dir <path> --organize-by-peer --auto-accept-trusted-peers true --max-file-bytes 104857600
boundlessctl transport send-file <peer-id> <path>
boundlessctl transport events
```

The two-node smoke script intentionally enables auto-accept on the receiving node before it sends a file. That keeps the runtime validation honest: file receipt is tested, but only after explicit policy opt-in.

Windows clipboard file-list ingestion and paste/receive semantics are not complete yet. V5 must not mark clipboard-file transfer as `validated` until copied-file clipboard paths are captured, transferred, received, and surfaced through the same receive-policy and progress model as explicit send-file.

## Validation Requirements

Before the V5 release packet can mark clipboard-file transfer as `validated`, it must include:

- default-deny receive-policy test evidence,
- configured auto-accept receive-policy test evidence,
- configured size-limit rejection evidence,
- safe filename and path traversal rejection evidence,
- duplicate filename conflict evidence,
- interrupted transfer cleanup evidence,
- hash-mismatch rejection evidence,
- Windows clipboard copied-file ingestion and receive evidence,
- per-peer receive prompt or per-peer auto-accept enforcement evidence,
- two-node smoke evidence with explicit receive opt-in,
- tray evidence for receive folder, progress, completion, and failure state.

## Drag/Drop Readiness Decision

The tray currently has a preview drop-to-peer path that can call the shared send-file command for dropped local files. It is not release-validated drag/drop parity. The supported V5 fallback remains explicit `transport send-file`.

The product must not advertise drag/drop file transfer until the readiness packet contains validation evidence for single-file, folder, multiple-file, network-path, cancellation, receive-policy, progress, and failure cases. If that validation does not land, the readiness packet must explicitly defer the public drag/drop claim while leaving the preview affordance either hidden or labeled as experimental.
