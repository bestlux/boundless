use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        ClientConfig, RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
        server::WebPkiClientVerifier,
    },
};
use tracing::{error, info, warn};

use core_input::{InputEvent, InputFrame, KeyState, MouseButton};
use core_protocol::{
    MAX_WIRE_PAYLOAD_BYTES, PROTOCOL_CURRENT, ProtocolVersion, WIRE_FRAME_LENGTH_PREFIX_BYTES,
    WireCodecError, WireInputEvent, WireKeyState, WireMessage, WireMouseButton,
    decode_frame_payload, encode_frame_to_vec,
};
use core_transfer::validate_transfer_size;

use crate::state::{AppState, OutboundPayload};

mod codec;
mod outbound;
mod runtime;
mod session;
mod tls;

#[cfg(test)]
use codec::input_events_to_wire;
#[cfg(test)]
use outbound::flush_outgoing_payloads;
#[cfg(test)]
use runtime::outbound_target_candidates;
use runtime::{listener_loop, supervisor_loop};
#[cfg(test)]
use session::reconnect_requested_for_peer;
use session::{connect_and_run_outbound, handle_incoming_connection};
#[cfg(test)]
use tls::parse_server_name;
use tls::{
    build_tls_acceptor, build_tls_connector, machine_id_from_presented_ca,
    parse_server_name_for_peer,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const OUTGOING_INPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(4);
const OUTGOING_BULK_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
const OUTGOING_BULK_MAX_PAYLOADS_PER_FLUSH: usize = 4;
const SUPERVISOR_TICK: Duration = Duration::from_secs(3);
const MAX_BACKOFF_SECONDS: u64 = 30;
const MAX_WIRE_FRAME_BYTES: usize = MAX_WIRE_PAYLOAD_BYTES;
const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;
const MAX_INBOUND_TRANSFERS_PER_PEER: usize = 4;
const FALLBACK_BIND_HOST: &str = "0.0.0.0";

pub fn start(state: AppState, listener: Option<TcpListener>) {
    if let Some(listener) = listener {
        tokio::spawn(listener_loop(state.clone(), listener));
    } else {
        warn!("transport listener not started");
    }
    tokio::spawn(supervisor_loop(state));
}

pub async fn prepare_listener(state: &AppState) -> Option<TcpListener> {
    let configured_port = state.snapshot().await.network_port;
    let configured_bind = format!("{FALLBACK_BIND_HOST}:{configured_port}");

    match TcpListener::bind(&configured_bind).await {
        Ok(listener) => Some(listener),
        Err(primary_error) => {
            warn!(
                configured_bind = %configured_bind,
                error = %primary_error,
                "configured transport bind failed; trying automatic fallback port"
            );

            let fallback_bind = format!("{FALLBACK_BIND_HOST}:0");
            let listener = match TcpListener::bind(&fallback_bind).await {
                Ok(listener) => listener,
                Err(fallback_error) => {
                    error!(
                        configured_bind = %configured_bind,
                        fallback_bind = %fallback_bind,
                        primary_error = %primary_error,
                        fallback_error = %fallback_error,
                        "transport listener failed to bind on configured and fallback ports"
                    );
                    return None;
                }
            };

            let effective_port = match listener.local_addr() {
                Ok(addr) => addr.port(),
                Err(error) => {
                    error!(
                        error = %error,
                        "transport listener fallback bind succeeded but local_addr failed"
                    );
                    return Some(listener);
                }
            };

            if let Err(error) = state.update_network_port(effective_port).await {
                error!(
                    configured_port,
                    effective_port,
                    error = ?error,
                    "failed to persist effective fallback network port"
                );
            } else {
                warn!(
                    configured_port,
                    effective_port,
                    "transport listener port updated to fallback value and persisted"
                );
            }

            Some(listener)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use chrono::Utc;
    use core_security::{SecurityPaths, TrustRecord, ensure_device_identity};
    use tokio::io::AsyncWrite;

    struct FailAfterCallsWriter {
        calls: usize,
        fail_after_calls: usize,
    }

    impl FailAfterCallsWriter {
        fn new(fail_after_calls: usize) -> Self {
            Self {
                calls: 0,
                fail_after_calls,
            }
        }
    }

    struct FlushFailWriter {
        flush_calls: usize,
        fail_on_flush_call: usize,
    }

    impl FlushFailWriter {
        fn new(fail_on_flush_call: usize) -> Self {
            Self {
                flush_calls: 0,
                fail_on_flush_call,
            }
        }
    }

    impl AsyncWrite for FlushFailWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            self.flush_calls += 1;
            if self.flush_calls >= self.fail_on_flush_call {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced flush failure",
                )));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for FailAfterCallsWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            self.calls += 1;
            if self.calls >= self.fail_after_calls {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced write failure",
                )));
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            self.calls += 1;
            if self.calls >= self.fail_after_calls {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced flush failure",
                )));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct CaptureWriter {
        bytes: Vec<u8>,
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn decode_written_frames(bytes: &[u8]) -> Vec<WireMessage> {
        let mut cursor = bytes;
        let mut frames = Vec::new();
        while !cursor.is_empty() {
            assert!(
                cursor.len() >= WIRE_FRAME_LENGTH_PREFIX_BYTES,
                "frame must include length prefix"
            );
            let payload_len =
                u32::from_be_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]) as usize;
            assert!(
                cursor.len() >= WIRE_FRAME_LENGTH_PREFIX_BYTES + payload_len,
                "buffer must contain full frame payload"
            );
            let payload = &cursor
                [WIRE_FRAME_LENGTH_PREFIX_BYTES..WIRE_FRAME_LENGTH_PREFIX_BYTES + payload_len];
            let frame = decode_frame_payload(payload).expect("decode frame payload");
            frames.push(frame);
            cursor = &cursor[WIRE_FRAME_LENGTH_PREFIX_BYTES + payload_len..];
        }
        frames
    }

    #[test]
    fn extracts_server_name_from_ipv4_socket() {
        let server_name = parse_server_name("127.0.0.1:15100").expect("server name");
        assert_eq!(server_name.to_str(), "127.0.0.1");
    }

    #[test]
    fn extracts_server_name_from_dns() {
        let server_name = parse_server_name("peer.local:15100").expect("server name");
        assert_eq!(server_name.to_str(), "peer.local");
    }

    #[test]
    fn rejects_invalid_server_name() {
        let err = parse_server_name("!").expect_err("must fail");
        assert!(err.to_string().contains("parse server name"));
    }

    #[test]
    fn parse_server_name_for_peer_prefers_peer_id_hint() {
        let server_name =
            parse_server_name_for_peer("peer-machine-id", "192.168.1.7:15100").expect("name");
        assert_eq!(server_name.to_str(), "peer-machine-id");
    }

    #[test]
    fn outbound_target_candidates_prefers_discovered_endpoint_first() {
        let selected = outbound_target_candidates(
            "manual-host:15100",
            Some("10.0.0.7:15100".parse().expect("endpoint")),
        );
        assert_eq!(selected, vec!["10.0.0.7:15100", "manual-host:15100"]);
    }

    #[test]
    fn outbound_target_candidates_falls_back_to_manual_address() {
        let selected = outbound_target_candidates(" manual-host:15100 ", None);
        assert_eq!(selected, vec!["manual-host:15100"]);
    }

    #[test]
    fn maps_presented_ca_to_machine_id() {
        let root =
            std::env::temp_dir().join(format!("boundless-network-test-{}", uuid::Uuid::new_v4()));
        let node1 = SecurityPaths::for_root(root.join("n1"));
        let node2 = SecurityPaths::for_root(root.join("n2"));

        let id1 = ensure_device_identity(&node1, "machine-1", "node1", Some("127.0.0.1"))
            .expect("identity 1");
        let id2 = ensure_device_identity(&node2, "machine-2", "node2", Some("127.0.0.1"))
            .expect("identity 2");

        let records = vec![
            TrustRecord {
                machine_id: "machine-1".to_string(),
                ca_cert_pem: id1.ca_cert_pem,
                added_at: Utc::now(),
            },
            TrustRecord {
                machine_id: "machine-2".to_string(),
                ca_cert_pem: id2.ca_cert_pem.clone(),
                added_at: Utc::now(),
            },
        ];

        let presented = CertificateDer::pem_slice_iter(id2.ca_cert_pem.as_bytes())
            .next()
            .expect("cert present")
            .expect("parse cert");
        let mapped =
            machine_id_from_presented_ca(&records, &presented).expect("mapping must succeed");
        assert_eq!(mapped.as_deref(), Some("machine-2"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn returns_none_for_unknown_presented_ca() {
        let root =
            std::env::temp_dir().join(format!("boundless-network-test-{}", uuid::Uuid::new_v4()));
        let known = SecurityPaths::for_root(root.join("known"));
        let unknown = SecurityPaths::for_root(root.join("unknown"));

        let known_id = ensure_device_identity(&known, "known-id", "known", Some("127.0.0.1"))
            .expect("known identity");
        let unknown_id =
            ensure_device_identity(&unknown, "unknown-id", "unknown", Some("127.0.0.1"))
                .expect("unknown identity");

        let records = vec![TrustRecord {
            machine_id: "known-id".to_string(),
            ca_cert_pem: known_id.ca_cert_pem,
            added_at: Utc::now(),
        }];

        let presented = CertificateDer::pem_slice_iter(unknown_id.ca_cert_pem.as_bytes())
            .next()
            .expect("cert present")
            .expect("parse cert");
        let mapped = machine_id_from_presented_ca(&records, &presented).expect("mapping");
        assert!(mapped.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    async fn state_with_peer_for_queue_test() -> (AppState, String, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("boundless-queue-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state = AppState::load_or_create_with_paths(config_path, security_root).expect("state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join");

        (state, peer_id, root)
    }

    async fn state_for_listener_test() -> (AppState, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("boundless-listener-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        (state, root)
    }

    fn minimal_bmp_payload() -> Vec<u8> {
        vec![
            b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 255, 0,
        ]
    }

    #[test]
    fn perf_probe_outgoing_flush_tick_rate() {
        let input_flush_ms = OUTGOING_INPUT_FLUSH_INTERVAL.as_millis() as f64;
        let bulk_flush_ms = OUTGOING_BULK_FLUSH_INTERVAL.as_millis() as f64;
        let input_theoretical_max_hz = if input_flush_ms > 0.0 {
            1000.0 / input_flush_ms
        } else {
            0.0
        };
        let bulk_theoretical_max_hz = if bulk_flush_ms > 0.0 {
            1000.0 / bulk_flush_ms
        } else {
            0.0
        };
        eprintln!(
            "PERF_PROBE outgoing_flush input_interval_ms={} input_theoretical_max_hz={:.2} bulk_interval_ms={} bulk_theoretical_max_hz={:.2} bulk_max_payloads_per_flush={}",
            input_flush_ms,
            input_theoretical_max_hz,
            bulk_flush_ms,
            bulk_theoretical_max_hz,
            OUTGOING_BULK_MAX_PAYLOADS_PER_FLUSH
        );
        assert!(
            input_flush_ms > 0.0 && bulk_flush_ms > 0.0,
            "flush intervals must be positive"
        );
    }

    #[test]
    fn input_wire_conversion_preserves_all_event_types() {
        let events = vec![
            InputEvent::MouseMoveAbsolute {
                x_norm: 10,
                y_norm: 20,
            },
            InputEvent::MouseMove { dx: 3, dy: -4 },
        ];
        let wire = input_events_to_wire(&events);

        assert_eq!(wire.len(), 2);
        assert!(matches!(
            wire.first(),
            Some(WireInputEvent::MouseMoveAbsolute { x_norm, y_norm })
                if *x_norm == 10 && *y_norm == 20
        ));
        assert!(matches!(
            wire.get(1),
            Some(WireInputEvent::MouseMove { dx, dy }) if *dx == 3 && *dy == -4
        ));
    }

    #[tokio::test]
    async fn flush_requeues_all_payloads_when_first_write_fails() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;

        state
            .queue_clipboard_text(&peer_id, "one".to_string())
            .await
            .expect("queue one");
        state
            .queue_clipboard_text(&peer_id, "two".to_string())
            .await
            .expect("queue two");

        let mut writer = FailAfterCallsWriter::new(1);
        let _err = flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect_err("must fail");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "one"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text == "two"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_requeues_remaining_payloads_on_mid_flush_failure() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let large = "x".repeat(16 * 1024);

        state
            .queue_clipboard_text(&peer_id, large.clone())
            .await
            .expect("queue one");
        state
            .queue_clipboard_text(&peer_id, large.clone())
            .await
            .expect("queue two");
        state
            .queue_clipboard_text(&peer_id, large)
            .await
            .expect("queue three");

        // Oversized lines force direct writes past BufWriter, so this fails on the second payload write.
        let mut writer = FailAfterCallsWriter::new(2);
        let _ = flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect_err("must fail");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardText { text }) if text.len() == 16 * 1024
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text.len() == 16 * 1024
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_requeues_all_payloads_when_batch_flush_fails() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;

        state
            .queue_clipboard_text(&peer_id, "one".to_string())
            .await
            .expect("queue one");
        state
            .queue_clipboard_text(&peer_id, "two".to_string())
            .await
            .expect("queue two");

        // Writes succeed; final flush fails deterministically.
        let mut writer = FlushFailWriter::new(1);
        let _ = flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect_err("must fail");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "one"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text == "two"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_rejects_non_canonical_protocol_version() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_clipboard_image(&peer_id, minimal_bmp_payload())
            .await
            .expect("queue image");

        let mut writer = FailAfterCallsWriter::new(1);
        let error = flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            ProtocolVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            &mut writer,
        )
        .await
        .expect_err("non-canonical protocol should be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported peer protocol for canonical v1"),
            "unexpected error: {error:#}"
        );

        let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        assert_eq!(queued.len(), 1, "payload should remain queued on rejection");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_drops_clipboard_image_that_exceeds_wire_frame_cap() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .requeue_outgoing_front(
                &peer_id,
                vec![OutboundPayload::ClipboardImage {
                    image_bmp: vec![0u8; 300 * 1024],
                }],
            )
            .await;

        let mut writer = FailAfterCallsWriter::new(1);
        flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect("oversized clipboard image should be dropped before write");

        let queued = state.drain_outgoing(&peer_id).await;
        assert!(
            queued.is_empty(),
            "oversized dropped image must not remain queued"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_applies_file_chunk_backpressure_contract() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("flow.bin");
        let payload = vec![9u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 7];
        tokio::fs::write(&file_path, &payload)
            .await
            .expect("write payload");

        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let mut writer = CaptureWriter::default();
        flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect("flush file transfer");

        let frames = decode_written_frames(&writer.bytes);
        assert_eq!(
            frames.len(),
            1,
            "without chunk credit only file-start should be sent"
        );
        let transfer_id = match frames.first() {
            Some(WireMessage::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected first wire frame to be file start, got {other:?}"),
        };

        let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        assert_eq!(
            queued.len(),
            3,
            "two file chunks and file-end should remain queued after backpressure defer"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::FileChunk {
                transfer_id: chunk_transfer_id,
                ..
            }) if chunk_transfer_id == &transfer_id
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::FileChunk {
                transfer_id: chunk_transfer_id,
                ..
            }) if chunk_transfer_id == &transfer_id
        ));
        assert!(matches!(
            queued.get(2),
            Some(OutboundPayload::FileEnd {
                transfer_id: end_transfer_id,
                ..
            }) if end_transfer_id == &transfer_id
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_listener_uses_configured_port_when_available() {
        let (state, root) = state_for_listener_test().await;
        let probe = TcpListener::bind("0.0.0.0:0").await.expect("probe bind");
        let preferred_port = probe.local_addr().expect("probe addr").port();
        drop(probe);

        state
            .update_network_port(preferred_port)
            .await
            .expect("set preferred port");

        let listener = prepare_listener(&state).await.expect("listener");
        let effective_port = listener.local_addr().expect("addr").port();
        assert_eq!(effective_port, preferred_port);
        assert_eq!(state.snapshot().await.network_port, preferred_port);

        drop(listener);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_listener_falls_back_and_persists_effective_port() {
        let (state, root) = state_for_listener_test().await;
        let blocker = TcpListener::bind("0.0.0.0:0").await.expect("block bind");
        let blocked_port = blocker.local_addr().expect("block addr").port();

        state
            .update_network_port(blocked_port)
            .await
            .expect("set blocked port");

        let listener = prepare_listener(&state).await.expect("fallback listener");
        let effective_port = listener.local_addr().expect("addr").port();
        assert_ne!(
            effective_port, blocked_port,
            "fallback must avoid blocked configured port"
        );
        assert_eq!(state.snapshot().await.network_port, effective_port);

        drop(listener);
        drop(blocker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconnect_request_signal_is_edge_triggered_per_generation() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut observed = state.peer_reconnect_generation(&peer_id).await;

        assert!(
            !reconnect_requested_for_peer(&state, &peer_id, &mut observed).await,
            "no reconnect request should be visible initially"
        );

        state.request_peer_reconnect(&peer_id).await;
        assert!(
            reconnect_requested_for_peer(&state, &peer_id, &mut observed).await,
            "new reconnect generation should be observed once"
        );
        assert!(
            !reconnect_requested_for_peer(&state, &peer_id, &mut observed).await,
            "same generation must not retrigger"
        );

        state.request_peer_reconnect(&peer_id).await;
        assert!(
            reconnect_requested_for_peer(&state, &peer_id, &mut observed).await,
            "next generation should retrigger"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
