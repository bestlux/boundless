# Security and trust model

Boundless shares desktop input and selected data between explicitly paired PCs on a trusted local network. Windows-to-Windows is the supported product direction. Boundless is not an administrative remote-command service and does not isolate itself from a compromised administrator or other processes running as the same user.

This document describes the Windows hardening source contract. The [project status](project-status.md) and [readiness matrix](parity/mouse-without-borders.md) identify implementation and runtime evidence; they do not imply these changes are installed on an existing PC.

## Pairing and transport

Pairing requires deliberate challenge confirmation. Discovery suggests candidates; it does not establish trust. Verify the intended PC before exchanging approval, and use manual host entry when discovery is unavailable. Import trust bundles only after authenticating their source and verifying the peer identity out of band.

Peer sessions authenticate against the local trust store and enforce exact protocol compatibility. The paired-test extension uses protocol 4.5.0, requiring compatible builds on both PCs. An offline peer remains paired; repeated connection failures must be delayed and summarized. Incoming connection admission, TLS handshake time, frame sizes, queues, and log storage have independent limits.

Removing a peer and rotating local trust are different operations. Reset and rotation controls must describe which pairings, identity, and preferences they remove. Never upload a trust store or private key as diagnostic evidence.

## Windows identities and privilege

| Component | Authority and boundary |
| --- | --- |
| MSI and Windows service control | Explicit Windows elevation; machine-wide installed binaries under `%ProgramFiles%\Boundless`. |
| `BoundlessService` | LocalSystem service, currently also hosting peer transport and core runtime. This remains a substantial privileged surface. |
| Local control pipe | ACL admits System, local Administrators, and the configured allowed user; operation-specific identity checks further restrict broker and user-session operations. |
| Tray input broker | Selected user's interactive session; SID, session, PID, and process creation time come from verified Windows client identity, not RPC assertions. |
| User file operations | Captured unelevated authority for the configured user's active console session; missing or changed authority fails closed for that operation. |
| Optional elevated injector | Explicitly launched narrow input/release helper for the same split-token administrator account; no general remote-command interface. |

The current MSI still requires `BOUNDLESS_ALLOWED_USER_SID`; its matching helper captures the intended user before UAC. It must not silently substitute the elevating administrator. Moving to a plain MSI and reducing the service to a smaller privileged role are separate roadmap items, not completed consequences of file-I/O impersonation.

## Local control and input lifetime

Windows defaults to `npipe://./pipe/boundlessd-api`. The service uses identities obtained from the connected pipe/process. A caller-supplied SID, PID, session ID, or broker token alone is insufficient authority to become the input broker. Local same-user access is intentional; the dashboard is not an isolation boundary against another process of the same user.

A live broker reconnect and a replacement process have different recovery semantics. Only the same verified process incarnation may retain exact delivery receipts. Process replacement must not replay uncertain key-downs, moves, or wheel events. It can schedule conservative releases, clear ownership, rotate the input epoch, and require a fresh handoff. See the [broker lifetime contract](architecture/user-session-input-broker.md).

In the canonical service + tray-broker mode, the tray's local emergency release remains available when daemon IPC is unavailable. A standalone developer daemon owns its own hooks and still requires daemon acknowledgment for a dashboard pause. Supported input scope is the selected user's unlocked normal desktop. Secure desktop, UAC prompts, lock screen, and other user sessions remain unsupported. The experimental elevated injector is described in the [service guide](user/service-mode.md); unsigned dogfood operation is not a trusted-publisher or UIAccess claim.

## User files and diagnostics

A trusted peer is not filesystem authority. File receipt defaults to denial, requires explicit global trusted-peer auto-accept, and is subject to file sharing, size, path, hash, and temporary-file rules. Per-peer/per-transfer consent remains a product gap. Received files are not opened automatically.

In service mode, user-selected source, receive, and diagnostic-export paths must be resolved/opened under the selected user's unelevated token. Scoped synchronous impersonation must revert before returning or awaiting. Deferred operations revalidate the session lease; retries do not reopen the same path with System permissions. Already opened handles retain their original access rights, so session validation and handle lifetime both matter. This boundary reduces authority; it is not a separate-process sandbox.

Diagnostic exports redact configured identity, endpoint, path, and event categories and exclude clipboard contents, private keys, and trust secrets. A redaction manifest accompanies the export. Filenames require an explicit opt-in. These are locally saved support artifacts; the user decides whether to share them. Redaction does not cover arbitrary OS crash dumps, shell transcripts, screenshots, or unrelated files.

## Resource safety

Runtime and service-startup logs have independent byte budgets, segment limits, bounded record sizes, and bounded producer queues. Storage failures disable disk logging temporarily instead of blocking input release or creating an unbounded fallback log. Limits are per stream and Windows security context, not a global limit across every Windows account and installation. See [log storage and recovery](user/troubleshooting.md#log-storage-and-disk-growth).

Rate limits do not replace storage caps. A healthy peer must keep making progress while another is unreachable or its writer is blocked; connection/session ownership is per peer. The [network map](architecture/network-v1.md) specifies deadlines, queue ownership, and cancellation behavior.

## Remaining limits

- The full service-hosted runtime is still privileged. Further process separation needs an explicit migration and failure contract.
- Session change, Windows ACLs, installed broker/helper death, and upgraded binaries need Windows runtime evidence in addition to deterministic tests.
- Firewall policy is not silently changed. Use a trusted private network; do not expose control or pairing ports to the internet.
- Physical input behavior depends on hooks, focus, desktop/session state, DPI, and display arrangement.
- Security reports should use [SECURITY.md](../SECURITY.md), not public issue attachments containing sensitive state.
