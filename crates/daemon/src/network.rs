use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
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
    PROTOCOL_CURRENT, WireInputEvent, WireKeyState, WireMessage, WireMouseButton, decode_bytes_b64,
    decode_line, encode_bytes_b64, encode_line,
};
use core_transfer::validate_transfer_size;

use crate::state::{AppState, OutboundPayload};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const SUPERVISOR_TICK: Duration = Duration::from_secs(3);
const MAX_BACKOFF_SECONDS: u64 = 30;
const FILE_CHUNK_BYTES: usize = 48 * 1024;

#[derive(Debug)]
struct InboundTransfer {
    peer_id: String,
    file_name: String,
    total_bytes: u64,
    bytes: Vec<u8>,
}

pub fn start(state: AppState) {
    tokio::spawn(listener_loop(state.clone()));
    tokio::spawn(supervisor_loop(state));
}

async fn listener_loop(state: AppState) {
    let snapshot = state.snapshot().await;
    let bind = format!("0.0.0.0:{}", snapshot.network_port);

    let listener = match TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(%error, bind = %bind, "transport listener failed to bind");
            return;
        }
    };

    info!(bind = %bind, "transport listener started");

    loop {
        match listener.accept().await {
            Ok((socket, remote)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_incoming_connection(state, socket).await {
                        warn!(error = ?error, remote = %remote, "incoming session ended with error");
                    }
                });
            }
            Err(error) => {
                warn!(%error, "transport accept failed");
                time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn supervisor_loop(state: AppState) {
    let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();

    loop {
        workers.retain(|_, handle| !handle.is_finished());

        let snapshot = state.snapshot().await;
        for peer in snapshot.peers {
            if peer.peer_id == snapshot.machine_id || peer.address.trim().is_empty() {
                continue;
            }

            if workers.contains_key(&peer.peer_id) {
                continue;
            }

            let state = state.clone();
            let peer_id = peer.peer_id.clone();
            let handle = tokio::spawn(async move {
                peer_worker(state, peer_id).await;
            });
            workers.insert(peer.peer_id, handle);
        }

        time::sleep(SUPERVISOR_TICK).await;
    }
}

async fn peer_worker(state: AppState, peer_id: String) {
    let mut backoff_secs: u64 = 1;

    loop {
        let Some(peer) = state.get_peer(&peer_id).await else {
            info!(peer_id = %peer_id, "peer worker exiting; peer removed");
            return;
        };

        match connect_and_run_outbound(state.clone(), &peer_id, &peer.address).await {
            Ok(()) => {
                backoff_secs = 1;
            }
            Err(error) => {
                warn!(peer_id = %peer_id, address = %peer.address, error = ?error, "outbound connect failed");
                if let Err(mark_error) = state.set_peer_connected(&peer_id, false).await {
                    warn!(%mark_error, "failed to mark peer disconnected");
                }

                time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            }
        }
    }
}

async fn connect_and_run_outbound(state: AppState, peer_id: &str, address: &str) -> Result<()> {
    let socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("tcp connect {address}"))?;

    let connector = build_tls_connector(&state).await?;
    let server_name = parse_server_name(address)?;
    let stream = connector
        .connect(server_name, socket)
        .await
        .with_context(|| format!("tls connect {address}"))?;

    run_session(
        state,
        Some(peer_id.to_string()),
        tokio_rustls::TlsStream::Client(stream),
        true,
    )
    .await
}

async fn handle_incoming_connection(state: AppState, socket: TcpStream) -> Result<()> {
    let acceptor = build_tls_acceptor(&state).await?;
    let stream = acceptor.accept(socket).await.context("tls accept")?;
    run_session(state, None, tokio_rustls::TlsStream::Server(stream), false).await
}

async fn run_session<S>(
    state: AppState,
    peer_hint: Option<String>,
    stream: tokio_rustls::TlsStream<S>,
    is_outbound: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let authenticated_peer_id = authenticated_peer_machine_id(&state, &stream).await?;
    if let Some(expected_peer_id) = peer_hint.as_deref()
        && expected_peer_id != authenticated_peer_id
    {
        bail!(
            "peer identity mismatch: expected {} from topology, authenticated {} from TLS",
            expected_peer_id,
            authenticated_peer_id
        );
    }

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut interval = time::interval(HEARTBEAT_INTERVAL);

    let snapshot = state.snapshot().await;
    let local_hello = WireMessage::Hello {
        machine_id: snapshot.machine_id.clone(),
        display_name: snapshot.device_name.clone(),
        protocol: PROTOCOL_CURRENT,
        capability_count: core_protocol::default_capabilities().len(),
    };

    send_message(&mut writer, &local_hello).await?;

    let remote_peer_id = Some(authenticated_peer_id.clone());
    let mut inbound_transfers: HashMap<String, InboundTransfer> = HashMap::new();

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let heartbeat = WireMessage::Heartbeat {
                    machine_id: snapshot.machine_id.clone(),
                    timestamp_unix_ms: now_millis(),
                };
                send_message(&mut writer, &heartbeat).await?;
                flush_outgoing_payloads(
                    &state,
                    &snapshot.machine_id,
                    remote_peer_id.as_deref(),
                    &mut writer,
                )
                .await?;
            }
            read = reader.read_line(&mut line) => {
                let read = read.context("read transport line")?;
                if read == 0 {
                    break;
                }

                let message = decode_line(&line).context("decode wire message")?;
                line.clear();

                match message {
                    WireMessage::Hello { machine_id, .. } => {
                        if machine_id != authenticated_peer_id {
                            warn!(
                                claimed_machine_id = %machine_id,
                                authenticated_machine_id = %authenticated_peer_id,
                                "hello machine_id mismatch from authenticated peer"
                            );
                            let _ = send_message(
                                &mut writer,
                                &WireMessage::Error {
                                    message: "hello machine_id mismatch".to_string(),
                                },
                            )
                            .await;
                            break;
                        }

                        if let Some(peer_id) = &remote_peer_id {
                            let _ = state.set_peer_connected(peer_id, true).await;
                        }

                        if !is_outbound {
                            let ack = WireMessage::HelloAck {
                                machine_id: snapshot.machine_id.clone(),
                                accepted: true,
                            };
                            send_message(&mut writer, &ack).await?;
                        }

                        flush_outgoing_payloads(
                            &state,
                            &snapshot.machine_id,
                            remote_peer_id.as_deref(),
                            &mut writer,
                        )
                        .await?;
                    }
                    WireMessage::HelloAck { accepted, .. } => {
                        if accepted && let Some(peer_id) = &remote_peer_id {
                            let _ = state.set_peer_connected(peer_id, true).await;
                        }

                        flush_outgoing_payloads(
                            &state,
                            &snapshot.machine_id,
                            remote_peer_id.as_deref(),
                            &mut writer,
                        )
                        .await?;
                    }
                    WireMessage::Heartbeat { .. } => {
                        if let Some(peer_id) = &remote_peer_id {
                            let _ = state.touch_peer(peer_id).await;
                        }
                    }
                    WireMessage::ClipboardText { machine_id, text } => {
                        if machine_id != authenticated_peer_id {
                            warn!(
                                claimed_machine_id = %machine_id,
                                authenticated_machine_id = %authenticated_peer_id,
                                "dropping clipboard payload with mismatched machine_id"
                            );
                            continue;
                        }

                        if let Some(peer_id) = &remote_peer_id {
                            state.record_incoming_clipboard_text(peer_id, &text).await;
                            info!(
                                peer_id = %peer_id,
                                size_bytes = text.len(),
                                "received clipboard text payload"
                            );
                        }
                    }
                    WireMessage::FileStart {
                        machine_id,
                        transfer_id,
                        file_name,
                        total_bytes,
                    } => {
                        if machine_id != authenticated_peer_id {
                            warn!(
                                claimed_machine_id = %machine_id,
                                authenticated_machine_id = %authenticated_peer_id,
                                transfer_id = %transfer_id,
                                "dropping file start with mismatched machine_id"
                            );
                            continue;
                        }
                        validate_transfer_size(total_bytes).context("validate file start size")?;

                        if let Some(peer_id) = &remote_peer_id {
                            inbound_transfers.insert(
                                transfer_id.clone(),
                                InboundTransfer {
                                    peer_id: peer_id.clone(),
                                    file_name: file_name.clone(),
                                    total_bytes,
                                    bytes: Vec::new(),
                                },
                            );
                            info!(
                                peer_id = %peer_id,
                                transfer_id = %transfer_id,
                                file_name = %file_name,
                                total_bytes,
                                "started inbound file transfer"
                            );
                        }
                    }
                    WireMessage::FileChunk {
                        transfer_id,
                        data_b64,
                    } => {
                        let Some(transfer) = inbound_transfers.get_mut(&transfer_id) else {
                            warn!(transfer_id = %transfer_id, "received file chunk for unknown transfer");
                            continue;
                        };

                        let chunk = match decode_bytes_b64(&data_b64) {
                            Ok(chunk) => chunk,
                            Err(error) => {
                                warn!(transfer_id = %transfer_id, error = ?error, "failed to decode file chunk");
                                inbound_transfers.remove(&transfer_id);
                                continue;
                            }
                        };

                        let next_size = transfer.bytes.len() + chunk.len();
                        validate_transfer_size(next_size as u64).context("validate chunk size")?;

                        if next_size as u64 > transfer.total_bytes {
                            warn!(
                                transfer_id = %transfer_id,
                                announced_total = transfer.total_bytes,
                                attempted_total = next_size as u64,
                                "inbound file exceeded announced total bytes"
                            );
                            inbound_transfers.remove(&transfer_id);
                            continue;
                        }

                        transfer.bytes.extend_from_slice(&chunk);
                    }
                    WireMessage::FileEnd { transfer_id } => {
                        let Some(transfer) = inbound_transfers.remove(&transfer_id) else {
                            warn!(transfer_id = %transfer_id, "received file end for unknown transfer");
                            continue;
                        };

                        if transfer.bytes.len() as u64 != transfer.total_bytes {
                            warn!(
                                transfer_id = %transfer_id,
                                expected = transfer.total_bytes,
                                actual = transfer.bytes.len() as u64,
                                "inbound file transfer ended with size mismatch"
                            );
                            continue;
                        }

                        let InboundTransfer {
                            peer_id,
                            file_name,
                            total_bytes: _,
                            bytes,
                        } = transfer;

                        match state
                            .store_incoming_file(&peer_id, &file_name, bytes)
                            .await
                        {
                            Ok(path) => {
                                info!(
                                    peer_id = %peer_id,
                                    transfer_id = %transfer_id,
                                    file_name = %file_name,
                                    path = %path.display(),
                                    "stored inbound file payload"
                                );
                            }
                            Err(error) => {
                                warn!(
                                    peer_id = %peer_id,
                                    transfer_id = %transfer_id,
                                    error = ?error,
                                    "failed to store inbound file payload"
                                );
                            }
                        }
                    }
                    WireMessage::InputFrame {
                        machine_id,
                        sequence,
                        timestamp_unix_ms,
                        events,
                    } => {
                        if machine_id != authenticated_peer_id {
                            warn!(
                                claimed_machine_id = %machine_id,
                                authenticated_machine_id = %authenticated_peer_id,
                                "dropping input frame with mismatched machine_id"
                            );
                            continue;
                        }

                        if let Some(peer_id) = &remote_peer_id {
                            let frame = InputFrame {
                                source_peer_id: peer_id.clone(),
                                sequence,
                                timestamp_unix_ms,
                                events: events
                                    .into_iter()
                                    .map(input_event_from_wire)
                                    .collect(),
                            };

                            match state.route_incoming_input_frame(peer_id, frame).await {
                                Ok(decision) => {
                                    info!(
                                        peer_id = %peer_id,
                                        sequence,
                                        decision = ?decision,
                                        "processed inbound input frame"
                                    );
                                }
                                Err(error) => {
                                    warn!(
                                        peer_id = %peer_id,
                                        sequence,
                                        error = ?error,
                                        "failed to process inbound input frame"
                                    );
                                }
                            }
                        }
                    }
                    WireMessage::Error { message } => {
                        warn!(%message, "remote error frame");
                    }
                }
            }
        }
    }

    if let Some(peer_id) = &remote_peer_id {
        let _ = state.set_peer_connected(peer_id, false).await;
    }

    Ok(())
}

async fn authenticated_peer_machine_id<S>(
    state: &AppState,
    stream: &tokio_rustls::TlsStream<S>,
) -> Result<String> {
    let (_, session) = stream.get_ref();
    let peer_chain = session
        .peer_certificates()
        .context("missing peer certificate chain")?;

    if peer_chain.len() < 2 {
        bail!("peer TLS certificate chain must include issuer CA certificate");
    }

    let presented_ca = peer_chain
        .last()
        .context("peer certificate chain is unexpectedly empty")?;

    let trusted = state.trusted_records().await?;
    let Some(machine_id) = machine_id_from_presented_ca(&trusted, presented_ca)? else {
        bail!("presented peer CA certificate does not map to a trusted machine record");
    };

    Ok(machine_id)
}

fn machine_id_from_presented_ca(
    records: &[core_security::TrustRecord],
    presented_ca: &CertificateDer<'_>,
) -> Result<Option<String>> {
    let mut matched_machine_id: Option<String> = None;

    for record in records {
        for cert in CertificateDer::pem_slice_iter(record.ca_cert_pem.as_bytes()) {
            let cert = cert.context("parse trusted CA certificate")?;
            if cert.as_ref() != presented_ca.as_ref() {
                continue;
            }

            if let Some(existing) = &matched_machine_id {
                if existing != &record.machine_id {
                    bail!("presented CA certificate matched multiple machine records");
                }
            } else {
                matched_machine_id = Some(record.machine_id.clone());
            }
        }
    }

    Ok(matched_machine_id)
}

async fn send_message<W>(writer: &mut W, message: &WireMessage) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(encode_line(message)?.as_bytes())
        .await
        .context("write transport frame")?;
    writer.flush().await.context("flush transport frame")?;
    Ok(())
}

async fn flush_outgoing_payloads<W>(
    state: &AppState,
    local_machine_id: &str,
    remote_peer_id: Option<&str>,
    writer: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(peer_id) = remote_peer_id else {
        return Ok(());
    };

    let mut pending = VecDeque::from(state.drain_outgoing(peer_id).await);
    while let Some(payload) = pending.pop_front() {
        if let Err(error) =
            send_outbound_payload(state, local_machine_id, peer_id, &payload, writer).await
        {
            let mut unsent = Vec::with_capacity(pending.len() + 1);
            unsent.push(payload);
            unsent.extend(pending.into_iter());
            state.requeue_outgoing_front(peer_id, unsent).await;
            return Err(error);
        }
    }

    Ok(())
}

async fn send_outbound_payload<W>(
    state: &AppState,
    local_machine_id: &str,
    peer_id: &str,
    payload: &OutboundPayload,
    writer: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    match payload {
        OutboundPayload::ClipboardText { text } => {
            let message = WireMessage::ClipboardText {
                machine_id: local_machine_id.to_string(),
                text: text.clone(),
            };
            send_message(writer, &message).await?;
            state.record_outgoing_clipboard_text(peer_id, text).await;
        }
        OutboundPayload::File { file_name, bytes } => {
            let total_bytes = bytes.len() as u64;
            validate_transfer_size(total_bytes)?;

            let transfer_id = uuid::Uuid::new_v4().to_string();
            send_message(
                writer,
                &WireMessage::FileStart {
                    machine_id: local_machine_id.to_string(),
                    transfer_id: transfer_id.clone(),
                    file_name: file_name.clone(),
                    total_bytes,
                },
            )
            .await?;

            for chunk in bytes.chunks(FILE_CHUNK_BYTES) {
                send_message(
                    writer,
                    &WireMessage::FileChunk {
                        transfer_id: transfer_id.clone(),
                        data_b64: encode_bytes_b64(chunk),
                    },
                )
                .await?;
            }

            send_message(writer, &WireMessage::FileEnd { transfer_id }).await?;
            state
                .record_outgoing_file(peer_id, file_name, total_bytes)
                .await;
        }
        OutboundPayload::InputFrame {
            sequence,
            timestamp_unix_ms,
            events,
        } => {
            send_message(
                writer,
                &WireMessage::InputFrame {
                    machine_id: local_machine_id.to_string(),
                    sequence: *sequence,
                    timestamp_unix_ms: *timestamp_unix_ms,
                    events: events.iter().map(input_event_to_wire).collect(),
                },
            )
            .await?;

            state
                .record_outgoing_input_frame(peer_id, events.len())
                .await;
        }
    }

    Ok(())
}

fn input_event_to_wire(event: &InputEvent) -> WireInputEvent {
    match event {
        InputEvent::MouseMove { dx, dy } => WireInputEvent::MouseMove { dx: *dx, dy: *dy },
        InputEvent::MouseButton { button, state } => WireInputEvent::MouseButton {
            button: match button {
                MouseButton::Left => WireMouseButton::Left,
                MouseButton::Right => WireMouseButton::Right,
                MouseButton::Middle => WireMouseButton::Middle,
                MouseButton::X1 => WireMouseButton::X1,
                MouseButton::X2 => WireMouseButton::X2,
            },
            state: match state {
                KeyState::Down => WireKeyState::Down,
                KeyState::Up => WireKeyState::Up,
            },
        },
        InputEvent::MouseWheel { delta_x, delta_y } => WireInputEvent::MouseWheel {
            delta_x: *delta_x,
            delta_y: *delta_y,
        },
        InputEvent::Key { scan_code, state } => WireInputEvent::Key {
            scan_code: *scan_code,
            state: match state {
                KeyState::Down => WireKeyState::Down,
                KeyState::Up => WireKeyState::Up,
            },
        },
    }
}

fn input_event_from_wire(event: WireInputEvent) -> InputEvent {
    match event {
        WireInputEvent::MouseMove { dx, dy } => InputEvent::MouseMove { dx, dy },
        WireInputEvent::MouseButton { button, state } => InputEvent::MouseButton {
            button: match button {
                WireMouseButton::Left => MouseButton::Left,
                WireMouseButton::Right => MouseButton::Right,
                WireMouseButton::Middle => MouseButton::Middle,
                WireMouseButton::X1 => MouseButton::X1,
                WireMouseButton::X2 => MouseButton::X2,
            },
            state: match state {
                WireKeyState::Down => KeyState::Down,
                WireKeyState::Up => KeyState::Up,
            },
        },
        WireInputEvent::MouseWheel { delta_x, delta_y } => {
            InputEvent::MouseWheel { delta_x, delta_y }
        }
        WireInputEvent::Key { scan_code, state } => InputEvent::Key {
            scan_code,
            state: match state {
                WireKeyState::Down => KeyState::Down,
                WireKeyState::Up => KeyState::Up,
            },
        },
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn build_tls_acceptor(state: &AppState) -> Result<TlsAcceptor> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("build client verifier")?;

    let cert_chain = build_presented_cert_chain(&identity)?;
    let private_key = parse_private_key(&identity.device_key_pem)?;

    let server = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server tls config")?;

    Ok(TlsAcceptor::from(Arc::new(server)))
}

async fn build_tls_connector(state: &AppState) -> Result<TlsConnector> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;

    let cert_chain = build_presented_cert_chain(&identity)?;
    let private_key = parse_private_key(&identity.device_key_pem)?;

    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, private_key)
        .context("build client tls config")?;

    Ok(TlsConnector::from(Arc::new(client)))
}

fn build_root_store(records: &[core_security::TrustRecord]) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();

    for record in records {
        for cert in CertificateDer::pem_slice_iter(record.ca_cert_pem.as_bytes()) {
            roots
                .add(cert.context("parse trusted CA certificate")?)
                .context("add trusted CA certificate")?;
        }
    }

    Ok(roots)
}

fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse cert chain")
}

fn build_presented_cert_chain(
    identity: &core_security::DeviceIdentity,
) -> Result<Vec<CertificateDer<'static>>> {
    let mut chain = parse_cert_chain(&identity.device_cert_pem)?;
    let mut ca_chain = parse_cert_chain(&identity.ca_cert_pem)?;
    chain.append(&mut ca_chain);
    Ok(chain)
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parse private key")
}

fn parse_server_name(address: &str) -> Result<ServerName<'static>> {
    if let Ok(socket) = address.parse::<std::net::SocketAddr>() {
        return ServerName::try_from(socket.ip().to_string())
            .context("parse server name from socket address");
    }

    let host = if address.starts_with('[') {
        address
            .split(']')
            .next()
            .map(|s| s.trim_start_matches('[').to_string())
            .unwrap_or_else(|| address.to_string())
    } else {
        address
            .rsplit_once(':')
            .map(|(host, _)| host.to_string())
            .unwrap_or_else(|| address.to_string())
    };

    ServerName::try_from(host).context("parse server name")
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
        let _err = flush_outgoing_payloads(&state, "local", Some(&peer_id), &mut writer)
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

        state
            .queue_clipboard_text(&peer_id, "one".to_string())
            .await
            .expect("queue one");
        state
            .queue_clipboard_text(&peer_id, "two".to_string())
            .await
            .expect("queue two");
        state
            .queue_clipboard_text(&peer_id, "three".to_string())
            .await
            .expect("queue three");

        // Each successful payload costs write+flush (2 calls). Fail on second payload write.
        let mut writer = FailAfterCallsWriter::new(3);
        let _ = flush_outgoing_payloads(&state, "local", Some(&peer_id), &mut writer)
            .await
            .expect_err("must fail");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "two"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text == "three"
        ));

        let _ = std::fs::remove_dir_all(root);
    }
}
