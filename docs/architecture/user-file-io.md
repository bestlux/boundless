# Windows user file I/O authority

Status: implemented candidate; Windows token/ACL fixtures pass. Installed LocalSystem service and multi-user session validation remain release gates.

The Windows service still hosts the daemon as LocalSystem. That identity owns daemon configuration, device identity and service logs. It must not supply access to paths chosen through file transfer, receive-folder settings or diagnostic export. Those operations now use `platform_windows::user_io::UserIoLease`.

## Authority contract

In service mode, a lease requires the existing installed allowed SID and a nonzero active console session. `WTSQueryUserToken` supplies that session's primary token. The adapter verifies its SID, session and unelevated state, duplicates an impersonation token, and retains the session and authentication LUID. It rejects an elevated desktop token, a different user, a missing console or a missing configured SID. There is no process-token fallback for a service.

In the per-user daemon, the lease uses its process token. LocalSystem is explicitly rejected on this path. Running the per-user daemon elevated therefore retains that explicitly elevated process's file permissions; the installed service contract requires an unelevated user token.

Each name-based operation runs in a synchronous closure on a Tokio blocking worker:

1. Recheck the active console, fixed SID and authentication LUID.
2. Impersonate the retained user token on that worker thread.
3. Resolve and operate on filesystem names synchronously.
4. Revert the thread token before returning, including during unwinding.

A failed service authority check permanently revokes that lease. Revert failure terminates the process because reusing a worker with unintended authority is unsafe. A worker already impersonating another identity is rejected. No impersonation scope crosses an `await`, and no closure returns deferred path-based I/O for later execution.

Opened file handles may cross async boundaries. Windows checked their granted access using the user token when they were opened. Later operations use those handles rather than resolving the source path again as LocalSystem. User-selected paths, including junctions and other reparse points, therefore remain subject to the user's OS access checks at the operation that resolves them. Lexical prefix checks are not the privilege boundary.

## Operations and ownership

| Operation | Authority and lifetime |
| --- | --- |
| Queue a source file | Capture a lease; open source and read metadata on the same user-authorized handle; retain that handle and lease in daemon `OutboundFileTransfer`. |
| Read a transfer chunk | Recheck feature and lease; inspect and read the retained handle. A source path replacement does not redirect the transfer. |
| Retry a failed source | Start a new transfer, capture fresh authority and open the source again as that user. No privileged reopen. |
| Reserve a receive file | Check feature, size and sanitized basename before storage creation; create directory and exclusive `.part` file under the lease. |
| Write/complete a receive | Recheck feature, receive consent and lease at chunk/end boundaries; write the retained handle, then publish under the lease with no-overwrite rename. |
| Remove a partial file | Close its data handle and attempt removal through its original lease. Revoked authority never triggers a LocalSystem cleanup fallback. |
| Change receive folder | Create/validate the requested folder through a lease before persisting the configuration. |
| Export diagnostics | Resolve the user's default known folder or supplied destination through a lease; create exclusive dump and redaction sidecar files under that token. |

`InboundTransfer` belongs to `daemon::network`, where its user lease and filesystem handle can be owned together. `peer-transport` retains protocol and flow-control state without platform authority. The older internal `OutboundPayload::FileChunk` carries only a source path; service mode rejects it. Normal `SendFile` uses the supported cursor and retained-handle path. This changes no remote wire format.

At most 64 queued/active outbound sources retain handles. A semaphore reserves capacity before a source is opened, including concurrent requests; error, completion and cancellation release it. Reaching the limit returns a capacity error instead of accumulating more handles.

The service does not create the receive directory during daemon startup. An exact persisted service-profile default is resolved to the installed user's Downloads/Boundless folder when authority is available. Explicit destinations remain explicit. Existing files, private keys and installation identity are not moved.

Diagnostic status/config queries remain available without a selected user token. Diagnostic filesystem exports require a valid selected user, even when the requesting pipe client is an administrator. Default exports go to that user's LocalAppData/Boundless/diagnostics.

## Feature and consent behavior

`transfer_file=false` prevents new source opens, retries and receive reservations. Disabling cancels known outbound transfers and their queued payloads. The outbound writer checks the feature again for payloads already drained into a local batch. An active incoming transfer is rejected and discarded at its next chunk/end boundary; no further chunk is written after that check. An idle incoming transfer can retain its existing handle/partial until another frame, session cleanup or shutdown.

Incoming file acceptance remains separately opt-in through `auto_accept_trusted_peers`; the default is false. File authority does not grant peer trust or receive consent. Existing size, basename and transfer-count limits still apply. Files are never launched automatically.

Checks are operation-boundary revocation, not a transactional fence around all async work. An operation already admitted on a user-authorized handle may complete while policy or the console changes. A very brief switch away and back between observations is not detected; a failed observation latches revocation. Windows session-event generations and immediate cancellation are future lifecycle work, not claims made by these checks.

## Named-pipe policy

The control pipe DACL retains full access for LocalSystem and enabled built-in Administrators. The installed allowed SID receives explicit `0x12019b` client rights, excluding `FILE_CREATE_PIPE_INSTANCE` (the bit shared with `FILE_APPEND_DATA`). The shared client opens with those data rights and identification-only security QoS instead of generic write.

The service listener checks nonadministrative clients against the fixed SID and active console session when accepting and on subsequent reads/writes. Administrative diagnostic/recovery access remains usable without a console. Input broker authorization applies its own stricter identity policy; transport admission does not substitute for it.

An unelevated per-user daemon retains full same-user pipe rights because its accept loop must create successor instances under that same SID. There is no elevation boundary between that host and its ordinary clients. Service and elevated hosts use the narrow user ACE and their own SYSTEM/Administrators ACE for server operations. A restricted-token fixture checks both policies, including ordinary per-user successor creation and reconnect.

Client SID and process creation time are queried using the same held process handle; session information is obtained from the connected pipe. Failed queries remain absent and identity-gated operations reject them. Remote pipe clients are rejected.

The narrow DACL prevents an ordinary allowed user from adding another instance of an existing service pipe. It does not authenticate a service that is absent: another process could create the first instance while Boundless is stopped. Generic daemon-client server authentication remains a separate follow-up; the elevated injector's existing expected-server-PID check is preserved.

## Validation and release gates

The Windows Rust fixtures use UUID-named disposable directories/pipes and restricted tokens. They do not change installed-service state, existing user-file ACLs or system destinations. They demonstrate:

- Allowed user writes and publication succeed; host-only fixture reads, writes, directory creation, deletion and publication fail under the restricted token.
- A real NTFS junction cannot turn an inaccessible target into a privileged read, write or final publication.
- Source handles continue to read the authorized original after a name replacement; reopening the replacement is denied by its fixture ACL.
- Thread impersonation reverts after both an error and a caught panic on the same worker thread.
- An existing final destination survives a publication collision unchanged.
- An ordinary allowed user can connect to the control pipe but cannot create an additional server instance.
- Daemon feature-disable, missing service authority, retained source and receive-revocation paths behave as specified.

The ordinary-user fixtures cannot exercise successful `WTSQueryUserToken`, which requires LocalSystem and the appropriate privilege. Before public qualification, run an isolated installed-service matrix covering standard user, UAC administrator desktop, no console, sign-out/sign-in, fast user switching, console reconnect, denied source/destination ACLs, redirected known folders and service restart. Verify that supported desktops supply an unelevated token; elevated-only desktop configurations fail closed and need an explicit support decision.

Also verify real tray/CLI named-pipe interoperability, admin diagnostic access without a console, large-file transfer/cancellation and antivirus/disk-full publication behavior. Forced shutdown or expired leases can retain partial files: future cleanup must reacquire the same user's authority and enforce storage retention, never sweep user paths as SYSTEM.

This boundary makes the current fixed-SID service safer while preserving file transfer. It is a prerequisite for BND-NEXT-45, not authorization to widen admission to whichever user logs on. A later user-session engine can replace this adapter after identity ownership, session revocation, repair and upgrade contracts are tested; a whole-product restart is unnecessary for that migration.

## Windows references

- [WTSQueryUserToken](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken): service privilege requirement and token-handle ownership.
- [Named-pipe security and access rights](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights): additional-instance permission and generic-write aliasing.
- [RevertToSelf](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-reverttoself): ending thread impersonation and failure behavior.
