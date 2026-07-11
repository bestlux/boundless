# Clipboard Image Memory Profiling

Date: 2026-06-20
Scope: BND-NEXT-8A, clipboard image memory pressure.

## Repro Command

Run from the repo root:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\profile-clipboard-image-memory.ps1

Canonical validation command:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\profile-clipboard-image-memory.ps1 -Mode Validate

To preserve a named evidence file:

    powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\dev\profile-clipboard-image-memory.ps1 -OutputPath .\artifacts\performance\clipboard-image-memory\clipboard-image-memory.json

The script builds the boundless-daemon lib test binary, runs the ignored clipboard_image_memory_profile_workload test with synthetic valid BMP payloads, and samples the child process working set. It does not read the real system clipboard, start a daemon, require a second machine, or print clipboard payload bytes.

Default scenarios:

| scenario | path exercised |
| --- | --- |
| noop | Test process and AppState setup baseline. |
| direct-outbound | send_clipboard_image-style peer queue into outbound transport flush. |
| local-outbound | Local clipboard observation, replay retention, connected-peer queue, outbound transport flush. |
| inbound-chunked | Chunked remote image start/chunk/end reassembly into the remote clipboard apply queue. |

Default payload sizes are 2 MiB and 8 MiB. 8 MiB is the default ClipboardPolicy::max_image_bytes.

## Image Path Map

- Windows clipboard reads use platform-windows::clipboard_backend::WindowsClipboardBackend::read_payload, which materializes formats::Bitmap into ClipboardPayload::Image(Vec<u8>).
- Local clipboard observation calls queue_local_clipboard_image_for_connected_peers; it validates policy and BMP shape, stores the latest replay snapshot, prunes stale outgoing clipboard payloads, and queues the image for connected peers.
- Direct control-plane image sends call AppState::queue_clipboard_image, validate BMP shape, and enqueue OutboundPayload::ClipboardImage.
- Outbound transport drains bulk queues in network::outbound; protocol 4.4 sends images above 8 KiB as ClipboardImageStart, receiver-credited 8 KiB ClipboardImageChunk frames, and ClipboardImageEnd.
- Inbound chunked images allocate InboundClipboardImageTransfer.data with the announced total size, extend it per chunk, validate final hash, and enqueue a full remote clipboard image payload.
- Remote clipboard apply writes the full ClipboardPayload::Image back to the platform clipboard backend. This still requires a full image payload in memory.

## Evidence

Measured on Windows with a debug test binary using sampled child-process working set. Payload bytes are synthetic BMP data and are not logged.

| scenario | size | before peak MiB | after peak MiB | delta |
| --- | ---: | ---: | ---: | ---: |
| noop | 0 | 10.61 | 12.76 | +2.15 |
| direct-outbound | 2 MiB | 19.75 | 15.18 | -4.57 |
| direct-outbound | 8 MiB | 37.75 | 21.14 | -16.61 |
| local-outbound | 2 MiB | 21.89 | 17.26 | -4.63 |
| local-outbound | 8 MiB | 45.89 | 29.28 | -16.61 |
| inbound-chunked | 2 MiB | 16.66 | 16.61 | -0.05 |
| inbound-chunked | 8 MiB | 28.65 | 28.61 | -0.04 |

Evidence files from the BND-NEXT-8A run:

- artifacts/performance/clipboard-image-memory/baseline-before-fix.json
- artifacts/performance/clipboard-image-memory/after-bounded-fix.json

These artifact files are local run output and are not required to be committed.

## Recommendation

Category: bounded allocation fix implemented.

The earlier material issue was in the outbound/local path: images that exceeded the wire payload cap were cloned into a monolithic ClipboardImage message before falling back to chunked transfer. Protocol 4.4 now bypasses monolithic encoding above 8 KiB and advances one receiver-credited 8 KiB chunk per read/write turn. Chunk hashing still avoids a temporary ClipboardPayload::Image clone, and local replay state retains one image payload.

The inbound path still reassembles and queues a full BMP payload because the current clipboard apply boundary and Windows clipboard API path require a complete image buffer. A streaming or spooling design should be a separate architecture task if future profiling shows inbound image pressure is unacceptable.
