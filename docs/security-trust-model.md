# Security And Trust Model

Boundless v5 is a local-network desktop utility. It is not a remote administration product and does not claim hard isolation against the same Windows user account.

## Local Pairing

Pairing is explicit. Users compare and approve challenge-confirmation codes before trust is established. Manual host fallback exists for networks where discovery is unavailable.

Trust rotation clears peers, revokes local trust material, aborts active sessions, and forces re-pairing after restart.

## TLS Trust

Peer transport is authenticated against Boundless trust state. Diagnostics and readiness evidence must distinguish stale trust, protocol mismatch, firewall-suspect behavior, and ordinary reconnects.

## Local IPC

The default Windows control endpoint is a local named pipe:

```text
npipe://./pipe/boundlessd-api
```

Tray and CLI default to the same endpoint. Local same-user mutability remains in scope; do not describe the tray dashboard or diagnostics as a security boundary.

## Service Mode

The service binary exists, but the per-user MSI does not silently install a service. Service install remains an explicit admin action and is blocked by default until named-pipe ACL and privilege-boundary review is complete.

Lock-screen and elevated-app control are not release-grade claims until validated on Windows with service smoke evidence.

## Clipboard And Files

Clipboard sharing and file transfer are user-controlled. File receive policy defaults conservative, and trusted-peer auto-accept must be explicitly enabled. Per-peer auto-accept remains follow-up work. Received files are not auto-opened.

## Diagnostics

Diagnostics redact machine identity, fingerprints, API endpoints, input owner/target ids, request ids, and lockout IPs. A sidecar redaction manifest records what categories were redacted.

Diagnostics are support evidence, not proof that no sensitive data exists elsewhere in logs, OS crash dumps, shell history, or user-captured screenshots.

## Residual Risks

- Same-user local processes can often observe or interfere with desktop utilities.
- Network discovery can be filtered, spoofed, or unavailable depending on local network policy.
- Firewall rules are not silently managed by v5.
- Service mode needs more privilege-boundary validation before elevated-app or lock-screen claims are complete.
- Runtime input behavior depends on Windows hook availability, focus state, display topology, DPI, and fallback mode.
