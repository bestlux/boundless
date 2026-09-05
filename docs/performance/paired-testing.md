# Paired transport testing

This suite lets a person or an AI tool on one PC measure an existing trusted connection, after someone explicitly permits testing on the other PC. It exercises the actual authenticated TCP/TLS session. It does not type, move the pointer, read or change the clipboard, transfer user files, execute commands, or interrupt a connection.

Both PCs must run transport protocol **4.5**. Protocol 4.4 peers are rejected during negotiation; upgrade both PCs. Existing 4.4 local configuration migrates automatically. The application version alone is insufficient to identify a development build; reports include SHA-256 of the running executable when readable, the daemon instance, and the negotiated transport session IDs.

## One controller, explicit permission on the peer

Find the exact paired machine IDs with `boundlessctl peer list --json` and `boundlessctl daemon status --json`.

On the receiving PC:

```powershell
boundlessctl paired-test allow <controller-machine-id> --seconds 300 --json
```

On the controller:

```powershell
boundlessctl paired-test run <receiving-machine-id> --samples 20 --payload-bytes 65536 --timeout-ms 2000 --json > paired-transport.json
```

On the receiving PC, inspect or revoke its permission:

```powershell
boundlessctl paired-test status --json
boundlessctl paired-test revoke --json
```

The CLI exits unsuccessfully when the test fails, while retaining its structured report on stdout. Failure to contact the local daemon, an invalid option, an offline peer, or local authorization failure is a command error on stderr. Granting permission never starts tests; running a test never grants permission on the peer. The local control endpoint must be an authorized Windows named pipe or loopback TCP development endpoint. A paired connection cannot invoke the consent RPC.

## What is measured

| Test | Work | Evidence |
| --- | --- | --- |
| `transport_rtt` | Echo 64 synthetic bytes for each sample | End-to-end daemon queuing, transport and remote handling, timed using one local monotonic clock |
| `bulk_echo_integrity` | Echo the requested synthetic payload, compare every byte | Integrity and latency for bounded payloads on the live connection |

Each summary includes the requested and completed sample counts, raw microsecond samples, nearest-rank p50/p95, verified round-trip byte count, and explicit errors. A failed or timed-out request stops the suite; missing measurements are never synthesized. Session or daemon identity changes fail the run. Build metadata is prepared before the measured loop; startup and pairing costs are not included. Identity metadata records whether debug assertions were enabled; compare optimized candidate measurements separately from development test binaries.

`evidence_category` derives from the actual socket:

- `loopback`: TCP/TLS over a loopback address, as in the automated two-daemon fixture.
- `real_paired`: TCP/TLS over a non-loopback address. It does **not** attest that two physical PCs were used; a VM, container, or another interface on this PC can also qualify.
- `synthetic`: the in-memory, post-authentication harness. This is never physical-device evidence.
- `null`: no successful response established a measurement category.

The report explicitly lists physical keyboard/mouse injection, emergency unlock, clipboard, file workflows, reconnect recovery, CPU/memory/disk budgets, and physical two-PC attestation as `not_tested`. A passing transport test does not establish those product contracts. In particular, bulk echo is not a file-transfer benchmark and no throughput claim is made from these short RTT samples.

## Limits and privacy

- One local test run and one outstanding request at a time. Cancellation discards the pending request and its payload.
- At most 100 samples per workload, 64 KiB per request, 30 seconds per run, and 100–5,000 ms per request.
- One permitted peer per daemon, for at most 600 seconds. The volatile lease disappears on daemon restart, can be revoked, and permits at most 256 requests and 16 MiB of request payload. Echo responses add at most the same amount of payload traffic; framing and identity metadata add small bounded overhead.
- Requests require a trusted, authenticated, currently owned session and the peer-specific lease. Late replies and replies from another session cannot satisfy the pending request. Rejected requests produce at most one denial response per second across peers and no per-probe logs.
- Synthetic payloads never contain user data. Reports contain machine IDs, version, OS/architecture, random daemon/run IDs, executable hashes, and transport timings. They contain no display names, IP addresses, filesystem paths, certificates, input events, clipboard text or file contents.
- `source_revision` is present only if `BOUNDLESS_SOURCE_REVISION` was supplied at compile time; otherwise it is `null`. Never infer the installed daemon's revision from the controller's checkout. To bind a candidate, compare both binary hashes with the exact candidate executable; a version string is not sufficient.

## Functional validation

```powershell
cargo test -p boundless-daemon paired_testing -- --nocapture
```

The TCP/TLS fixture creates isolated configuration, identities, trust and inboxes in unique temporary directories. It verifies denial before permission, real authenticated echo measurements after permission, revocation, identity/session provenance, exact 256-request exhaustion, actual lease expiry, and absence of queued input or received files. An untrusted certificate fails actual TLS authentication. A per-daemon test barrier pauses an accepted probe while a preferred authenticated session replaces the old connection: the old request times out, and a fresh run on the replacement succeeds. The barrier does not fabricate a response or a timing sample. Additional deterministic tests cover peer scope, byte exhaustion, wrong peer/request/session correlation, and cancellation cleanup. The fixture's emitted `PAIRED_TEST_REPORT` is an actual local measurement, labeled `loopback`; its executable hash identifies the test executable, not an installed daemon.

Use this suite as the first transport check in a physical test session. Public readiness still requires separate intentional tests for interaction, failure recovery, installation and sustained offline resource use.
