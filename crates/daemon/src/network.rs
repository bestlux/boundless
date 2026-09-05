#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::{TcpListener, TcpStream},
    sync::oneshot,
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

use crate::{
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::{AppState, OutboundPayload},
};
use core_input::{InputEvent, InputFrame, KeySemantics, KeyState, MouseButton};
use core_protocol::{
    MAX_WIRE_PAYLOAD_BYTES, PROTOCOL_CURRENT, ProtocolVersion, WIRE_FRAME_LENGTH_PREFIX_BYTES,
    WireCodecError, WireInputEvent, WireKeySemantics, WireKeyState, WireMessage, WireMouseButton,
    decode_frame_payload, encode_frame_to_vec,
};
#[cfg(test)]
use peer_transport::{DEFAULT_TRANSPORT_TUNING, OutboundTransferFlow};

mod codec;
mod control;
mod inbound;
mod inbound_payload;
mod outbound;
#[cfg(test)]
mod paired_testing_tests;
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
use session::handle_incoming_connection;
#[cfg(test)]
use session::{
    WireFrameReader, configure_low_latency_socket, reconnect_requested_for_peer,
    run_authenticated_session,
};
#[cfg(test)]
use tls::parse_server_name;
use tls::{
    build_tls_acceptor, build_tls_connector, machine_id_from_presented_ca,
    parse_server_name_for_peer,
};

const SUPERVISOR_TICK: Duration = Duration::from_secs(1);
const OUTBOUND_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_BACKOFF_SECONDS: u64 = 30;
const MAX_WIRE_FRAME_BYTES: usize = MAX_WIRE_PAYLOAD_BYTES;
const LISTEN_BACKLOG: i32 = 1024;
const PORT_ZERO_SPLIT_STACK_RETRIES: usize = 8;

// File handles and Windows user authority belong to the runtime adapter,
// rather than the shared peer-transport policy crate.
#[derive(Debug)]
struct InboundTransfer {
    peer_id: String,
    file_name: String,
    total_bytes: u64,
    bytes_received: u64,
    remaining_chunk_credits: u32,
    final_path: std::path::PathBuf,
    temp_path: std::path::PathBuf,
    temp_file: tokio::fs::File,
    user_io: platform_windows::user_io::UserIoLease,
}

pub fn start(state: AppState, listeners: Vec<TcpListener>) {
    if !listeners.is_empty() {
        let listener_state = state.clone();
        state.spawn_runtime_task(
            RuntimeTaskSpec::new(
                "network.listener",
                RuntimeTaskOwner::Network,
                RuntimeTaskShutdown::AbortOnDaemonShutdown,
            ),
            listener_loop(listener_state, listeners),
        );
    } else {
        warn!("transport listener not started");
    }
    let supervisor_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "network.supervisor",
            RuntimeTaskOwner::Network,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        supervisor_loop(supervisor_state),
    );
}

pub async fn prepare_listener(state: &AppState) -> Vec<TcpListener> {
    let configured_port = state.snapshot().await.network_port;

    match bind_dual_stack_tcp_listeners(configured_port) {
        Ok(listeners) => listeners,
        Err(primary_error) => {
            let configured_bind = format!("dual-stack-any:{configured_port}");
            warn!(
                configured_bind = %configured_bind,
                error = %primary_error,
                "configured transport bind failed; trying automatic fallback port"
            );

            let listeners = match bind_dual_stack_tcp_listeners(0) {
                Ok(listeners) => listeners,
                Err(fallback_error) => {
                    let fallback_bind = "dual-stack-any:0";
                    error!(
                        configured_bind = %configured_bind,
                        fallback_bind = %fallback_bind,
                        primary_error = %primary_error,
                        fallback_error = %fallback_error,
                        "transport listener failed to bind on configured and fallback ports"
                    );
                    return Vec::new();
                }
            };

            let effective_port = match listeners
                .first()
                .and_then(|listener| listener.local_addr().ok())
            {
                Some(addr) => addr.port(),
                None => {
                    error!("transport listener fallback bind succeeded but local_addr failed");
                    return listeners;
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

            listeners
        }
    }
}

pub(crate) fn bind_dual_stack_tcp_listeners(port: u16) -> Result<Vec<TcpListener>> {
    match bind_dual_stack_listener(port) {
        Ok(listener) => return Ok(vec![listener]),
        Err(dual_stack_error) => {
            warn!(
                port,
                error = %dual_stack_error,
                "dual-stack TCP listener bind failed; trying separate IPv6/IPv4 listeners"
            );
        }
    }

    let attempts = if port == 0 {
        PORT_ZERO_SPLIT_STACK_RETRIES
    } else {
        1
    };
    let mut last_addr_in_use_error = None;
    for attempt in 1..=attempts {
        match bind_split_stack_tcp_listeners(port) {
            Ok(listeners) => return Ok(listeners),
            Err(error) if port == 0 && error_chain_has_addr_in_use(&error) => {
                warn!(
                    attempt,
                    attempts,
                    error = %error,
                    "split-stack TCP listener hit random port collision; retrying"
                );
                last_addr_in_use_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_addr_in_use_error
        .expect("port zero split-stack retry loop should record an address-in-use error"))
}

fn bind_split_stack_tcp_listeners(port: u16) -> Result<Vec<TcpListener>> {
    let mut listeners = Vec::new();
    let mut target_port = port;
    match bind_single_stack_listener(
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        true,
    ) {
        Ok(listener) => {
            target_port = listener
                .local_addr()
                .context("read IPv6 listener port")?
                .port();
            listeners.push(listener);
        }
        Err(error) => {
            if error.kind() == io::ErrorKind::AddrInUse {
                return Err(error).context("bind IPv6 fallback listener");
            }
            warn!(port, error = %error, "IPv6-only TCP listener bind failed");
        }
    }

    let v4_port = target_port;
    match bind_single_stack_listener(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), v4_port),
        false,
    ) {
        Ok(listener) => listeners.push(listener),
        Err(error) => {
            if error.kind() == io::ErrorKind::AddrInUse {
                return Err(error).context("bind IPv4 fallback listener");
            }
            if listeners.is_empty() {
                return Err(error).context("bind IPv4 fallback listener");
            }
            warn!(
                port = v4_port,
                error = %error,
                "IPv4 TCP listener bind failed after IPv6-only listener succeeded"
            );
        }
    }

    if listeners.is_empty() {
        bail!("no TCP listeners could be bound");
    }
    Ok(listeners)
}

fn error_chain_has_addr_in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_error| io_error.kind() == io::ErrorKind::AddrInUse)
    })
}

fn bind_dual_stack_listener(port: u16) -> io::Result<TcpListener> {
    bind_socket2_listener(
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        Some(false),
    )
}

fn bind_single_stack_listener(addr: SocketAddr, only_v6: bool) -> io::Result<TcpListener> {
    bind_socket2_listener(addr, addr.is_ipv6().then_some(only_v6))
}

fn bind_socket2_listener(addr: SocketAddr, only_v6: Option<bool>) -> io::Result<TcpListener> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    if let Some(only_v6) = only_v6 {
        socket.set_only_v6(only_v6)?;
    }
    set_exclusive_addr_use(&socket)?;
    socket.bind(&socket2::SockAddr::from(addr))?;
    socket.listen(LISTEN_BACKLOG)?;
    let listener: std::net::TcpListener = socket.into();
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

#[cfg(windows)]
fn set_exclusive_addr_use(socket: &socket2::Socket) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, setsockopt,
    };

    let value: i32 = 1;
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const value).cast(),
            std::mem::size_of_val(&value) as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_exclusive_addr_use(_socket: &socket2::Socket) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        io,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    use super::*;
    use chrono::Utc;
    use core_clipboard::{ClipboardPayload, payload_hash_hex};
    use core_security::{SecurityPaths, TrustRecord, ensure_device_identity};
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

    #[derive(Clone, Default)]
    struct TestLogCapture(Arc<Mutex<Vec<u8>>>);

    struct TestLogWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for TestLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture log lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for TestLogCapture {
        type Writer = TestLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            TestLogWriter(self.0.clone())
        }
    }

    impl TestLogCapture {
        fn rendered(&self) -> String {
            String::from_utf8(self.0.lock().expect("read captured logs").clone())
                .expect("captured logs are utf-8")
        }
    }

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

    struct PartialWriteBlockingWriter {
        bytes: Vec<u8>,
        entered_blocked_write: Arc<tokio::sync::Notify>,
        wrote_prefix: bool,
    }

    impl PartialWriteBlockingWriter {
        fn new() -> (Self, Arc<tokio::sync::Notify>) {
            let entered_blocked_write = Arc::new(tokio::sync::Notify::new());
            (
                Self {
                    bytes: Vec::new(),
                    entered_blocked_write: entered_blocked_write.clone(),
                    wrote_prefix: false,
                },
                entered_blocked_write,
            )
        }
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

    impl AsyncWrite for PartialWriteBlockingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            if self.wrote_prefix {
                return Poll::Pending;
            }
            let written = (buf.len() / 2).max(1);
            self.bytes.extend_from_slice(&buf[..written]);
            self.wrote_prefix = true;
            self.entered_blocked_write.notify_one();
            Poll::Ready(Ok(written))
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

    async fn read_framed_message<R>(reader: &mut R) -> WireMessage
    where
        R: AsyncRead + Unpin,
    {
        let mut length_prefix = [0u8; WIRE_FRAME_LENGTH_PREFIX_BYTES];
        reader
            .read_exact(&mut length_prefix)
            .await
            .expect("read frame length");
        let payload_len = u32::from_be_bytes(length_prefix) as usize;
        let mut payload = vec![0; payload_len];
        reader
            .read_exact(&mut payload)
            .await
            .expect("read frame payload");
        decode_frame_payload(&payload).expect("decode frame payload")
    }

    #[derive(Default)]
    struct FaultSwitches {
        fail_next_read: AtomicBool,
        fail_next_write: AtomicBool,
        fail_next_flush: AtomicBool,
    }

    struct FaultInjectedStream {
        inner: DuplexStream,
        faults: Arc<FaultSwitches>,
    }

    struct FaultRemotePeer {
        inner: DuplexStream,
        faults: Arc<FaultSwitches>,
        delayed_frames: VecDeque<WireMessage>,
        frame_buffer: Vec<u8>,
    }

    struct TransportFaultHarness;

    impl TransportFaultHarness {
        fn pair() -> (FaultInjectedStream, FaultRemotePeer) {
            let (session_side, remote_side) = tokio::io::duplex(64 * 1024);
            let faults = Arc::new(FaultSwitches::default());
            (
                FaultInjectedStream {
                    inner: session_side,
                    faults: Arc::clone(&faults),
                },
                FaultRemotePeer {
                    inner: remote_side,
                    faults,
                    delayed_frames: VecDeque::new(),
                    frame_buffer: Vec::with_capacity(4096),
                },
            )
        }

        fn reconnect_pair() -> (FaultInjectedStream, FaultRemotePeer) {
            Self::pair()
        }
    }

    impl FaultRemotePeer {
        fn fail_next_read(&self) {
            self.faults.fail_next_read.store(true, Ordering::SeqCst);
        }

        fn fail_next_write(&self) {
            self.faults.fail_next_write.store(true, Ordering::SeqCst);
        }

        fn fail_next_flush(&self) {
            self.faults.fail_next_flush.store(true, Ordering::SeqCst);
        }

        async fn send_frame(&mut self, message: WireMessage) {
            encode_frame_to_vec(&message, &mut self.frame_buffer).expect("encode frame");
            self.inner
                .write_all(&self.frame_buffer)
                .await
                .expect("write remote frame");
            self.inner.flush().await.expect("flush remote frame");
        }

        fn queue_delayed_frame(&mut self, message: WireMessage) {
            self.delayed_frames.push_back(message);
        }

        async fn release_delayed_frame(&mut self) {
            let message = self
                .delayed_frames
                .pop_front()
                .expect("delayed frame queued");
            self.send_frame(message).await;
        }

        async fn read_frame(&mut self) -> WireMessage {
            read_framed_message(&mut self.inner).await
        }

        async fn read_until<F>(&mut self, description: &str, mut predicate: F) -> WireMessage
        where
            F: FnMut(&WireMessage) -> bool,
        {
            let mut frames = Vec::new();
            for _ in 0..16 {
                let frame = time::timeout(Duration::from_secs(1), self.read_frame())
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
                if predicate(&frame) {
                    return frame;
                }
                frames.push(frame);
            }
            panic!("did not observe {description}; saw {frames:?}");
        }

        async fn disconnect(&mut self) {
            self.inner.shutdown().await.expect("remote disconnect");
        }
    }

    impl AsyncRead for FaultInjectedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.faults.fail_next_read.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced read failure",
                )));
            }
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FaultInjectedStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.faults.fail_next_write.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced write failure",
                )));
            }
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.faults.fail_next_flush.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "forced flush failure",
                )));
            }
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
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
            &[
                "[2001:db8::7]:15100".parse().expect("ipv6 endpoint"),
                "10.0.0.7:15100".parse().expect("ipv4 endpoint"),
            ],
        );
        assert_eq!(
            selected,
            vec!["[2001:db8::7]:15100", "10.0.0.7:15100", "manual-host:15100"]
        );
    }

    #[test]
    fn outbound_target_candidates_falls_back_to_manual_address() {
        let selected = outbound_target_candidates(" manual-host:15100 ", &[]);
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

    fn state_with_ordered_peer_for_queue_test(
        local_machine_id: &str,
        peer_id: &str,
    ) -> (AppState, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "boundless-ordered-queue-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let mut config = crate::config::RuntimeConfig {
            machine_id: local_machine_id.to_string(),
            ..Default::default()
        };
        config.peers.push(crate::config::PeerConfig {
            peer_id: peer_id.to_string(),
            display_name: "peer".to_string(),
            address: "127.0.0.1:15100".to_string(),
            connected: false,
            last_seen: Utc::now(),
        });
        crate::config::save_config_at(&config_path, &config).expect("seed ordered config");
        let state = AppState::load_or_create_with_paths(config_path, security_root).expect("state");
        (state, peer_id.to_string(), root)
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

    fn synthetic_bmp_payload(size_bytes: usize) -> Vec<u8> {
        const BMP_FILE_HEADER_BYTES: usize = 14;
        const BMP_INFO_HEADER_BYTES: usize = 40;
        const BMP_PIXEL_OFFSET: usize = BMP_FILE_HEADER_BYTES + BMP_INFO_HEADER_BYTES;
        assert!(
            size_bytes > BMP_PIXEL_OFFSET,
            "synthetic BMP must leave room for pixel data"
        );
        assert!(
            u32::try_from(size_bytes).is_ok(),
            "synthetic BMP size must fit the BMP file-size header"
        );

        let pixel_bytes = size_bytes - BMP_PIXEL_OFFSET;
        let mut payload = vec![0u8; size_bytes];
        payload[0] = b'B';
        payload[1] = b'M';
        payload[2..6].copy_from_slice(&(size_bytes as u32).to_le_bytes());
        payload[10..14].copy_from_slice(&(BMP_PIXEL_OFFSET as u32).to_le_bytes());
        payload[14..18].copy_from_slice(&(BMP_INFO_HEADER_BYTES as u32).to_le_bytes());
        payload[18..22].copy_from_slice(&1u32.to_le_bytes());
        payload[22..26].copy_from_slice(&1u32.to_le_bytes());
        payload[26..28].copy_from_slice(&1u16.to_le_bytes());
        payload[28..30].copy_from_slice(&24u16.to_le_bytes());
        payload[34..38].copy_from_slice(&(pixel_bytes as u32).to_le_bytes());
        payload[BMP_PIXEL_OFFSET] = 0x7f;
        payload
    }

    fn clipboard_image_hash_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update([0x02]);
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    async fn flush_profile_clipboard_image<W>(state: &AppState, peer_id: &str, writer: &mut W)
    where
        W: AsyncWrite + Unpin,
    {
        let mut outbound_transfer_flow = peer_transport::OutboundTransferFlows::new();
        let mut frame_buffer = Vec::with_capacity(4096);
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            state,
            "local",
            Some(peer_id),
            PROTOCOL_CURRENT,
            usize::MAX,
            &mut outbound_transfer_flow,
            writer,
            &mut frame_buffer,
        )
        .await
        .expect("start outbound clipboard image profile transfer");

        while state.outgoing_bulk_queue_len(peer_id).await > 0 {
            let transfer_id = outbound_transfer_flow
                .keys()
                .next()
                .cloned()
                .expect("chunked clipboard profile transfer must retain flow state");
            peer_transport::apply_outbound_chunk_credits(
                &mut outbound_transfer_flow,
                &transfer_id,
                1,
            )
            .expect("credit active outbound clipboard profile transfer");
            super::outbound::flush_outgoing_bulk_payloads_with_buffer(
                state,
                "local",
                Some(peer_id),
                PROTOCOL_CURRENT,
                usize::MAX,
                &mut outbound_transfer_flow,
                writer,
                &mut frame_buffer,
            )
            .await
            .expect("flush credited outbound clipboard profile chunk");
        }
    }

    #[tokio::test]
    #[ignore = "run with scripts/dev/profile-clipboard-image-memory.ps1"]
    async fn clipboard_image_memory_profile_workload() {
        let scenario = std::env::var("BOUNDLESS_CLIPBOARD_IMAGE_PROFILE_SCENARIO")
            .unwrap_or_else(|_| "local-outbound".to_string());
        let size_bytes = std::env::var("BOUNDLESS_CLIPBOARD_IMAGE_PROFILE_SIZE_BYTES")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(2 * 1024 * 1024);

        match scenario.as_str() {
            "noop" => {
                let (_state, _peer_id, root) = state_with_peer_for_queue_test().await;
                let _ = std::fs::remove_dir_all(root);
            }
            "direct-outbound" => {
                let (state, peer_id, root) = state_with_peer_for_queue_test().await;
                state
                    .queue_clipboard_image(&peer_id, synthetic_bmp_payload(size_bytes))
                    .await
                    .expect("queue direct clipboard image");
                let mut writer = tokio::io::sink();
                flush_profile_clipboard_image(&state, &peer_id, &mut writer).await;
                assert!(
                    state.drain_outgoing(&peer_id).await.is_empty(),
                    "direct outbound profile must drain queued payloads"
                );
                let _ = std::fs::remove_dir_all(root);
            }
            "local-outbound" => {
                let (state, peer_id, root) = state_with_peer_for_queue_test().await;
                state
                    .set_peer_connected(&peer_id, true)
                    .await
                    .expect("connect peer for local clipboard replay");
                let queued = state
                    .queue_local_clipboard_image_for_connected_peers(synthetic_bmp_payload(
                        size_bytes,
                    ))
                    .await
                    .expect("queue local clipboard image");
                assert!(queued, "local image should queue for connected peer");
                let mut writer = tokio::io::sink();
                flush_profile_clipboard_image(&state, &peer_id, &mut writer).await;
                assert!(
                    state.drain_outgoing(&peer_id).await.is_empty(),
                    "local outbound profile must drain queued payloads"
                );
                let _ = std::fs::remove_dir_all(root);
            }
            "inbound-chunked" => {
                let (state, peer_id, root) = state_with_peer_for_queue_test().await;
                let image = synthetic_bmp_payload(size_bytes);
                let hash_hex = clipboard_image_hash_hex(&image);
                let mut inbound_transfers = HashMap::new();
                handle_clipboard_image_start(
                    &state,
                    &peer_id,
                    Some(&peer_id),
                    peer_id.clone(),
                    "profile-clipboard-image".to_string(),
                    image.len() as u64,
                    hash_hex,
                    &mut inbound_transfers,
                )
                .await
                .expect("start inbound clipboard image profile transfer");
                for chunk in image.chunks(peer_transport::CLIPBOARD_IMAGE_CHUNK_BYTES) {
                    handle_clipboard_image_chunk(
                        &state,
                        "profile-clipboard-image".to_string(),
                        chunk.to_vec(),
                        &mut inbound_transfers,
                    )
                    .await
                    .expect("append inbound clipboard image profile chunk");
                }
                handle_clipboard_image_end(
                    &state,
                    "profile-clipboard-image".to_string(),
                    &mut inbound_transfers,
                )
                .await
                .expect("finish inbound clipboard image profile transfer");
                assert!(
                    state.dequeue_remote_clipboard_payload().await.is_some(),
                    "inbound profile must enqueue remote clipboard image"
                );
                let _ = std::fs::remove_dir_all(root);
            }
            other => panic!("unknown clipboard image memory profile scenario: {other}"),
        }

        println!("clipboard_image_memory_profile scenario={scenario} size_bytes={size_bytes}");
    }

    fn remote_hello(peer_id: &str) -> WireMessage {
        WireMessage::Hello {
            machine_id: peer_id.to_string(),
            display_name: "peer".to_string(),
            protocol: PROTOCOL_CURRENT,
            capability_count: core_protocol::default_capabilities().len(),
        }
    }

    async fn assert_pre_hello_message_is_rejected(message: WireMessage) {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let (stream, mut remote) = TransportFaultHarness::pair();
        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            true,
            None,
        ));

        remote
            .read_until("local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        remote.send_frame(message).await;

        let rejection = remote
            .read_until("pre-Hello protocol rejection", |frame| {
                matches!(frame, WireMessage::Error { .. })
            })
            .await;
        assert!(matches!(
            rejection,
            WireMessage::Error { message }
                if message.contains("protocol not negotiated")
                    && message.contains("initial Hello required")
        ));
        session
            .await
            .expect("session task joins")
            .expect("protocol rejection closes session cleanly");

        assert_eq!(
            state.pending_inject_input_frame_count().await,
            0,
            "pre-Hello payload must not reach input injection"
        );
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "transport_frame_rejected"
                && event.peer_id == peer_id
                && event.detail.contains("reason=protocol_not_negotiated")
                && event.detail.contains("expected=initial_hello")
        }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pre_hello_input_frame_is_rejected_before_dispatch() {
        assert_pre_hello_message_is_rejected(WireMessage::InputFrame {
            machine_id: "remote-machine".to_string(),
            sequence: 1,
            timestamp_unix_ms: Utc::now().timestamp_millis(),
            events: vec![WireInputEvent::MouseMove { dx: 7, dy: -3 }],
        })
        .await;
    }

    #[tokio::test]
    async fn pre_hello_ack_is_rejected_before_dispatch() {
        assert_pre_hello_message_is_rejected(WireMessage::HelloAck {
            machine_id: "remote-machine".to_string(),
            accepted: true,
        })
        .await;
    }

    #[tokio::test]
    async fn fault_harness_injects_read_write_flush_and_disconnect() {
        let (mut stream, mut remote) = TransportFaultHarness::pair();

        remote.fail_next_write();
        let write_error = stream.write_all(b"hello").await.expect_err("write fails");
        assert_eq!(write_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(write_error.to_string().contains("forced write failure"));

        remote.fail_next_flush();
        let flush_error = stream.flush().await.expect_err("flush fails");
        assert_eq!(flush_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(flush_error.to_string().contains("forced flush failure"));

        remote.fail_next_read();
        let mut byte = [0];
        let read_error = stream.read_exact(&mut byte).await.expect_err("read fails");
        assert_eq!(read_error.kind(), io::ErrorKind::BrokenPipe);
        assert!(read_error.to_string().contains("forced read failure"));

        remote.disconnect().await;
        let eof = stream.read(&mut byte).await.expect("read eof");
        assert_eq!(eof, 0, "remote shutdown should surface as disconnect EOF");
    }

    #[tokio::test]
    async fn wire_frame_reader_resumes_after_header_and_payload_read_cancellation() {
        let first = WireMessage::ClipboardText {
            machine_id: "peer-a".to_string(),
            text: "partial-frame".to_string(),
        };
        let second = WireMessage::Heartbeat {
            machine_id: "peer-a".to_string(),
            timestamp_unix_ms: 42,
        };
        let mut first_frame = Vec::new();
        let mut second_frame = Vec::new();
        encode_frame_to_vec(&first, &mut first_frame).expect("encode first frame");
        encode_frame_to_vec(&second, &mut second_frame).expect("encode second frame");

        let (mut sender, receiver) = tokio::io::duplex(1024);
        let mut receiver = BufReader::new(receiver);
        let mut frame_reader = WireFrameReader::default();

        sender
            .write_all(&first_frame[..2])
            .await
            .expect("write partial header");
        assert!(
            time::timeout(
                Duration::from_millis(20),
                frame_reader.read_next(&mut receiver)
            )
            .await
            .is_err(),
            "read should be cancelled while waiting for the rest of the header"
        );

        let partial_payload_end = WIRE_FRAME_LENGTH_PREFIX_BYTES + 3;
        sender
            .write_all(&first_frame[2..partial_payload_end])
            .await
            .expect("write rest of header and partial payload");
        assert!(
            time::timeout(
                Duration::from_millis(20),
                frame_reader.read_next(&mut receiver)
            )
            .await
            .is_err(),
            "read should be cancelled while waiting for the rest of the payload"
        );

        sender
            .write_all(&first_frame[partial_payload_end..])
            .await
            .expect("finish first frame");
        sender
            .write_all(&second_frame)
            .await
            .expect("write following frame");

        let first_len = frame_reader
            .read_next(&mut receiver)
            .await
            .expect("resume first frame")
            .expect("first frame available");
        assert_eq!(
            first_len,
            first_frame.len() - WIRE_FRAME_LENGTH_PREFIX_BYTES
        );
        assert_eq!(
            decode_frame_payload(frame_reader.payload()).expect("decode resumed first frame"),
            first
        );

        frame_reader
            .read_next(&mut receiver)
            .await
            .expect("read second frame")
            .expect("second frame available");
        assert_eq!(
            decode_frame_payload(frame_reader.payload()).expect("decode following frame"),
            second
        );
    }

    #[tokio::test]
    async fn fault_harness_delayed_hello_flushes_clipboard_replay_then_disconnects() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_clipboard_text(&peer_id, "delayed-clipboard".to_string())
            .await
            .expect("queue clipboard replay");
        let (stream, mut remote) = TransportFaultHarness::pair();

        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            true,
            None,
        ));

        assert!(matches!(
            remote
                .read_until("local hello", |frame| matches!(
                    frame,
                    WireMessage::Hello { .. }
                ))
                .await,
            WireMessage::Hello { .. }
        ));

        remote.queue_delayed_frame(remote_hello(&peer_id));
        remote.release_delayed_frame().await;
        remote
            .send_frame(WireMessage::HelloAck {
                machine_id: peer_id.clone(),
                accepted: true,
            })
            .await;

        let local_machine_id = state.snapshot().await.machine_id;
        let replay = remote
            .read_until("clipboard replay", |frame| {
                matches!(frame, WireMessage::ClipboardText { text, .. } if text == "delayed-clipboard")
            })
            .await;
        assert!(matches!(
            replay,
            WireMessage::ClipboardText { machine_id, text }
                if machine_id == local_machine_id && text == "delayed-clipboard"
        ));

        remote.disconnect().await;
        session
            .await
            .expect("session task joins")
            .expect("disconnect closes session cleanly");
        assert!(
            !state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "session close should mark the peer disconnected"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn authenticated_clipboard_restart_gets_fresh_credit_and_newer_payloads_retire_it() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let (stream, mut remote) = TransportFaultHarness::pair();
        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            true,
            None,
        ));
        remote
            .read_until("local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        remote.send_frame(remote_hello(&peer_id)).await;
        remote
            .send_frame(WireMessage::HelloAck {
                machine_id: peer_id.clone(),
                accepted: true,
            })
            .await;

        let image_one = synthetic_bmp_payload(16 * 1024);
        let transfer_id = "same-id-restart".to_string();
        let start = WireMessage::ClipboardImageStart {
            machine_id: peer_id.clone(),
            transfer_id: transfer_id.clone(),
            total_bytes: image_one.len() as u64,
            hash_hex: clipboard_image_hash_hex(&image_one),
        };
        remote.send_frame(start.clone()).await;
        remote
            .read_until("initial clipboard credit", |frame| {
                matches!(
                    frame,
                    WireMessage::ClipboardImageChunkCredit {
                        transfer_id: credited,
                        chunk_credits: 1,
                    } if credited == &transfer_id
                )
            })
            .await;
        remote.send_frame(start).await;
        remote
            .read_until("same-id restart clipboard credit", |frame| {
                matches!(
                    frame,
                    WireMessage::ClipboardImageChunkCredit {
                        transfer_id: credited,
                        chunk_credits: 1,
                    } if credited == &transfer_id
                )
            })
            .await;

        remote
            .send_frame(WireMessage::ClipboardText {
                machine_id: peer_id.clone(),
                text: "newer-text".to_string(),
            })
            .await;

        let transfer_two = "inline-retired-transfer".to_string();
        remote
            .send_frame(WireMessage::ClipboardImageStart {
                machine_id: peer_id.clone(),
                transfer_id: transfer_two.clone(),
                total_bytes: image_one.len() as u64,
                hash_hex: clipboard_image_hash_hex(&image_one),
            })
            .await;
        remote
            .read_until("post-text clipboard credit barrier", |frame| {
                matches!(
                    frame,
                    WireMessage::ClipboardImageChunkCredit {
                        transfer_id: credited,
                        chunk_credits: 1,
                    } if credited == &transfer_two
                )
            })
            .await;
        let text = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("newer text should be queued");
        assert!(matches!(
            text.payload,
            ClipboardPayload::Text(ref value) if value == "newer-text"
        ));
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "text supersession should queue only the latest payload"
        );
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "clipboard_image_superseded"
                && event.detail == "payload_type=bmp disposition=superseded reason=clipboard_text"
        }));

        let mut inline_image = minimal_bmp_payload();
        inline_image[54] = 0x33;
        remote
            .send_frame(WireMessage::ClipboardImage {
                machine_id: peer_id.clone(),
                data: inline_image.clone(),
            })
            .await;

        let barrier_transfer_id = "inline-barrier".to_string();
        remote
            .send_frame(WireMessage::ClipboardImageStart {
                machine_id: peer_id.clone(),
                transfer_id: barrier_transfer_id.clone(),
                total_bytes: 64,
                hash_hex: "barrier-hash".to_string(),
            })
            .await;
        remote
            .read_until("post-inline clipboard credit barrier", |frame| {
                matches!(
                    frame,
                    WireMessage::ClipboardImageChunkCredit {
                        transfer_id: credited,
                        chunk_credits: 1,
                    } if credited == &barrier_transfer_id
                )
            })
            .await;
        let inline = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("newer inline image should be queued");
        assert!(matches!(
            inline.payload,
            ClipboardPayload::Image(ref image) if image == &inline_image
        ));
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "inline image supersession should queue only the latest payload"
        );
        assert!(state.transport_events().await.iter().any(|event| {
            event.kind == "clipboard_image_superseded"
                && event.detail
                    == "payload_type=bmp disposition=superseded reason=clipboard_image_inline"
        }));

        remote.disconnect().await;
        session
            .await
            .expect("session task joins")
            .expect("disconnect closes session cleanly");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn authenticated_duplicate_session_is_recorded_and_rejected() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let (first_stream, mut first_remote) = TransportFaultHarness::pair();

        let first_session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            first_stream,
            false,
            None,
        ));
        first_remote
            .read_until("first local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        assert!(
            state.has_active_transport_session(&peer_id),
            "first authenticated session should own the peer"
        );

        let (duplicate_stream, _duplicate_remote) = TransportFaultHarness::pair();
        run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            duplicate_stream,
            false,
            None,
        )
        .await
        .expect("duplicate session should be closed without failing caller");

        let events = state.transport_events().await;
        assert!(events.iter().any(|event| {
            event.kind == "transport_session_authenticated"
                && event.peer_id == peer_id
                && event.detail.contains("transport=reverse_initiated")
                && event.detail.contains("ownership=claimed")
        }));
        assert!(events.iter().any(|event| {
            event.kind == "transport_session_duplicate"
                && event.peer_id == peer_id
                && event.detail.contains("transport=reverse_initiated")
                && event.detail.contains("ownership=duplicate")
        }));

        first_remote.disconnect().await;
        first_session
            .await
            .expect("first session task joins")
            .expect("disconnect closes first session cleanly");
        assert!(
            !state.has_active_transport_session(&peer_id),
            "first session exit should clear active ownership"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reverse_initiated_session_flushes_input_queued_after_hello() {
        let (state, peer_id, root) = state_with_ordered_peer_for_queue_test("z-local", "a-peer");
        let (stream, mut remote) = TransportFaultHarness::pair();

        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            false,
            None,
        ));
        remote
            .read_until("reverse session local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        remote.send_frame(remote_hello(&peer_id)).await;
        remote
            .read_until("reverse session hello ack", |frame| {
                matches!(frame, WireMessage::HelloAck { accepted: true, .. })
            })
            .await;

        let (duplicate_stream, _duplicate_remote) = TransportFaultHarness::pair();
        run_authenticated_session(state.clone(), peer_id.clone(), duplicate_stream, true, None)
            .await
            .expect(
                "duplicate outbound session should be rejected without disturbing reverse owner",
            );

        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 3, dy: 2 }])
            .await
            .expect("queue input after reverse session negotiation");
        let frame = remote
            .read_until("input frame on reverse session", |frame| {
                matches!(frame, WireMessage::InputFrame { .. })
            })
            .await;
        assert!(matches!(
            frame,
            WireMessage::InputFrame { events, .. }
                if matches!(events.as_slice(), [WireInputEvent::MouseMove { dx: 3, dy: 2 }])
        ));

        remote.disconnect().await;
        time::timeout(Duration::from_secs(1), session)
            .await
            .expect("reverse session exits promptly")
            .expect("reverse session task joins")
            .expect("disconnect closes reverse session cleanly");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn simultaneous_large_clipboard_replay_keeps_both_read_loops_live_for_first_input() {
        let (state_a, peer_b, root_a) =
            state_with_ordered_peer_for_queue_test("a-machine", "b-machine");
        let (state_b, peer_a, root_b) =
            state_with_ordered_peer_for_queue_test("b-machine", "a-machine");
        let image_a = synthetic_bmp_payload(128 * 1024);
        let mut image_b = synthetic_bmp_payload(128 * 1024);
        image_b[54] = 0x42;

        state_a
            .queue_clipboard_image(&peer_b, image_a.clone())
            .await
            .expect("queue A clipboard replay");
        state_b
            .queue_clipboard_image(&peer_a, image_b.clone())
            .await
            .expect("queue B clipboard replay");
        state_a
            .queue_input_events(&peer_b, vec![InputEvent::MouseMove { dx: 11, dy: 0 }])
            .await
            .expect("queue A first input");
        state_b
            .queue_input_events(&peer_a, vec![InputEvent::MouseMove { dx: -11, dy: 0 }])
            .await
            .expect("queue B first input");

        // This capacity is deliberately far below either image. The previous
        // whole-payload write path filled both directions and prevented either
        // session from returning to its read loop.
        let (stream_a, stream_b) = tokio::io::duplex(16 * 1024);
        let session_a = tokio::spawn(run_authenticated_session(
            state_a.clone(),
            peer_b.clone(),
            stream_a,
            true,
            None,
        ));
        let session_b = tokio::spawn(run_authenticated_session(
            state_b.clone(),
            peer_a.clone(),
            stream_b,
            false,
            None,
        ));

        time::timeout(Duration::from_secs(3), async {
            loop {
                let a_received_image = state_a.transport_events().await.iter().any(|event| {
                    event.direction == "incoming"
                        && event.kind == "clipboard_image"
                        && event.size_bytes == image_b.len() as u64
                });
                let b_received_image = state_b.transport_events().await.iter().any(|event| {
                    event.direction == "incoming"
                        && event.kind == "clipboard_image"
                        && event.size_bytes == image_a.len() as u64
                });
                if a_received_image
                    && b_received_image
                    && state_a.pending_inject_input_frame_count().await == 1
                    && state_b.pending_inject_input_frame_count().await == 1
                {
                    break;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("simultaneous image replay and first input must converge without deadlock");
        assert!(
            state_a
                .drain_outgoing_input(&peer_b, usize::MAX)
                .await
                .is_empty()
        );
        assert!(
            state_b
                .drain_outgoing_input(&peer_a, usize::MAX)
                .await
                .is_empty()
        );
        assert_eq!(state_a.outgoing_bulk_queue_len(&peer_b).await, 0);
        assert_eq!(state_b.outgoing_bulk_queue_len(&peer_a).await, 0);

        session_a.abort();
        session_b.abort();
        let _ = session_a.await;
        let _ = session_b.await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[tokio::test]
    async fn startup_bulk_turn_delivers_latest_max_clipboard_text_each_way() {
        let (state_a, peer_b, root_a) =
            state_with_ordered_peer_for_queue_test("a-machine", "b-machine");
        let (state_b, peer_a, root_b) =
            state_with_ordered_peer_for_queue_test("b-machine", "a-machine");

        for index in 0..5 {
            let prefix_a = format!("a-{index}-");
            let prefix_b = format!("b-{index}-");
            let text_a = format!(
                "{prefix_a}{}",
                "a".repeat(peer_transport::MAX_CLIPBOARD_TEXT_BYTES - prefix_a.len())
            );
            let text_b = format!(
                "{prefix_b}{}",
                "b".repeat(peer_transport::MAX_CLIPBOARD_TEXT_BYTES - prefix_b.len())
            );
            state_a
                .queue_clipboard_text(&peer_b, text_a)
                .await
                .expect("queue max text from A");
            state_b
                .queue_clipboard_text(&peer_a, text_b)
                .await
                .expect("queue max text from B");
        }
        state_a
            .queue_input_events(&peer_b, vec![InputEvent::MouseMove { dx: 5, dy: 0 }])
            .await
            .expect("queue A first input");
        state_b
            .queue_input_events(&peer_a, vec![InputEvent::MouseMove { dx: -5, dy: 0 }])
            .await
            .expect("queue B first input");

        let (stream_a, stream_b) = tokio::io::duplex(4 * 1024);
        let session_a = tokio::spawn(run_authenticated_session(
            state_a.clone(),
            peer_b.clone(),
            stream_a,
            true,
            None,
        ));
        let session_b = tokio::spawn(run_authenticated_session(
            state_b.clone(),
            peer_a.clone(),
            stream_b,
            false,
            None,
        ));

        let mut received_by_a = Vec::new();
        let mut received_by_b = Vec::new();
        time::timeout(Duration::from_secs(5), async {
            loop {
                while let Some(item) = state_a.dequeue_remote_clipboard_payload().await {
                    received_by_a.push(item);
                }
                while let Some(item) = state_b.dequeue_remote_clipboard_payload().await {
                    received_by_b.push(item);
                }
                if received_by_a.len() == 1
                    && received_by_b.len() == 1
                    && state_a.pending_inject_input_frame_count().await == 1
                    && state_b.pending_inject_input_frame_count().await == 1
                {
                    break;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("deterministic startup bulk turns must deliver latest max text each way");

        assert!(received_by_a.iter().all(|item| matches!(
            &item.payload,
            ClipboardPayload::Text(text)
                if text.len() == peer_transport::MAX_CLIPBOARD_TEXT_BYTES
                    && text.starts_with("b-4-")
        )));
        assert!(received_by_b.iter().all(|item| matches!(
            &item.payload,
            ClipboardPayload::Text(text)
                if text.len() == peer_transport::MAX_CLIPBOARD_TEXT_BYTES
                    && text.starts_with("a-4-")
        )));
        assert_eq!(state_a.outgoing_bulk_queue_len(&peer_b).await, 0);
        assert_eq!(state_b.outgoing_bulk_queue_len(&peer_a).await, 0);

        session_a.abort();
        session_b.abort();
        let _ = session_a.await;
        let _ = session_b.await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[tokio::test]
    async fn superseded_teardown_cannot_disconnect_replacement_owner() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let old_cancellation = Arc::new(crate::state::RuntimeWakeSignal::default());
        assert_eq!(
            state
                .claim_transport_session(&peer_id, 10, false, old_cancellation.clone())
                .await,
            crate::state::TransportSessionClaim::Claimed
        );
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("mark old owner connected");

        let (release_teardown, wait_for_teardown) = tokio::sync::oneshot::channel();
        let teardown_state = state.clone();
        let teardown_peer = peer_id.clone();
        let old_teardown = tokio::spawn(async move {
            let _ = wait_for_teardown.await;
            teardown_state
                .close_active_transport_session(&teardown_peer, 10)
                .await
        });

        assert_eq!(
            state
                .claim_transport_session(
                    &peer_id,
                    20,
                    true,
                    Arc::new(crate::state::RuntimeWakeSignal::default()),
                )
                .await,
            crate::state::TransportSessionClaim::Replaced {
                active_session_id: 10
            }
        );
        assert!(old_cancellation.take_pending());
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("mark replacement owner connected");

        release_teardown.send(()).expect("release old teardown");
        assert!(
            !time::timeout(Duration::from_secs(1), old_teardown)
                .await
                .expect("old teardown exits promptly")
                .expect("old teardown joins"),
            "superseded session must not clear replacement ownership"
        );
        assert!(
            state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "superseded teardown must not publish a stale disconnect"
        );
        assert!(state.close_active_transport_session(&peer_id, 20).await);
        assert!(
            !state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "closing the current replacement owner must publish disconnect"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn blocked_partial_write_times_out_requeues_and_unblocks_preferred_replacement() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let old_cancellation = Arc::new(crate::state::RuntimeWakeSignal::default());
        assert_eq!(
            state
                .claim_transport_session(&peer_id, 10, false, old_cancellation.clone())
                .await,
            crate::state::TransportSessionClaim::Claimed
        );
        state
            .queue_input_events(
                &peer_id,
                vec![InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: KeyState::Down,
                }],
            )
            .await
            .expect("queue sequence one");

        let (writer, entered_blocked_write) = PartialWriteBlockingWriter::new();
        let old_state = state.clone();
        let old_peer = peer_id.clone();
        let old_flush = tokio::spawn(async move {
            let _egress = old_state
                .acquire_transport_session_egress(&old_peer, 10)
                .await
                .expect("old session owns egress");
            let mut writer = writer;
            let mut flow = HashMap::new();
            let mut frame_buffer = Vec::new();
            let result = super::outbound::flush_outgoing_input_payloads_with_buffer(
                &old_state,
                "local",
                Some(&old_peer),
                PROTOCOL_CURRENT,
                &mut flow,
                &mut writer,
                &mut frame_buffer,
            )
            .await;
            (writer, result)
        });
        time::timeout(Duration::from_secs(1), entered_blocked_write.notified())
            .await
            .expect("old lane reaches blocked partial write");

        let replacement_state = state.clone();
        let replacement_peer = peer_id.clone();
        let replacement = tokio::spawn(async move {
            replacement_state
                .claim_transport_session(
                    &replacement_peer,
                    20,
                    true,
                    Arc::new(crate::state::RuntimeWakeSignal::default()),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !replacement.is_finished(),
            "replacement claim must wait while the old owner still owns a partial frame"
        );

        state
            .queue_input_events(
                &peer_id,
                vec![InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: KeyState::Up,
                }],
            )
            .await
            .expect("queue sequence two during replacement");

        let (old_writer, old_result) = time::timeout(
            peer_transport::TRANSPORT_EGRESS_IO_TIMEOUT + Duration::from_secs(1),
            old_flush,
        )
        .await
        .expect("old partial write exits after bounded timeout")
        .expect("old flush joins");
        let old_error = old_result.expect_err("partial write must time out");
        assert!(
            old_error.to_string().contains("timed out"),
            "unexpected old-lane failure: {old_error:#}"
        );
        assert!(
            !old_writer.bytes.is_empty(),
            "test must exercise a partially written frame before timeout"
        );
        assert_eq!(
            time::timeout(Duration::from_secs(1), replacement)
                .await
                .expect("replacement claims promptly after old write timeout")
                .expect("replacement joins"),
            crate::state::TransportSessionClaim::Replaced {
                active_session_id: 10
            }
        );
        assert!(old_cancellation.take_pending());

        let _egress = state
            .acquire_transport_session_egress(&peer_id, 20)
            .await
            .expect("replacement owns egress");
        let mut replacement_writer = CaptureWriter::default();
        let mut replacement_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        super::outbound::flush_outgoing_input_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut replacement_flow,
            &mut replacement_writer,
            &mut frame_buffer,
        )
        .await
        .expect("replacement input flush");
        assert!(matches!(
            decode_written_frames(&replacement_writer.bytes).as_slice(),
            [
                WireMessage::InputFrame { sequence: 1, .. },
                WireMessage::InputFrame { sequence: 2, .. }
            ]
        ));
        assert!(
            state
                .drain_outgoing_input(&peer_id, usize::MAX)
                .await
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn preferred_session_replaces_nonpreferred_and_becomes_only_input_lane() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let local_machine_id = state.snapshot().await.machine_id;
        let preferred_is_outbound = local_machine_id < peer_id;
        let old_is_outbound = !preferred_is_outbound;
        let (old_stream, mut old_remote) = TransportFaultHarness::pair();
        let old_session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            old_stream,
            old_is_outbound,
            None,
        ));
        old_remote
            .read_until("nonpreferred local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        old_remote.send_frame(remote_hello(&peer_id)).await;
        if !old_is_outbound {
            old_remote
                .read_until("nonpreferred reverse hello ack", |frame| {
                    matches!(frame, WireMessage::HelloAck { accepted: true, .. })
                })
                .await;
        }
        for _ in 0..50 {
            if state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected)
            {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "nonpreferred sole-reachable lane must negotiate"
        );

        let (preferred_stream, mut preferred_remote) = TransportFaultHarness::pair();
        let preferred_session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            preferred_stream,
            preferred_is_outbound,
            None,
        ));
        preferred_remote
            .read_until("preferred local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        preferred_remote.send_frame(remote_hello(&peer_id)).await;
        if !preferred_is_outbound {
            preferred_remote
                .read_until("preferred reverse hello ack", |frame| {
                    matches!(frame, WireMessage::HelloAck { accepted: true, .. })
                })
                .await;
        }
        time::timeout(Duration::from_secs(1), old_session)
            .await
            .expect("superseded session exits promptly")
            .expect("superseded session task joins")
            .expect("superseded session exits cleanly");
        assert!(
            state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "superseded teardown must preserve replacement connected state"
        );

        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 7, dy: -4 }])
            .await
            .expect("queue input for replacement session");
        let frame = preferred_remote
            .read_until("input on preferred replacement", |frame| {
                matches!(frame, WireMessage::InputFrame { .. })
            })
            .await;
        assert!(matches!(
            frame,
            WireMessage::InputFrame { events, .. }
                if matches!(events.as_slice(), [WireInputEvent::MouseMove { dx: 7, dy: -4 }])
        ));

        preferred_remote.disconnect().await;
        time::timeout(Duration::from_secs(1), preferred_session)
            .await
            .expect("preferred session exits promptly")
            .expect("preferred session task joins")
            .expect("preferred disconnect closes session cleanly");
        assert!(
            !state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "only closing the preferred current owner should disconnect the peer"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fault_harness_input_flushes_across_reconnect_after_delayed_frame() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 7, dy: -3 }])
            .await
            .expect("queue input frame");
        let (stream, mut remote) = TransportFaultHarness::pair();

        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            true,
            None,
        ));

        remote
            .read_until("local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        remote.send_frame(remote_hello(&peer_id)).await;

        let local_machine_id = state.snapshot().await.machine_id;
        let input_frame = remote
            .read_until("queued input frame", |frame| {
                matches!(frame, WireMessage::InputFrame { .. })
            })
            .await;
        assert!(matches!(
            input_frame,
            WireMessage::InputFrame {
                machine_id,
                sequence: 1,
                events,
                ..
            } if machine_id == local_machine_id
                && matches!(events.as_slice(), [WireInputEvent::MouseMove { dx: 7, dy: -3 }])
        ));

        remote.queue_delayed_frame(WireMessage::Heartbeat {
            machine_id: peer_id.clone(),
            timestamp_unix_ms: Utc::now().timestamp_millis(),
        });
        state.request_peer_reconnect(&peer_id).await;
        remote.release_delayed_frame().await;

        session
            .await
            .expect("session task joins")
            .expect("reconnect request closes session cleanly");
        assert!(
            !state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected),
            "reconnect should end the active session and mark the peer disconnected"
        );

        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: -2, dy: 5 }])
            .await
            .expect("queue input after reconnect request");
        let (stream, mut remote) = TransportFaultHarness::reconnect_pair();
        let session = tokio::spawn(run_authenticated_session(
            state.clone(),
            peer_id.clone(),
            stream,
            true,
            None,
        ));

        remote
            .read_until("second local hello", |frame| {
                matches!(frame, WireMessage::Hello { .. })
            })
            .await;
        remote.send_frame(remote_hello(&peer_id)).await;

        let input_frame = remote
            .read_until("input frame after reconnect", |frame| {
                matches!(frame, WireMessage::InputFrame { .. })
            })
            .await;
        assert!(matches!(
            input_frame,
            WireMessage::InputFrame {
                machine_id,
                sequence: 2,
                events,
                ..
            } if machine_id == local_machine_id
                && matches!(events.as_slice(), [WireInputEvent::MouseMove { dx: -2, dy: 5 }])
        ));

        remote.disconnect().await;
        session
            .await
            .expect("second session task joins")
            .expect("second disconnect closes session cleanly");

        let _ = std::fs::remove_dir_all(root);
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
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "one".to_string(),
                },
            )
            .await;
        state
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "two".to_string(),
                },
            )
            .await;

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
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec == "one"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec == "two"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_sends_layout_matrix_payload() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;

        state
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "self,peer".to_string(),
                },
            )
            .await;

        let mut writer = CaptureWriter::default();
        flush_outgoing_payloads(
            &state,
            "local-machine",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            &mut writer,
        )
        .await
        .expect("flush layout");

        let frames = decode_written_frames(&writer.bytes);
        assert!(matches!(
            frames.as_slice(),
            [WireMessage::LayoutMatrix {
                machine_id,
                matrix_spec
            }] if machine_id == "local-machine" && matrix_spec == "self,peer"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_requeues_unsafely_committed_and_remaining_payloads_on_mid_write_failure() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let large = "x".repeat(16 * 1024);

        for suffix in ["one", "two", "three"] {
            state
                .queue_outgoing_bulk_payload(
                    &peer_id,
                    OutboundPayload::LayoutMatrix {
                        matrix_spec: format!("{suffix}-{large}"),
                    },
                )
                .await;
        }

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
        assert_eq!(queued.len(), 3);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec.starts_with("one-")
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec.starts_with("two-")
        ));
        assert!(matches!(
            queued.get(2),
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec.starts_with("three-")
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_requeues_all_payloads_when_batch_flush_fails() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;

        state
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "one".to_string(),
                },
            )
            .await;
        state
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "two".to_string(),
                },
            )
            .await;

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
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec == "one"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::LayoutMatrix { matrix_spec }) if matrix_spec == "two"
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
    async fn flush_credit_streams_large_clipboard_image() {
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

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("start large clipboard image replay");

        let start_frames = decode_written_frames(&writer.bytes);
        let transfer_id = match start_frames.as_slice() {
            [
                WireMessage::ClipboardImageStart {
                    machine_id,
                    transfer_id,
                    total_bytes,
                    hash_hex,
                },
            ] if machine_id == "local"
                && *total_bytes == image_bmp.len() as u64
                && hash_hex == &payload_hash_hex(&ClipboardPayload::Image(image_bmp.clone())) =>
            {
                transfer_id.clone()
            }
            other => panic!("expected one clipboard image start, got {other:?}"),
        };

        while state.outgoing_bulk_queue_len(&peer_id).await > 0 {
            peer_transport::apply_outbound_chunk_credits(
                &mut outbound_transfer_flow,
                &transfer_id,
                1,
            )
            .expect("active clipboard replay flow");
            super::outbound::flush_outgoing_bulk_payloads_with_buffer(
                &state,
                "local",
                Some(&peer_id),
                PROTOCOL_CURRENT,
                DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
                &mut outbound_transfer_flow,
                &mut writer,
                &mut frame_buffer,
            )
            .await
            .expect("flush one credited clipboard image chunk");
        }

        let queued = state.drain_outgoing(&peer_id).await;
        assert!(
            queued.is_empty(),
            "credited clipboard image must not remain queued after completion"
        );

        let frames = decode_written_frames(&writer.bytes);
        assert!(matches!(
            frames.first(),
            Some(WireMessage::ClipboardImageStart {
                machine_id,
                total_bytes,
                hash_hex,
                transfer_id: frame_transfer_id,
                ..
            }) if machine_id == "local" && frame_transfer_id == &transfer_id
                && *total_bytes == image_bmp.len() as u64
                && hash_hex == &payload_hash_hex(&ClipboardPayload::Image(image_bmp.clone()))
        ));
        assert!(matches!(
            frames.last(),
            Some(WireMessage::ClipboardImageEnd { transfer_id: frame_transfer_id })
                if frame_transfer_id == &transfer_id
        ));
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(frame, WireMessage::ClipboardImageChunk { .. }))
                .count(),
            image_bmp
                .len()
                .div_ceil(peer_transport::CLIPBOARD_IMAGE_CHUNK_BYTES),
            "large image should send exactly one frame per credited chunk"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn rapid_direct_clipboard_supersession_completes_latest_and_unblocks_layout() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let oldest_image = synthetic_bmp_payload(32 * 1024);
        state
            .queue_clipboard_image(&peer_id, oldest_image.clone())
            .await
            .expect("queue initial clipboard image");

        let mut outbound_transfer_flow = HashMap::new();
        let mut frame_buffer = Vec::new();
        let mut initial_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            usize::MAX,
            &mut outbound_transfer_flow,
            &mut initial_writer,
            &mut frame_buffer,
        )
        .await
        .expect("start initial clipboard image");
        let initial_transfer_id = match decode_written_frames(&initial_writer.bytes).as_slice() {
            [WireMessage::ClipboardImageStart { transfer_id, .. }] => transfer_id.clone(),
            other => panic!("expected initial clipboard start, got {other:?}"),
        };
        assert!(
            state
                .has_outgoing_clipboard_image_cursor(&peer_id, &initial_transfer_id)
                .await
        );

        let mut inbound_transfers = HashMap::new();
        assert!(
            handle_clipboard_image_start(
                &state,
                &peer_id,
                Some(&peer_id),
                peer_id.clone(),
                initial_transfer_id.clone(),
                oldest_image.len() as u64,
                clipboard_image_hash_hex(&oldest_image),
                &mut inbound_transfers,
            )
            .await
            .expect("accept initial inbound clipboard start")
        );

        let mut newest_image = Vec::new();
        for index in 0..(peer_transport::MAX_INBOUND_TRANSFERS_PER_PEER + 3) {
            let mut image = synthetic_bmp_payload(32 * 1024 + index);
            image[54] = index as u8;
            newest_image = image.clone();
            state
                .queue_clipboard_image(&peer_id, image)
                .await
                .expect("queue superseding direct clipboard image");
        }
        state
            .queue_outgoing_bulk_payload(
                &peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: "self,peer".to_string(),
                },
            )
            .await;
        assert_eq!(
            state.outgoing_bulk_queue_len(&peer_id).await,
            2,
            "one latest image and following layout should remain queued"
        );

        let mut writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            usize::MAX,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("flush latest clipboard start and following layout");
        let frames = decode_written_frames(&writer.bytes);
        let latest_transfer_id = frames
            .iter()
            .find_map(|frame| match frame {
                WireMessage::ClipboardImageStart {
                    transfer_id,
                    total_bytes,
                    ..
                } if *total_bytes == newest_image.len() as u64 => Some(transfer_id.clone()),
                _ => None,
            })
            .expect("latest clipboard start frame");
        assert!(frames.iter().any(|frame| matches!(
            frame,
            WireMessage::LayoutMatrix { matrix_spec, .. } if matrix_spec == "self,peer"
        )));
        assert!(
            !outbound_transfer_flow.contains_key(&initial_transfer_id),
            "superseded live cursor flow must be retired"
        );
        assert!(
            handle_clipboard_image_start(
                &state,
                &peer_id,
                Some(&peer_id),
                peer_id.clone(),
                latest_transfer_id.clone(),
                newest_image.len() as u64,
                clipboard_image_hash_hex(&newest_image),
                &mut inbound_transfers,
            )
            .await
            .expect("accept latest inbound clipboard start")
        );
        assert_eq!(inbound_transfers.len(), 1);
        assert!(inbound_transfers.contains_key(&latest_transfer_id));

        while state.outgoing_bulk_queue_len(&peer_id).await > 0 {
            peer_transport::apply_outbound_chunk_credits(
                &mut outbound_transfer_flow,
                &latest_transfer_id,
                1,
            )
            .expect("credit latest clipboard transfer");
            let frame_offset = writer.bytes.len();
            super::outbound::flush_outgoing_bulk_payloads_with_buffer(
                &state,
                "local",
                Some(&peer_id),
                PROTOCOL_CURRENT,
                usize::MAX,
                &mut outbound_transfer_flow,
                &mut writer,
                &mut frame_buffer,
            )
            .await
            .expect("flush latest clipboard chunk");
            for frame in decode_written_frames(&writer.bytes[frame_offset..]) {
                match frame {
                    WireMessage::ClipboardImageChunk { transfer_id, data } => {
                        handle_clipboard_image_chunk(
                            &state,
                            transfer_id,
                            data,
                            &mut inbound_transfers,
                        )
                        .await
                        .expect("accept latest clipboard chunk");
                    }
                    WireMessage::ClipboardImageEnd { transfer_id } => {
                        handle_clipboard_image_end(&state, transfer_id, &mut inbound_transfers)
                            .await
                            .expect("complete latest clipboard image");
                    }
                    other => panic!("unexpected credited clipboard frame: {other:?}"),
                }
            }
        }

        let received = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("latest clipboard image should complete");
        assert!(matches!(
            received.payload,
            ClipboardPayload::Image(image) if image == newest_image
        ));
        assert!(inbound_transfers.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inbound_clipboard_start_is_latest_wins_beyond_transfer_cap_and_same_id_restart() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut inbound_transfers = HashMap::new();
        let total_starts = peer_transport::MAX_INBOUND_TRANSFERS_PER_PEER + 3;

        for index in 0..total_starts {
            let transfer_id = format!("clip-{index}");
            assert!(
                handle_clipboard_image_start(
                    &state,
                    &peer_id,
                    Some(&peer_id),
                    peer_id.clone(),
                    transfer_id.clone(),
                    64,
                    format!("hash-{index}"),
                    &mut inbound_transfers,
                )
                .await
                .expect("accept superseding clipboard start")
            );
            assert_eq!(inbound_transfers.len(), 1);
            assert!(inbound_transfers.contains_key(&transfer_id));
        }

        let latest_transfer_id = format!("clip-{}", total_starts - 1);
        let latest = inbound_transfers
            .get_mut(&latest_transfer_id)
            .expect("latest transfer retained");
        latest.bytes_received = 1;
        latest.data.push(7);
        assert!(
            handle_clipboard_image_start(
                &state,
                &peer_id,
                Some(&peer_id),
                peer_id.clone(),
                latest_transfer_id.clone(),
                96,
                "replacement-hash".to_string(),
                &mut inbound_transfers,
            )
            .await
            .expect("accept same-id clipboard restart")
        );
        let restarted = inbound_transfers
            .get(&latest_transfer_id)
            .expect("same-id restart retained");
        assert_eq!(restarted.total_bytes, 96);
        assert_eq!(restarted.bytes_received, 0);
        assert!(restarted.data.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_clipboard_chunk_restarts_from_start_on_replacement_session() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .requeue_outgoing_front(
                &peer_id,
                vec![OutboundPayload::ClipboardImage {
                    image_bmp: vec![0u8; 300 * 1024],
                }],
            )
            .await;

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
        .expect("send clipboard image start");
        let transfer_id = match decode_written_frames(&start_writer.bytes).as_slice() {
            [WireMessage::ClipboardImageStart { transfer_id, .. }] => transfer_id.clone(),
            other => panic!("expected clipboard image start, got {other:?}"),
        };
        peer_transport::apply_outbound_chunk_credits(&mut outbound_transfer_flow, &transfer_id, 1)
            .expect("active clipboard replay flow");

        let mut failing_writer = FailAfterCallsWriter::new(0);
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
        .expect_err("failed chunk write should preserve replay cursor");

        let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        assert_eq!(
            queued.len(),
            1,
            "failed chunk should requeue one clipboard replay cursor"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardImageCursor {
                transfer_id: cursor_transfer_id,
                image_bmp,
                offset_bytes: 0,
            }) if cursor_transfer_id == &transfer_id && image_bmp.len() == 300 * 1024
        ));

        state.requeue_outgoing_front(&peer_id, queued).await;
        let mut replacement_flow = HashMap::new();
        let mut replacement_writer = CaptureWriter::default();
        super::outbound::flush_outgoing_bulk_payloads_with_buffer(
            &state,
            "local",
            Some(&peer_id),
            PROTOCOL_CURRENT,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut replacement_flow,
            &mut replacement_writer,
            &mut frame_buffer,
        )
        .await
        .expect("replacement session restarts orphaned clipboard cursor");
        assert!(matches!(
            decode_written_frames(&replacement_writer.bytes).as_slice(),
            [WireMessage::ClipboardImageStart {
                transfer_id: replacement_transfer_id,
                ..
            }] if replacement_transfer_id != &transfer_id
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
                kind: peer_transport::OutboundTransferKind::File,
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
        let probe = match TcpListener::bind("[::]:0").await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind("0.0.0.0:0").await.expect("probe bind"),
        };
        let preferred_port = probe.local_addr().expect("probe addr").port();
        drop(probe);

        state
            .update_network_port(preferred_port)
            .await
            .expect("set preferred port");

        let listeners = prepare_listener(&state).await;
        assert!(!listeners.is_empty(), "listener bind should succeed");
        let effective_port = listeners[0].local_addr().expect("addr").port();
        assert_eq!(effective_port, preferred_port);
        assert_eq!(state.snapshot().await.network_port, preferred_port);

        drop(listeners);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_listener_falls_back_and_persists_effective_port() {
        let (state, root) = state_for_listener_test().await;
        let blocker = bind_dual_stack_tcp_listeners(0)
            .expect("block bind")
            .into_iter()
            .next()
            .expect("blocker listener");
        let blocked_port = blocker.local_addr().expect("block addr").port();

        state
            .update_network_port(blocked_port)
            .await
            .expect("set blocked port");

        let listeners = prepare_listener(&state).await;
        assert!(
            !listeners.is_empty(),
            "fallback listener bind should succeed"
        );
        let effective_port = listeners[0].local_addr().expect("addr").port();
        assert_ne!(
            effective_port, blocked_port,
            "fallback must avoid blocked configured port"
        );
        assert_eq!(state.snapshot().await.network_port, effective_port);

        drop(listeners);
        drop(blocker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepare_listener_falls_back_when_ipv4_only_blocks_configured_port() {
        let (state, root) = state_for_listener_test().await;
        let blocker = TcpListener::bind("0.0.0.0:0")
            .await
            .expect("bind IPv4 blocker");
        let blocked_port = blocker.local_addr().expect("block addr").port();

        let direct_bind = bind_dual_stack_tcp_listeners(blocked_port);
        assert!(
            direct_bind.is_err(),
            "partial IPv6-only fallback must not succeed on an IPv4-blocked configured port"
        );

        state
            .update_network_port(blocked_port)
            .await
            .expect("set blocked port");

        let listeners = prepare_listener(&state).await;
        assert!(
            !listeners.is_empty(),
            "fallback listener bind should succeed"
        );
        let effective_port = listeners[0].local_addr().expect("addr").port();
        assert_ne!(
            effective_port, blocked_port,
            "fallback must avoid IPv4-blocked configured port"
        );
        assert_eq!(state.snapshot().await.network_port, effective_port);

        drop(listeners);
        drop(blocker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn dual_stack_listener_binds_ipv6_any_or_ipv4_fallback() {
        let listeners = bind_dual_stack_tcp_listeners(0).expect("dual stack listener bind");
        assert!(!listeners.is_empty());
        let first_addr = listeners[0].local_addr().expect("listener addr");
        assert_ne!(first_addr.port(), 0);
        assert!(
            first_addr.is_ipv6()
                || listeners.iter().any(|listener| {
                    listener
                        .local_addr()
                        .map(|addr| addr.is_ipv4())
                        .unwrap_or(false)
                }),
            "listener set should include IPv6 dual-stack or an IPv4 fallback"
        );
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
    async fn hello_handler_rejects_protocol_4_3_before_clipboard_flow_control() {
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
            ProtocolVersion {
                major: 4,
                minor: 3,
                patch: 0,
            },
            &mut remote_protocol,
            &mut outbound_transfer_flow,
            &mut writer,
            &mut frame_buffer,
        )
        .await
        .expect("reject protocol 4.2 hello");

        assert!(matches!(handling, HelloHandling::TerminateSession));
        assert!(remote_protocol.is_none());
        assert!(matches!(
            decode_written_frames(&writer.bytes).as_slice(),
            [WireMessage::Error { message }]
                if message.contains("remote=4.3.0") && message.contains(&format!("expected={PROTOCOL_CURRENT}"))
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
    async fn inbound_hello_flushes_ack_and_defers_pending_bulk_to_startup_turn() {
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
        assert_eq!(frames.len(), 1, "inbound hello should flush only its ack");
        assert!(matches!(
            frames.first(),
            Some(WireMessage::HelloAck {
                machine_id,
                accepted: true
            }) if machine_id == "local-machine-id"
        ));
        assert!(matches!(
            state.drain_outgoing(&peer_id).await.as_slice(),
            [OutboundPayload::ClipboardText { text }] if text == "replay-inbound"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hello_ack_handler_flushes_input_only_and_defers_bulk_to_session_turn() {
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
        assert!(frames.is_empty());

        let queued = state.drain_outgoing(&peer_id).await;
        assert!(matches!(
            queued.as_slice(),
            [OutboundPayload::ClipboardText { text }] if text == "hello-control"
        ));

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
        const PEER_HASH_SENTINEL: &str = "BOUNDLESS_SECRET_SENTINEL_peer_hash_91f6310b";
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let image = minimal_bmp_payload();
        let actual_hash = payload_hash_hex(&ClipboardPayload::Image(image.clone()));
        let mut inbound_transfers = HashMap::new();

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "clip-bad".to_string(),
            image.len() as u64,
            PEER_HASH_SENTINEL.to_string(),
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
        let events = state.transport_events().await;
        let rejection = events
            .iter()
            .find(|event| event.kind == "clipboard_image_rejected")
            .expect("hash mismatch should be recorded as clipboard metadata");
        assert_eq!(
            rejection.detail,
            "payload_type=bmp disposition=rejected reason=hash_mismatch"
        );
        let rendered = format!("{events:?}");
        assert!(!rendered.contains(PEER_HASH_SENTINEL));
        assert!(!rendered.contains(&actual_hash));
        assert!(!rendered.contains("expected="));
        assert!(!rendered.contains("actual="));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn chunked_clipboard_image_rejections_preserve_safe_numeric_causes() {
        const TRANSFER_ID_SENTINEL: &str =
            "BOUNDLESS_SECRET_SENTINEL_clipboard_transfer_id_228337f6";
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let max_image_bytes = core_clipboard::ClipboardPolicy::default().max_image_bytes as u64;
        let mut inbound_transfers = HashMap::new();

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            TRANSFER_ID_SENTINEL.to_string(),
            max_image_bytes + 1,
            "unused".to_string(),
            &mut inbound_transfers,
        )
        .await
        .expect("reject oversized clipboard image start");

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "chunk-overflow".to_string(),
            1,
            "unused".to_string(),
            &mut inbound_transfers,
        )
        .await
        .expect("start chunk overflow transfer");
        handle_clipboard_image_chunk(
            &state,
            "chunk-overflow".to_string(),
            vec![0, 1],
            &mut inbound_transfers,
        )
        .await
        .expect("reject chunk beyond announced total");

        handle_clipboard_image_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "short-transfer".to_string(),
            2,
            "unused".to_string(),
            &mut inbound_transfers,
        )
        .await
        .expect("start short transfer");
        handle_clipboard_image_chunk(
            &state,
            "short-transfer".to_string(),
            vec![0],
            &mut inbound_transfers,
        )
        .await
        .expect("append short transfer chunk");
        handle_clipboard_image_end(&state, "short-transfer".to_string(), &mut inbound_transfers)
            .await
            .expect("reject short transfer end");

        let events = state.transport_events().await;
        let rejection_details = events
            .iter()
            .filter(|event| event.kind == "clipboard_image_rejected")
            .map(|event| event.detail.as_str())
            .collect::<Vec<_>>();
        assert!(rejection_details.iter().any(|detail| {
            detail.contains("reason=payload_too_large")
                && detail.contains(&format!("announced_bytes={}", max_image_bytes + 1))
                && detail.contains(&format!("configured_limit_bytes={max_image_bytes}"))
        }));
        assert!(rejection_details.iter().any(|detail| {
            detail.contains("reason=chunk_exceeds_total")
                && detail.contains("announced_bytes=1")
                && detail.contains("attempted_bytes=2")
        }));
        assert!(rejection_details.iter().any(|detail| {
            detail.contains("reason=size_mismatch")
                && detail.contains("expected_bytes=2")
                && detail.contains("received_bytes=1")
        }));
        assert!(!format!("{events:?}").contains(TRANSFER_ID_SENTINEL));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_clipboard_image_logs_omit_attacker_transfer_identifiers() {
        use tracing::instrument::WithSubscriber;

        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_clipboard_log_6d78c298";
        let capture = TestLogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);

        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let mut inbound_transfers = HashMap::new();
        async {
            tracing::callsite::rebuild_interest_cache();
            handle_clipboard_image_start(
                &state,
                &peer_id,
                Some(&peer_id),
                SECRET.to_string(),
                SECRET.to_string(),
                1,
                SECRET.to_string(),
                &mut inbound_transfers,
            )
            .await
            .expect("drop mismatched identity start");
            handle_clipboard_image_chunk(
                &state,
                SECRET.to_string(),
                vec![0],
                &mut inbound_transfers,
            )
            .await
            .expect("drop unknown chunk");
            handle_clipboard_image_end(&state, SECRET.to_string(), &mut inbound_transfers)
                .await
                .expect("drop unknown end");

            let image = minimal_bmp_payload();
            handle_clipboard_image_start(
                &state,
                &peer_id,
                Some(&peer_id),
                peer_id.clone(),
                SECRET.to_string(),
                image.len() as u64,
                payload_hash_hex(&ClipboardPayload::Image(image.clone())),
                &mut inbound_transfers,
            )
            .await
            .expect("start valid transfer with attacker identifier");
            handle_clipboard_image_chunk(&state, SECRET.to_string(), image, &mut inbound_transfers)
                .await
                .expect("append valid transfer");
            handle_clipboard_image_end(&state, SECRET.to_string(), &mut inbound_transfers)
                .await
                .expect("complete valid transfer");
        }
        .with_subscriber(dispatch)
        .await;

        let rendered = capture.rendered();
        assert!(rendered.contains("dropping clipboard image start"));
        assert!(rendered.contains("received clipboard image chunk for unknown transfer"));
        assert!(rendered.contains("received clipboard image end for unknown transfer"));
        assert!(rendered.contains("started inbound clipboard image transfer"));
        assert!(rendered.contains("completed inbound clipboard image transfer"));
        assert!(!rendered.contains(SECRET));

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
    async fn file_feature_revocation_discards_active_receive_and_rejects_new_start() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        let receive_dir = root.join("received");
        let mut config = state.file_transfer_config().await;
        config.auto_accept_trusted_peers = true;
        config.receive_dir = receive_dir.display().to_string();
        state
            .update_file_transfer_config(config)
            .await
            .expect("receive config");
        let mut inbound = HashMap::new();
        let mut writer = CaptureWriter::default();
        let mut buffer = Vec::new();
        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "active".to_string(),
            "file.txt".to_string(),
            3,
            &mut inbound,
            &mut writer,
            &mut buffer,
        )
        .await
        .expect("start");
        assert_eq!(inbound.len(), 1);
        state
            .set_feature("transfer_file".to_string(), false)
            .await
            .expect("disable file sharing");
        handle_file_chunk(
            &state,
            "active".to_string(),
            b"abc".to_vec(),
            &mut inbound,
            &mut writer,
            &mut buffer,
        )
        .await
        .expect("revoked chunk");
        assert!(inbound.is_empty());
        assert!(!receive_dir.join("file.txt").exists());
        assert!(!receive_dir.join(".file.txt.boundless.part").exists());
        handle_file_start(
            &state,
            &peer_id,
            Some(&peer_id),
            peer_id.clone(),
            "new".to_string(),
            "new.txt".to_string(),
            3,
            &mut inbound,
            &mut writer,
            &mut buffer,
        )
        .await
        .expect("rejected start");
        assert!(inbound.is_empty());
        assert_eq!(
            std::fs::read_dir(&receive_dir)
                .expect("receive folder")
                .count(),
            0
        );
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
    async fn hello_then_hello_ack_leave_pending_bulk_for_session_startup_turn() {
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
        assert!(hello_frames.is_empty());

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
            "control handlers leave bulk to the explicit session startup turn"
        );
        assert!(matches!(
            state.drain_outgoing(&peer_id).await.as_slice(),
            [OutboundPayload::ClipboardText { text }] if text == "replay-once"
        ));

        let _ = std::fs::remove_dir_all(root);
    }
    async fn measure_unrelated_peer_egress() -> serde_json::Value {
        let (state, blocked_peer, root) = state_with_peer_for_queue_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let healthy_peer = state
            .join_peer(code, "127.0.0.2:15100".into(), None)
            .await
            .unwrap();
        state
            .claim_transport_session(
                &blocked_peer,
                501,
                true,
                Arc::new(crate::state::RuntimeWakeSignal::default()),
            )
            .await;
        state
            .queue_clipboard_text(&blocked_peer, "blocked bulk".repeat(512))
            .await
            .unwrap();
        let (writer, entered) = PartialWriteBlockingWriter::new();
        let blocked_state = state.clone();
        let blocked_id = blocked_peer.clone();
        let blocked = tokio::spawn(async move {
            let _guard = blocked_state
                .acquire_transport_session_egress(&blocked_id, 501)
                .await
                .unwrap();
            let mut writer = writer;
            let mut flow = HashMap::new();
            let mut buffer = Vec::new();
            super::outbound::flush_outgoing_bulk_payloads_with_buffer(
                &blocked_state,
                "local",
                Some(&blocked_id),
                PROTOCOL_CURRENT,
                4,
                &mut flow,
                &mut writer,
                &mut buffer,
            )
            .await
        });
        time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("bulk writer stalls");
        let started = time::Instant::now();
        let healthy_result = time::timeout(Duration::from_millis(250), async {
            assert_eq!(
                state
                    .claim_transport_session(
                        &healthy_peer,
                        502,
                        true,
                        Arc::new(crate::state::RuntimeWakeSignal::default())
                    )
                    .await,
                crate::state::TransportSessionClaim::Claimed
            );
            state
                .queue_input_events(
                    &healthy_peer,
                    vec![InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Up,
                    }],
                )
                .await
                .unwrap();
            let _guard = state
                .acquire_transport_session_egress(&healthy_peer, 502)
                .await
                .unwrap();
            let mut writer = CaptureWriter::default();
            let mut flow = HashMap::new();
            let mut buffer = Vec::new();
            super::outbound::flush_outgoing_input_payloads_with_buffer(
                &state,
                "local",
                Some(&healthy_peer),
                PROTOCOL_CURRENT,
                &mut flow,
                &mut writer,
                &mut buffer,
            )
            .await
            .unwrap();
            assert!(
                !writer.bytes.is_empty(),
                "unrelated peer receives serialized input"
            );
        })
        .await;
        let elapsed_us = started.elapsed().as_micros();
        let blocked_still_waiting = !blocked.is_finished();
        blocked.abort();
        let _ = blocked.await;
        let _ = std::fs::remove_dir_all(root);
        healthy_result
            .expect("one peer's stalled bulk must not hold unrelated input or session claims");
        assert!(
            blocked_still_waiting,
            "measurement must finish while other writer is still blocked"
        );
        serde_json::json!({"kind":"synthetic_transport", "scenario":"unrelated_peer_input_during_stalled_bulk", "unrelated_peer_elapsed_us":elapsed_us, "stalled_peer_pending":blocked_still_waiting, "deadline_ms":250})
    }

    #[tokio::test]
    async fn stalled_bulk_peer_does_not_block_unrelated_peer_input() {
        measure_unrelated_peer_egress().await;
    }

    #[tokio::test]
    #[ignore = "opt-in repeatable synthetic transport benchmark; does not measure hardware input latency"]
    async fn transport_safety_benchmark() {
        for closed in [false, true] {
            let metric =
                super::runtime::measure_worker_retry_cadence(closed, Duration::from_millis(3250))
                    .await;
            assert_eq!(
                metric["attempts"], 3,
                "one, two and four second retry cadence: {metric}"
            );
            println!("boundless_transport_benchmark={metric}");
        }
        for _ in 0..10 {
            let metric = measure_unrelated_peer_egress().await;
            println!("boundless_transport_benchmark={metric}");
        }
    }

    #[tokio::test]
    async fn simultaneous_post_startup_maximum_text_remains_live() {
        use core_clipboard::ClipboardPayload;
        let (state_a, peer_b, root_a) = state_with_ordered_peer_for_queue_test("a-live", "b-live");
        let (state_b, peer_a, root_b) = state_with_ordered_peer_for_queue_test("b-live", "a-live");
        let (a, b) = tokio::io::duplex(4096);
        let mut sessions = tokio::task::JoinSet::new();
        sessions.spawn(run_authenticated_session(
            state_a.clone(),
            peer_b.clone(),
            a,
            true,
            None,
        ));
        sessions.spawn(run_authenticated_session(
            state_b.clone(),
            peer_a.clone(),
            b,
            false,
            None,
        ));
        time::timeout(Duration::from_secs(2), async {
            loop {
                if state_a.get_peer(&peer_b).await.unwrap().connected
                    && state_b.get_peer(&peer_a).await.unwrap().connected
                {
                    break;
                }
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("initial handshake");
        // Ensure this is live egress after both startup turns, not replay.
        time::sleep(Duration::from_millis(100)).await;
        for round in 0..3 {
            state_a
                .queue_clipboard_text(
                    &peer_b,
                    format!(
                        "{round}{}",
                        "a".repeat(peer_transport::MAX_CLIPBOARD_TEXT_BYTES - 1)
                    ),
                )
                .await
                .unwrap();
            state_b
                .queue_clipboard_text(
                    &peer_a,
                    format!(
                        "{round}{}",
                        "b".repeat(peer_transport::MAX_CLIPBOARD_TEXT_BYTES - 1)
                    ),
                )
                .await
                .unwrap();
            let received = time::timeout(Duration::from_secs(1), async {
                let mut got_a = false;
                let mut got_b = false;
                loop {
                    if let Some(item) = state_a.dequeue_remote_clipboard_payload().await { got_a |= matches!(item.payload, ClipboardPayload::Text(ref text) if text.len() == peer_transport::MAX_CLIPBOARD_TEXT_BYTES); }
                    if let Some(item) = state_b.dequeue_remote_clipboard_payload().await { got_b |= matches!(item.payload, ClipboardPayload::Text(ref text) if text.len() == peer_transport::MAX_CLIPBOARD_TEXT_BYTES); }
                    if got_a && got_b { break; }
                    time::sleep(Duration::from_millis(5)).await;
                }
            }).await;
            if received.is_err() {
                sessions.shutdown().await;
            }
            received.unwrap_or_else(|_| {
                panic!("post-startup full-duplex maximum text stalled in round {round}")
            });
        }
        sessions.shutdown().await;
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }
}
