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

use crate::state::{AppState, OutboundPayload};
use core_input::{InputEvent, InputFrame, KeyState, MouseButton};
use core_protocol::{
    MAX_WIRE_PAYLOAD_BYTES, PROTOCOL_CURRENT, ProtocolVersion, WIRE_FRAME_LENGTH_PREFIX_BYTES,
    WireCodecError, WireInputEvent, WireKeyState, WireMessage, WireMouseButton,
    decode_frame_payload, encode_frame_to_vec,
};
#[cfg(test)]
use peer_transport::{DEFAULT_TRANSPORT_TUNING, OutboundTransferFlow};

mod codec;
mod control;
mod inbound;
mod inbound_payload;
mod outbound;
mod runtime;
mod session;
mod tls;

#[cfg(test)]
use codec::input_events_to_wire;
#[cfg(test)]
use control::{HelloHandling, handle_hello_ack_message, handle_hello_message};
#[cfg(test)]
use inbound::{
    handle_clipboard_image_chunk, handle_clipboard_image_end, handle_clipboard_image_start,
    handle_file_chunk, handle_file_end, handle_file_start,
};
#[cfg(test)]
use outbound::flush_outgoing_payloads;
use runtime::{listener_loop, supervisor_loop};
#[cfg(test)]
use runtime::{outbound_target_candidates, wait_for_reconcile_or_backoff};
#[cfg(test)]
use session::{configure_low_latency_socket, reconnect_requested_for_peer};
use session::{connect_and_run_outbound, handle_incoming_connection};
#[cfg(test)]
use tls::parse_server_name;
use tls::{
    build_tls_acceptor, build_tls_connector, machine_id_from_presented_ca,
    parse_server_name_for_peer,
};

const SUPERVISOR_TICK: Duration = Duration::from_secs(1);
const MAX_BACKOFF_SECONDS: u64 = 30;
const MAX_WIRE_FRAME_BYTES: usize = MAX_WIRE_PAYLOAD_BYTES;
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
        collections::HashMap,
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use chrono::Utc;
    use core_clipboard::{ClipboardPayload, payload_hash_hex};
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
        let input_flush_ms = DEFAULT_TRANSPORT_TUNING
            .outgoing_input_flush_interval
            .as_millis() as f64;
        let bulk_flush_ms = DEFAULT_TRANSPORT_TUNING
            .outgoing_bulk_flush_interval
            .as_millis() as f64;
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
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush
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
    async fn flush_chunks_clipboard_image_that_exceeds_wire_frame_cap() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let image_bmp = vec![0u8; 300 * 1024];
        state
            .requeue_outgoing_front(
                &peer_id,
                vec![OutboundPayload::ClipboardImage {
                    image_bmp: image_bmp.clone(),
                }],
            )
            .await;

        let mut writer = CaptureWriter::default();
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
            "chunked clipboard image must not remain queued after send"
        );

        let frames = decode_written_frames(&writer.bytes);
        assert!(matches!(
            frames.first(),
            Some(WireMessage::ClipboardImageStart {
                machine_id,
                total_bytes,
                hash_hex,
                ..
            }) if machine_id == "local"
                && *total_bytes == image_bmp.len() as u64
                && hash_hex == &payload_hash_hex(&ClipboardPayload::Image(image_bmp.clone()))
        ));
        assert!(matches!(
            frames.last(),
            Some(WireMessage::ClipboardImageEnd { .. })
        ));
        assert!(
            frames
                .iter()
                .filter(|frame| matches!(frame, WireMessage::ClipboardImageChunk { .. }))
                .count()
                >= 2,
            "oversized image should be split across multiple chunk frames"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_requeues_chunked_clipboard_image_on_mid_transfer_failure() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .requeue_outgoing_front(
                &peer_id,
                vec![OutboundPayload::ClipboardImage {
                    image_bmp: vec![0u8; 300 * 1024],
                }],
            )
            .await;

        let mut writer = FailAfterCallsWriter::new(3);
        let _ = flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect_err("chunked clipboard image should requeue on mid-transfer failure");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(
            queued.len(),
            1,
            "failed chunked transfer should requeue the original clipboard image payload"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardImage { image_bmp }) if image_bmp.len() == 300 * 1024
        ));

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
            1,
            "file cursor should remain queued after backpressure defer"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::FileTransferCursor {
                transfer_id: cursor_transfer_id,
            }) if cursor_transfer_id == &transfer_id
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_file_transfer_cursor_sends_one_lazy_chunk_per_credit() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("lazy.bin");
        let payload = vec![7u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 11];
        tokio::fs::write(&file_path, &payload)
            .await
            .expect("write payload");
        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut start_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut start_writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush start");

        let start_frames = decode_written_frames(&start_writer.bytes);
        let transfer_id = match start_frames.first() {
            Some(WireMessage::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected file start, got {other:?}"),
        };
        assert_eq!(start_frames.len(), 1);
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 1);

        outbound_transfer_flow
            .get_mut(&transfer_id)
            .expect("registered outbound flow")
            .available_chunk_credits = 1;

        let mut chunk_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut chunk_writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush first chunk");

        let chunk_frames = decode_written_frames(&chunk_writer.bytes);
        assert_eq!(chunk_frames.len(), 1);
        assert!(matches!(
            chunk_frames.first(),
            Some(WireMessage::FileChunk { transfer_id: chunk_transfer_id, data })
                if chunk_transfer_id == &transfer_id
                    && data.len() == crate::state::FILE_TRANSFER_CHUNK_BYTES
        ));
        assert_eq!(
            outbound_transfer_flow
                .get(&transfer_id)
                .expect("active flow")
                .available_chunk_credits,
            0
        );
        assert_eq!(
            state.outgoing_bulk_queue_len(&peer_id).await,
            1,
            "remaining data should stay represented by one cursor"
        );
        assert_eq!(state.outbound_file_transfer_count().await, 1);

        assert!(
            state
                .cancel_outbound_file_transfer(&peer_id, &transfer_id, "user_cancelled")
                .await
        );
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 0);
        assert_eq!(state.outbound_file_transfer_count().await, 0);
        let cancelled = state
            .transport_events()
            .await
            .into_iter()
            .filter(|event| event.kind == "file_transfer_cancelled")
            .count();
        assert_eq!(cancelled, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_failure_after_file_chunk_retries_same_cursor_bytes() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("chunk-flush-fail.bin");
        let payload = vec![5u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 11];
        tokio::fs::write(&file_path, &payload)
            .await
            .expect("write payload");
        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut start_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut start_writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush start");
        let transfer_id = match decode_written_frames(&start_writer.bytes).first() {
            Some(WireMessage::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected file start, got {other:?}"),
        };

        outbound_transfer_flow
            .get_mut(&transfer_id)
            .expect("registered outbound flow")
            .available_chunk_credits = 1;
        let mut failing_writer = FlushFailWriter::new(1);
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut failing_writer,
            &mut frame_buffer,
        )
        .await
        .expect_err("chunk batch flush should fail");

        assert_eq!(
            outbound_transfer_flow
                .get(&transfer_id)
                .expect("active flow")
                .available_chunk_credits,
            1,
            "failed flush should restore chunk credit for retry"
        );
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 1);
        assert_eq!(state.outbound_file_transfer_count().await, 1);

        let mut retry_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut retry_writer,
            &mut frame_buffer,
        )
        .await
        .expect("retry chunk");

        let retry_frames = decode_written_frames(&retry_writer.bytes);
        assert_eq!(retry_frames.len(), 1);
        assert!(matches!(
            retry_frames.first(),
            Some(WireMessage::FileChunk { transfer_id: chunk_transfer_id, data })
                if chunk_transfer_id == &transfer_id
                    && data.as_slice() == &payload[..crate::state::FILE_TRANSFER_CHUNK_BYTES]
        ));
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 1);
        assert_eq!(state.outbound_file_transfer_count().await, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_failure_after_final_file_chunk_does_not_complete_until_retry() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("final-flush-fail.bin");
        let payload = vec![6u8; 32];
        tokio::fs::write(&file_path, &payload)
            .await
            .expect("write payload");
        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut start_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut start_writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush start");
        let transfer_id = match decode_written_frames(&start_writer.bytes).first() {
            Some(WireMessage::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected file start, got {other:?}"),
        };

        outbound_transfer_flow
            .get_mut(&transfer_id)
            .expect("registered outbound flow")
            .available_chunk_credits = 1;
        let mut failing_writer = FlushFailWriter::new(1);
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut failing_writer,
            &mut frame_buffer,
        )
        .await
        .expect_err("final batch flush should fail");

        assert!(
            outbound_transfer_flow.contains_key(&transfer_id),
            "failed final flush must not remove flow state"
        );
        assert_eq!(
            outbound_transfer_flow
                .get(&transfer_id)
                .expect("active flow")
                .available_chunk_credits,
            1,
            "failed final flush should restore chunk credit for retry"
        );
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 1);
        assert_eq!(state.outbound_file_transfer_count().await, 1);
        assert!(
            state
                .transport_events()
                .await
                .iter()
                .all(|event| event.kind != "file_transfer_completed"),
            "failed final flush must not emit completion"
        );

        let mut retry_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut retry_writer,
            &mut frame_buffer,
        )
        .await
        .expect("retry final chunk");

        let retry_frames = decode_written_frames(&retry_writer.bytes);
        assert_eq!(retry_frames.len(), 2);
        assert!(matches!(
            retry_frames.first(),
            Some(WireMessage::FileChunk { transfer_id: chunk_transfer_id, data })
                if chunk_transfer_id == &transfer_id && data.as_slice() == payload.as_slice()
        ));
        assert!(matches!(
            retry_frames.get(1),
            Some(WireMessage::FileEnd { transfer_id: end_transfer_id })
                if end_transfer_id == &transfer_id
        ));
        assert!(!outbound_transfer_flow.contains_key(&transfer_id));
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 0);
        assert_eq!(state.outbound_file_transfer_count().await, 0);
        let completed = state
            .transport_events()
            .await
            .into_iter()
            .filter(|event| event.kind == "file_transfer_completed")
            .count();
        assert_eq!(completed, 1, "retry should emit one completion event");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_file_transfer_cursor_fails_after_source_mutation() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("mutated.bin");
        tokio::fs::write(
            &file_path,
            vec![7u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 11],
        )
        .await
        .expect("write payload");
        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut start_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut start_writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush start");
        let transfer_id = match decode_written_frames(&start_writer.bytes).first() {
            Some(WireMessage::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected file start, got {other:?}"),
        };

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&file_path)
            .await
            .expect("open payload for mutation");
        file.set_len(3).await.expect("mutate payload length");
        drop(file);

        outbound_transfer_flow
            .get_mut(&transfer_id)
            .expect("registered outbound flow")
            .available_chunk_credits = 1;
        let mut chunk_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut chunk_writer,
            &mut frame_buffer,
        )
        .await
        .expect("mutation should fail transfer without failing session flush");

        assert!(decode_written_frames(&chunk_writer.bytes).is_empty());
        assert!(!outbound_transfer_flow.contains_key(&transfer_id));
        assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 0);
        assert_eq!(state.outbound_file_transfer_count().await, 0);
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "file_transfer_failed"
                && event
                    .detail
                    .contains("source changed after transfer was queued")
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_honors_configured_file_transfer_limit() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let total_bytes = core_transfer::MAX_TRANSFER_BYTES + 1;
        let mut config = state.file_transfer_config().await;
        config.max_file_bytes = total_bytes;
        state
            .update_file_transfer_config(config)
            .await
            .expect("raise file limit");
        state
            .requeue_outgoing_front(
                &peer_id,
                vec![OutboundPayload::FileStart {
                    transfer_id: "large-transfer".to_string(),
                    file_name: "large.bin".to_string(),
                    total_bytes,
                }],
            )
            .await;

        let mut writer = CaptureWriter::default();
        flush_outgoing_payloads(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect("configured limit should allow file start");

        assert!(matches!(
            decode_written_frames(&writer.bytes).first(),
            Some(WireMessage::FileStart {
                transfer_id,
                total_bytes: actual_total,
                ..
            }) if transfer_id == "large-transfer" && *actual_total == total_bytes
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn file_transfer_rejection_clears_flow_and_queued_payloads() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let file_path = root.join("rejected.bin");
        tokio::fs::write(
            &file_path,
            vec![9u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 7],
        )
        .await
        .expect("write payload");

        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");
        let mut queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        let transfer_id = match queued.first() {
            Some(OutboundPayload::FileStart { transfer_id, .. }) => transfer_id.clone(),
            other => panic!("expected first payload to be file start, got {other:?}"),
        };
        queued.remove(0);
        state.requeue_outgoing_front(&peer_id, queued).await;

        let mut outbound_transfer_flow = HashMap::from([(
            transfer_id.clone(),
            OutboundTransferFlow {
                available_chunk_credits: 0,
            },
        )]);

        session::handle_file_transfer_rejected(
            &state,
            Some(&peer_id),
            transfer_id.clone(),
            "receive_policy_denied".to_string(),
            &mut outbound_transfer_flow,
        )
        .await;

        assert!(!outbound_transfer_flow.contains_key(&transfer_id));
        assert!(
            state
                .drain_outgoing_bulk(&peer_id, usize::MAX)
                .await
                .is_empty()
        );
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "file_transfer_rejected"
                && event.detail.contains("reason=receive_policy_denied")
        }));

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

    #[tokio::test]
    async fn configure_low_latency_socket_enables_tcp_nodelay() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener addr");

        let client_task =
            tokio::spawn(async move { TcpStream::connect(address).await.expect("connect client") });
        let (server_stream, _) = listener.accept().await.expect("accept");
        let client_stream = client_task.await.expect("join client task");

        configure_low_latency_socket(&client_stream).expect("configure client TCP_NODELAY");
        configure_low_latency_socket(&server_stream).expect("configure server TCP_NODELAY");

        assert!(
            client_stream.nodelay().expect("client nodelay"),
            "outbound transport socket should enable TCP_NODELAY"
        );
        assert!(
            server_stream.nodelay().expect("server nodelay"),
            "accepted transport socket should enable TCP_NODELAY"
        );
    }

    #[tokio::test]
    async fn discovered_endpoint_wake_interrupts_reconcile_backoff() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let reconcile_wake = state.peer_reconcile_wake_signal();
        let wait_task = tokio::spawn({
            let reconcile_wake = reconcile_wake.clone();
            async move {
                wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(30)).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        state
            .set_discovered_endpoint(
                &peer_id,
                "peer",
                "127.0.0.1:15100".parse().expect("endpoint"),
            )
            .await;

        tokio::time::timeout(Duration::from_millis(200), wait_task)
            .await
            .expect("discovered endpoint wake should interrupt reconcile backoff")
            .expect("wait task should finish cleanly");

        assert!(
            state.transport_events().await.iter().any(|event| {
                event.kind == "peer_reconcile_trigger"
                    && event.detail.contains("source=discovered_endpoint")
            }),
            "discovered endpoint updates should emit an explicit reconcile trigger"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconnect_request_wake_interrupts_reconcile_backoff() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let reconcile_wake = state.peer_reconcile_wake_signal();
        let wait_task = tokio::spawn({
            let reconcile_wake = reconcile_wake.clone();
            async move {
                wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(30)).await;
            }
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        let generation = state.request_peer_reconnect(&peer_id).await;

        tokio::time::timeout(Duration::from_millis(200), wait_task)
            .await
            .expect("reconnect request wake should interrupt reconcile backoff")
            .expect("wait task should finish cleanly");

        assert_eq!(
            generation, 1,
            "first reconnect request should increment generation"
        );
        assert!(
            state.transport_events().await.iter().any(|event| {
                event.kind == "peer_reconcile_trigger"
                    && event.detail.contains("source=peer_reconnect_requested")
            }),
            "explicit reconnect requests should emit an explicit reconcile trigger"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hello_handler_rejects_machine_id_mismatch_with_error_frame() {
        let (state, root) = state_for_listener_test().await;
        let mut remote_protocol = None;
        let mut outbound_transfer_flow = std::collections::HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        let handling = handle_hello_message(
            &state,
            "expected-machine-id",
            None,
            true,
            "local-machine-id",
            "claimed-machine-id".to_string(),
            PROTOCOL_CURRENT,
            &mut remote_protocol,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle hello mismatch");

        assert!(matches!(handling, HelloHandling::TerminateSession));
        assert!(remote_protocol.is_none());

        let frames = decode_written_frames(&writer.bytes);
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            frames.first(),
            Some(WireMessage::Error { message }) if message.contains("hello machine_id mismatch")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hello_handler_accepts_canonical_protocol_and_emits_ack_for_inbound() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut remote_protocol = None;
        let mut outbound_transfer_flow = std::collections::HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        let handling = handle_hello_message(
            &state,
            &peer_id,
            Some(&peer_id),
            false,
            "local-machine-id",
            peer_id.clone(),
            PROTOCOL_CURRENT,
            &mut remote_protocol,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle canonical hello");

        assert!(matches!(handling, HelloHandling::Continue));
        assert_eq!(remote_protocol, Some(PROTOCOL_CURRENT));

        let frames = decode_written_frames(&writer.bytes);
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            frames.first(),
            Some(WireMessage::HelloAck {
                machine_id,
                accepted: true
            }) if machine_id == "local-machine-id"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_hello_flushes_ack_and_pending_clipboard_replay_once() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_local_clipboard_text_for_connected_peers("replay-inbound".to_string())
            .await
            .expect("retain disconnected clipboard snapshot");

        let mut remote_protocol = None;
        let mut outbound_transfer_flow = std::collections::HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        let handling = handle_hello_message(
            &state,
            &peer_id,
            Some(&peer_id),
            false,
            "local-machine-id",
            peer_id.clone(),
            PROTOCOL_CURRENT,
            &mut remote_protocol,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle inbound hello");

        assert!(matches!(handling, HelloHandling::Continue));
        let frames = decode_written_frames(&writer.bytes);
        assert_eq!(
            frames.len(),
            2,
            "inbound hello should flush ack plus one replay"
        );
        assert!(matches!(
            frames.first(),
            Some(WireMessage::HelloAck {
                machine_id,
                accepted: true
            }) if machine_id == "local-machine-id"
        ));
        assert!(matches!(
            frames.get(1),
            Some(WireMessage::ClipboardText { machine_id, text })
                if machine_id == "local-machine-id" && text == "replay-inbound"
        ));
        assert!(
            state.drain_outgoing(&peer_id).await.is_empty(),
            "inbound hello should consume the pending replay exactly once"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hello_ack_handler_flushes_pending_outgoing_payloads() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_clipboard_text(&peer_id, "hello-control".to_string())
            .await
            .expect("queue clipboard text");

        let mut outbound_transfer_flow = std::collections::HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        handle_hello_ack_message(
            &state,
            Some(&peer_id),
            "local-machine-id",
            Some(PROTOCOL_CURRENT),
            &mut outbound_transfer_flow,
            true,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle hello ack");

        let frames = decode_written_frames(&writer.bytes);
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            frames.first(),
            Some(WireMessage::ClipboardText { machine_id, text })
                if machine_id == "local-machine-id" && text == "hello-control"
        ));

        let queued = state.drain_outgoing(&peer_id).await;
        assert!(queued.is_empty(), "payload should be flushed from queue");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn chunked_clipboard_image_transfer_reassembles_and_queues_remote_payload() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let image = minimal_bmp_payload();
        let hash_hex = payload_hash_hex(&ClipboardPayload::Image(image.clone()));
        let mut inbound_transfers = HashMap::new();
        let split = image.len() / 2;

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "clip-1".to_string(),
            image.len() as u64,
            hash_hex,
            &mut inbound_transfers,
        )
        .await
        .expect("start chunked clipboard image transfer");
        handle_clipboard_image_chunk(
            &state,
            "clip-1".to_string(),
            image[..split].to_vec(),
            &mut inbound_transfers,
        )
        .await
        .expect("first chunk");
        handle_clipboard_image_chunk(
            &state,
            "clip-1".to_string(),
            image[split..].to_vec(),
            &mut inbound_transfers,
        )
        .await
        .expect("second chunk");
        handle_clipboard_image_end(&state, "clip-1".to_string(), &mut inbound_transfers)
            .await
            .expect("end chunked clipboard image transfer");

        let queued = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("queued remote clipboard image");
        assert!(matches!(
            queued.payload,
            ClipboardPayload::Image(ref image_bmp) if image_bmp == &image
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn chunked_clipboard_image_transfer_rejects_hash_mismatch() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let image = minimal_bmp_payload();
        let mut inbound_transfers = HashMap::new();

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "clip-bad".to_string(),
            image.len() as u64,
            "deadbeef".to_string(),
            &mut inbound_transfers,
        )
        .await
        .expect("start chunked clipboard image transfer");
        handle_clipboard_image_chunk(
            &state,
            "clip-bad".to_string(),
            image,
            &mut inbound_transfers,
        )
        .await
        .expect("chunk");
        handle_clipboard_image_end(&state, "clip-bad".to_string(), &mut inbound_transfers)
            .await
            .expect("end chunked clipboard image transfer");

        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "hash-mismatched chunked clipboard image should be rejected"
        );
        assert!(
            state.transport_events().await.into_iter().any(|event| {
                event.kind == "transport_transfer_rejected"
                    && event.detail.contains("reason=hash_mismatch")
            }),
            "hash mismatch should be recorded as a transport rejection"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_file_start_respects_default_deny_receive_policy() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut inbound_transfers = HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "file-denied".to_string(),
            "payload.txt".to_string(),
            5,
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("policy denial should not fail the session");

        assert!(inbound_transfers.is_empty());
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "transport_transfer_rejected"
                && event.detail.contains("reason=receive_policy_denied")
        }));
        assert!(matches!(
            decode_written_frames(&writer.bytes).first(),
            Some(WireMessage::FileTransferRejected {
                transfer_id,
                reason,
            }) if transfer_id == "file-denied" && reason == "receive_policy_denied"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_file_start_uses_reserved_receive_dir_part_file() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let receive_dir = root.join("received");
        let mut config = state.file_transfer_config().await;
        config.auto_accept_trusted_peers = true;
        config.receive_dir = receive_dir.display().to_string();
        state
            .update_file_transfer_config(config)
            .await
            .expect("enable auto accept");

        let mut inbound_transfers = HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            r"..\evil".to_string(),
            "payload.txt".to_string(),
            5,
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("accepted start");

        let transfer = inbound_transfers
            .remove(r"..\evil")
            .expect("transfer tracked by wire id");
        let temp_file_name = transfer
            .temp_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temp file name");
        assert_eq!(transfer.final_path, receive_dir.join("payload.txt"));
        assert_eq!(transfer.temp_path.parent(), Some(receive_dir.as_path()));
        assert_eq!(temp_file_name, ".payload.txt.boundless.part");
        assert!(!temp_file_name.contains("evil"));
        assert!(
            !transfer.final_path.exists(),
            "accepted transfer must not expose the final path before completion"
        );

        inbound::discard_inbound_transfer(transfer).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_file_start_credit_flush_error_discards_reserved_part() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let receive_dir = root.join("received");
        let mut config = state.file_transfer_config().await;
        config.auto_accept_trusted_peers = true;
        config.receive_dir = receive_dir.display().to_string();
        state
            .update_file_transfer_config(config)
            .await
            .expect("enable auto accept");

        let mut inbound_transfers = HashMap::new();
        let mut writer = FlushFailWriter::new(1);
        let mut frame_buffer = Vec::with_capacity(256);

        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "file-flush-fails".to_string(),
            "payload.txt".to_string(),
            5,
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect_err("initial chunk-credit flush should fail");

        let reserved_part = receive_dir.join(".payload.txt.boundless.part");
        assert!(
            inbound_transfers.is_empty(),
            "failed initial credit flush should remove the active transfer"
        );
        assert!(
            !reserved_part.exists(),
            "failed initial credit flush must remove reserved inbound .part"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_file_start_allows_explicit_auto_accept_policy() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut config = state.file_transfer_config().await;
        config.auto_accept_trusted_peers = true;
        config.receive_dir = root.join("received").display().to_string();
        state
            .update_file_transfer_config(config)
            .await
            .expect("enable auto accept");

        let mut inbound_transfers = HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "file-accepted".to_string(),
            "payload.txt".to_string(),
            5,
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("accepted start");

        assert!(inbound_transfers.contains_key("file-accepted"));
        assert!(
            !writer.bytes.is_empty(),
            "accepted transfer should receive initial chunk credits"
        );
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "file_transfer_started" && event.detail.contains("file-accepted")
        }));

        for transfer in inbound_transfers.into_values() {
            inbound::discard_inbound_transfer(transfer).await;
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_file_chunks_replenish_credit_at_low_watermark() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let receive_dir = root.join("received");
        let mut config = state.file_transfer_config().await;
        config.auto_accept_trusted_peers = true;
        config.receive_dir = receive_dir.display().to_string();
        state
            .update_file_transfer_config(config)
            .await
            .expect("enable auto accept");

        let mut inbound_transfers = HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "file-credit".to_string(),
            "payload.txt".to_string(),
            6,
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("accepted start");

        assert!(matches!(
            decode_written_frames(&writer.bytes).first(),
            Some(WireMessage::FileChunkCredit {
                transfer_id,
                chunk_credits: 8,
            }) if transfer_id == "file-credit"
        ));
        writer.bytes.clear();

        for index in 0..5 {
            handle_file_chunk(
                &state,
                "file-credit".to_string(),
                vec![b'a' + index as u8],
                &mut inbound_transfers,
                &mut writer,
                &mut frame_buffer,
            )
            .await
            .expect("chunk before low watermark");
            assert!(
                writer.bytes.is_empty(),
                "chunk {} should not emit per-chunk credit",
                index + 1
            );
        }

        handle_file_chunk(
            &state,
            "file-credit".to_string(),
            vec![b'f'],
            &mut inbound_transfers,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("low watermark chunk");

        let credit_frames = decode_written_frames(&writer.bytes);
        assert_eq!(credit_frames.len(), 1);
        assert!(matches!(
            credit_frames.first(),
            Some(WireMessage::FileChunkCredit {
                transfer_id,
                chunk_credits: 6,
            }) if transfer_id == "file-credit"
        ));

        handle_file_end(&state, "file-credit".to_string(), &mut inbound_transfers)
            .await
            .expect("complete transfer");
        assert_eq!(
            std::fs::read(receive_dir.join("payload.txt")).expect("read completed file"),
            b"abcdef"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hello_then_hello_ack_only_flushes_pending_clipboard_replay_once() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_local_clipboard_text_for_connected_peers("replay-once".to_string())
            .await
            .expect("retain disconnected clipboard snapshot");

        let mut remote_protocol = None;
        let mut outbound_transfer_flow = std::collections::HashMap::new();
        let mut hello_writer = CaptureWriter::default();
        let mut frame_buffer = Vec::with_capacity(256);

        let handling = handle_hello_message(
            &state,
            &peer_id,
            Some(&peer_id),
            true,
            "local-machine-id",
            peer_id.clone(),
            PROTOCOL_CURRENT,
            &mut remote_protocol,
            &mut outbound_transfer_flow,
            &mut hello_writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle canonical hello");

        assert!(matches!(handling, HelloHandling::Continue));
        let hello_frames = decode_written_frames(&hello_writer.bytes);
        assert_eq!(
            hello_frames.len(),
            1,
            "outbound hello should flush one replay"
        );
        assert!(matches!(
            hello_frames.first(),
            Some(WireMessage::ClipboardText { machine_id, text })
                if machine_id == "local-machine-id" && text == "replay-once"
        ));

        let mut ack_writer = CaptureWriter::default();
        handle_hello_ack_message(
            &state,
            Some(&peer_id),
            "local-machine-id",
            Some(PROTOCOL_CURRENT),
            &mut outbound_transfer_flow,
            true,
            &mut ack_writer,
            &mut frame_buffer,
        )
        .await
        .expect("handle hello ack");

        let ack_frames = decode_written_frames(&ack_writer.bytes);
        assert!(
            ack_frames.is_empty(),
            "hello ack must not reschedule an already-flushed replay"
        );
        assert!(
            state.drain_outgoing(&peer_id).await.is_empty(),
            "no replay payload should remain queued after hello plus hello ack"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
