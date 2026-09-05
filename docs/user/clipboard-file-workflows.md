# Clipboard and file workflows

Use **Sharing** to choose what crosses between paired PCs. Clipboard sharing and file transfer have separate controls.

## Clipboard

Clipboard text and bitmap images are implemented, with echo suppression to prevent a received clipboard update from looping between peers. Both PCs need sharing enabled and a live authenticated connection.

Explorer file copy/paste and cross-PC drag/drop are not public product claims. Use **Files** for an explicit file send.

## Send and receive files

In **Files**, choose the destination PC and file, then follow its transfer status. The CLI fallback is:

```powershell
$BoundlessCtl = "$env:ProgramFiles\Boundless\boundlessctl.exe"
& $BoundlessCtl transport send-file <peer_id> <path>
& $BoundlessCtl transport events --limit 100
```

The receiving PC must have file transfer enabled, a permitted receive folder, sufficient space, and **trusted-peer auto-accept** explicitly enabled in Sharing. Auto-accept is currently global across trusted peers. There is no per-transfer approval inbox or per-peer auto-accept policy yet. Pairing alone does not permit receipt, and received files are never opened automatically.

Save a changed receive folder explicitly. Toggling another sharing setting must not silently save an unfinished folder edit. Organize-by-peer and maximum-file-size settings further constrain receipt.

## Windows authority

In service mode, user-selected file paths are accessed under the selected desktop user's unelevated authority. The LocalSystem service is not permission to read or write a protected folder. A missing or changed interactive session can stop an operation until the selected user is available again. Choose a folder that user can access normally; running the tray as administrator is not a workaround.

## A failed transfer

Inspect the transfer's reported reason before retrying. Check the peer's connection, file sharing and receipt settings, source-file availability, destination permissions, file size, and free space. Retry is an explicit operation and must recheck current policy and user authority.

Unsafe paths, oversized offers, hash mismatches, cancellation, interrupted writes, duplicate names, and abandoned temporary files are functional-test scenarios. Source tests cover portions of this behavior; the [readiness matrix](../parity/mouse-without-borders.md) records what still needs physical Windows evidence.
