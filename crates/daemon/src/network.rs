use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
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
    PROTOCOL_CLIPBOARD_IMAGE_MIN, PROTOCOL_CURRENT, PROTOCOL_INPUT_ANCHOR_MIN, ProtocolVersion,
    WireInputEvent, WireKeyState, WireMessage, WireMouseButton, decode_bytes_b64, decode_line,
    encode_bytes_b64, encode_line,
};
use core_transfer::validate_transfer_size;

use crate::state::{AppState, OutboundPayload};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const OUTGOING_FLUSH_INTERVAL: Duration = Duration::from_millis(20);
const SUPERVISOR_TICK: Duration = Duration::from_secs(3);
const MAX_BACKOFF_SECONDS: u64 = 30;
const FILE_CHUNK_BYTES: usize = 48 * 1024;
const FALLBACK_BIND_HOST: &str = "0.0.0.0";

#[derive(Debug)]
struct InboundTransfer {
    peer_id: String,
    file_name: String,
    total_bytes: u64,
    bytes: Vec<u8>,
}

pub fn start(state: AppState, listener: Option<TcpListener>) {
    if let Some(listener) = listener {
        tokio::spawn(listener_loop(state.clone(), listener));
    } else {
        warn!("transport listener not started");
    }
    tokio::spawn(supervisor_loop(state));
}

async fn listener_loop(state: AppState, listener: TcpListener) {
    let bind = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    info!(bind = %bind, "transport listener started");

    loop {
        match listener.accept().await {
            Ok((socket, remote)) => {
                let task_state = state.clone();
                let registration_state = state.clone();
                let (session_id_tx, session_id_rx) = oneshot::channel::<u64>();
                let task = tokio::spawn(async move {
                    let session_id = session_id_rx.await.ok();
                    if let Err(error) =
                        handle_incoming_connection(task_state, socket, session_id).await
                    {
                        warn!(error = ?error, remote = %remote, "incoming session ended with error");
                    }
                });

                let session_id = registration_state
                    .register_pending_transport_session(task.abort_handle())
                    .await;
                let _ = session_id_tx.send(session_id);
            }
            Err(error) => {
                warn!(%error, "transport accept failed");
                time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
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

async fn supervisor_loop(state: AppState) {
    let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut worker_session_ids: HashMap<String, u64> = HashMap::new();

    loop {
        let finished_peers = workers
            .iter()
            .filter_map(|(peer_id, handle)| {
                if handle.is_finished() {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for peer_id in finished_peers {
            workers.remove(&peer_id);
            if let Some(session_id) = worker_session_ids.remove(&peer_id) {
                state.clear_transport_session_registration(session_id).await;
            }
        }

        let snapshot = state.snapshot().await;
        for peer in snapshot.peers {
            if peer.peer_id == snapshot.machine_id {
                continue;
            }

            let has_manual_address = !peer.address.trim().is_empty();
            let has_discovered_endpoint = state.discovered_endpoint(&peer.peer_id).await.is_some();
            if !has_manual_address && !has_discovered_endpoint {
                continue;
            }

            if workers.contains_key(&peer.peer_id) {
                continue;
            }

            let worker_state = state.clone();
            let registration_state = state.clone();
            let peer_id = peer.peer_id.clone();
            let handle = tokio::spawn(async move {
                peer_worker(worker_state, peer_id).await;
            });
            let session_id = registration_state
                .register_transport_session_for_peer(&peer.peer_id, handle.abort_handle())
                .await;
            workers.insert(peer.peer_id.clone(), handle);
            worker_session_ids.insert(peer.peer_id, session_id);
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

        let discovered_endpoint = state.discovered_endpoint(&peer_id).await;
        let target_candidates = outbound_target_candidates(&peer.address, discovered_endpoint);
        if target_candidates.is_empty() {
            time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            continue;
        }

        let mut connected = false;
        let mut last_error: Option<anyhow::Error> = None;
        for target_address in &target_candidates {
            match connect_and_run_outbound(state.clone(), &peer_id, target_address).await {
                Ok(()) => {
                    backoff_secs = 1;
                    connected = true;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        if !connected {
            warn!(
                peer_id = %peer_id,
                configured_address = %peer.address,
                target_candidates = ?target_candidates,
                discovered_endpoint = ?discovered_endpoint,
                error = ?last_error,
                "outbound connect failed"
            );
            if let Err(mark_error) = state.set_peer_connected(&peer_id, false).await {
                warn!(%mark_error, "failed to mark peer disconnected");
            }

            time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
        }
    }
}

fn outbound_target_candidates(
    configured_address: &str,
    discovered_endpoint: Option<SocketAddr>,
) -> Vec<String> {
    let mut targets = Vec::new();

    if let Some(endpoint) = discovered_endpoint {
        targets.push(endpoint.to_string());
    }

    let manual = configured_address.trim();
    if !manual.is_empty() && !targets.iter().any(|target| target == manual) {
        targets.push(manual.to_string());
    }

    targets
}

async fn connect_and_run_outbound(state: AppState, peer_id: &str, address: &str) -> Result<()> {
    let socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("tcp connect {address}"))?;

    let connector = build_tls_connector(&state).await?;
    let server_name = parse_server_name_for_peer(peer_id, address)?;
    let stream = connector
        .connect(server_name, socket)
        .await
        .with_context(|| format!("tls connect {address}"))?;

    run_session(
        state,
        Some(peer_id.to_string()),
        tokio_rustls::TlsStream::Client(stream),
        true,
        None,
    )
    .await
}

async fn handle_incoming_connection(
    state: AppState,
    socket: TcpStream,
    session_registration_id: Option<u64>,
) -> Result<()> {
    let acceptor = build_tls_acceptor(&state).await?;
    let stream = acceptor.accept(socket).await.context("tls accept")?;
    let result = run_session(
        state.clone(),
        None,
        tokio_rustls::TlsStream::Server(stream),
        false,
        session_registration_id,
    )
    .await;

    if let Some(session_id) = session_registration_id {
        state.clear_transport_session_registration(session_id).await;
    }

    result
}

async fn run_session<S>(
    state: AppState,
    peer_hint: Option<String>,
    stream: tokio_rustls::TlsStream<S>,
    is_outbound: bool,
    session_registration_id: Option<u64>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let authenticated_peer_id = authenticated_peer_machine_id(&state, &stream).await?;
    if let Some(session_id) = session_registration_id {
        state
            .bind_pending_transport_session_to_peer(session_id, &authenticated_peer_id)
            .await;
    }
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
    let mut heartbeat_interval = time::interval(HEARTBEAT_INTERVAL);
    let mut outgoing_flush_interval = time::interval(OUTGOING_FLUSH_INTERVAL);

    let snapshot = state.snapshot().await;
    let local_hello = WireMessage::Hello {
        machine_id: snapshot.machine_id.clone(),
        display_name: snapshot.device_name.clone(),
        protocol: PROTOCOL_CURRENT,
        capability_count: core_protocol::default_capabilities().len(),
    };

    send_message(&mut writer, &local_hello).await?;

    let remote_peer_id = Some(authenticated_peer_id.clone());
    let mut observed_reconnect_generation = state
        .peer_reconnect_generation(&authenticated_peer_id)
        .await;
    let mut remote_protocol: Option<ProtocolVersion> = None;
    let mut inbound_transfers: HashMap<String, InboundTransfer> = HashMap::new();

    loop {
        tokio::select! {
            _ = heartbeat_interval.tick() => {
                if reconnect_requested_for_peer(
                    &state,
                    &authenticated_peer_id,
                    &mut observed_reconnect_generation,
                )
                .await
                {
                    info!(
                        peer_id = %authenticated_peer_id,
                        "ending session due to explicit reconnect request"
                    );
                    break;
                }

                let heartbeat = WireMessage::Heartbeat {
                    machine_id: snapshot.machine_id.clone(),
                    timestamp_unix_ms: now_millis(),
                };
                send_message(&mut writer, &heartbeat).await?;
                if let Some(remote_protocol) = remote_protocol {
                    flush_outgoing_payloads(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        &mut writer,
                    )
                    .await?;
                }
            }
            _ = outgoing_flush_interval.tick(), if remote_protocol.is_some() => {
                if reconnect_requested_for_peer(
                    &state,
                    &authenticated_peer_id,
                    &mut observed_reconnect_generation,
                )
                .await
                {
                    info!(
                        peer_id = %authenticated_peer_id,
                        "ending session due to explicit reconnect request"
                    );
                    break;
                }

                if let Some(remote_protocol) = remote_protocol {
                    flush_outgoing_payloads(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        &mut writer,
                    )
                    .await?;
                }
            }
            read = reader.read_line(&mut line) => {
                if reconnect_requested_for_peer(
                    &state,
                    &authenticated_peer_id,
                    &mut observed_reconnect_generation,
                )
                .await
                {
                    info!(
                        peer_id = %authenticated_peer_id,
                        "ending session due to explicit reconnect request"
                    );
                    break;
                }

                let read = read.context("read transport line")?;
                if read == 0 {
                    break;
                }

                let message = decode_line(&line).context("decode wire message")?;
                line.clear();

                match message {
                    WireMessage::Hello {
                        machine_id,
                        protocol,
                        ..
                    } => {
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
                        remote_protocol = Some(protocol);

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

                        if let Some(remote_protocol) = remote_protocol {
                            flush_outgoing_payloads(
                                &state,
                                &snapshot.machine_id,
                                remote_peer_id.as_deref(),
                                remote_protocol,
                                &mut writer,
                            )
                            .await?;
                        }
                    }
                    WireMessage::HelloAck { accepted, .. } => {
                        if accepted && let Some(peer_id) = &remote_peer_id {
                            let _ = state.set_peer_connected(peer_id, true).await;
                        }

                        if let Some(remote_protocol) = remote_protocol {
                            flush_outgoing_payloads(
                                &state,
                                &snapshot.machine_id,
                                remote_peer_id.as_deref(),
                                remote_protocol,
                                &mut writer,
                            )
                            .await?;
                        }
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
                            if let Err(error) =
                                state.enqueue_remote_clipboard_text(peer_id, text.clone()).await
                            {
                                warn!(
                                    peer_id = %peer_id,
                                    error = ?error,
                                    "failed to enqueue incoming clipboard text payload"
                                );
                            } else {
                                info!(
                                    peer_id = %peer_id,
                                    size_bytes = text.len(),
                                    "received clipboard text payload"
                                );
                            }
                        }
                    }
                    WireMessage::ClipboardImage {
                        machine_id,
                        data_b64,
                    } => {
                        if !remote_protocol.is_some_and(protocol_supports_clipboard_image) {
                            warn!(
                                peer_id = %authenticated_peer_id,
                                remote_protocol = ?remote_protocol,
                                required_protocol = %PROTOCOL_CLIPBOARD_IMAGE_MIN,
                                "dropping clipboard image payload from peer without image-frame support"
                            );
                            continue;
                        }

                        if machine_id != authenticated_peer_id {
                            warn!(
                                claimed_machine_id = %machine_id,
                                authenticated_machine_id = %authenticated_peer_id,
                                "dropping clipboard image payload with mismatched machine_id"
                            );
                            continue;
                        }

                        let image_bmp = match decode_bytes_b64(&data_b64) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                warn!(error = ?error, "failed to decode clipboard image payload");
                                continue;
                            }
                        };

                        if let Some(peer_id) = &remote_peer_id {
                            let size_bytes = image_bmp.len();
                            if let Err(error) = state
                                .enqueue_remote_clipboard_image(peer_id, image_bmp)
                                .await
                            {
                                warn!(
                                    peer_id = %peer_id,
                                    error = ?error,
                                    "failed to enqueue incoming clipboard image payload"
                                );
                            } else {
                                info!(
                                    peer_id = %peer_id,
                                    size_bytes,
                                    "received clipboard image payload"
                                );
                            }
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

async fn reconnect_requested_for_peer(
    state: &AppState,
    peer_id: &str,
    observed_generation: &mut u64,
) -> bool {
    let current_generation = state.peer_reconnect_generation(peer_id).await;
    if current_generation <= *observed_generation {
        return false;
    }

    *observed_generation = current_generation;
    true
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
    remote_protocol: ProtocolVersion,
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
        if let Err(error) = send_outbound_payload(
            state,
            local_machine_id,
            peer_id,
            remote_protocol,
            &payload,
            writer,
        )
        .await
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
    remote_protocol: ProtocolVersion,
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
        OutboundPayload::ClipboardImage { image_bmp } => {
            if !protocol_supports_clipboard_image(remote_protocol) {
                warn!(
                    peer_id = %peer_id,
                    remote_protocol = %remote_protocol,
                    required_protocol = %PROTOCOL_CLIPBOARD_IMAGE_MIN,
                    "dropping clipboard image payload for peer without image-frame support"
                );
                return Ok(());
            }

            let message = WireMessage::ClipboardImage {
                machine_id: local_machine_id.to_string(),
                data_b64: encode_bytes_b64(image_bmp),
            };
            send_message(writer, &message).await?;
            state
                .record_outgoing_clipboard_image(peer_id, image_bmp.len())
                .await;
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
            let wire_events = input_events_to_wire_for_protocol(events, remote_protocol);
            if wire_events.is_empty() {
                warn!(
                    peer_id = %peer_id,
                    sequence = *sequence,
                    remote_protocol = %remote_protocol,
                    required_protocol = %PROTOCOL_INPUT_ANCHOR_MIN,
                    "dropping input frame with unsupported events for negotiated protocol"
                );
                return Ok(());
            }

            send_message(
                writer,
                &WireMessage::InputFrame {
                    machine_id: local_machine_id.to_string(),
                    sequence: *sequence,
                    timestamp_unix_ms: *timestamp_unix_ms,
                    events: wire_events,
                },
            )
            .await?;

            state
                .record_outgoing_input_frame(peer_id, *sequence, events.len(), *timestamp_unix_ms)
                .await;
        }
    }

    Ok(())
}

fn protocol_supports_clipboard_image(protocol: ProtocolVersion) -> bool {
    protocol.as_tuple() >= PROTOCOL_CLIPBOARD_IMAGE_MIN.as_tuple()
}

fn protocol_supports_input_anchor(protocol: ProtocolVersion) -> bool {
    protocol.as_tuple() >= PROTOCOL_INPUT_ANCHOR_MIN.as_tuple()
}

fn input_events_to_wire_for_protocol(
    events: &[InputEvent],
    remote_protocol: ProtocolVersion,
) -> Vec<WireInputEvent> {
    events
        .iter()
        .filter_map(|event| {
            if matches!(event, InputEvent::MouseMoveAbsolute { .. })
                && !protocol_supports_input_anchor(remote_protocol)
            {
                return None;
            }
            Some(input_event_to_wire(event))
        })
        .collect()
}

fn input_event_to_wire(event: &InputEvent) -> WireInputEvent {
    match event {
        InputEvent::MouseMove { dx, dy } => WireInputEvent::MouseMove { dx: *dx, dy: *dy },
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => WireInputEvent::MouseMoveAbsolute {
            x_norm: *x_norm,
            y_norm: *y_norm,
        },
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
        WireInputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            InputEvent::MouseMoveAbsolute { x_norm, y_norm }
        }
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

fn parse_server_name_for_peer(peer_id: &str, address: &str) -> Result<ServerName<'static>> {
    if !peer_id.trim().is_empty()
        && let Ok(server_name) = ServerName::try_from(peer_id.to_string())
    {
        return Ok(server_name);
    }

    parse_server_name(address)
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
    fn clipboard_image_support_requires_protocol_1_1_or_newer() {
        assert!(!protocol_supports_clipboard_image(ProtocolVersion {
            major: 1,
            minor: 0,
            patch: 9,
        }));
        assert!(protocol_supports_clipboard_image(ProtocolVersion {
            major: 1,
            minor: 1,
            patch: 0,
        }));
    }

    #[test]
    fn input_anchor_support_requires_protocol_1_2_or_newer() {
        assert!(!protocol_supports_input_anchor(ProtocolVersion {
            major: 1,
            minor: 1,
            patch: 9,
        }));
        assert!(protocol_supports_input_anchor(ProtocolVersion {
            major: 1,
            minor: 2,
            patch: 0,
        }));
    }

    #[test]
    fn input_wire_filter_drops_absolute_move_for_legacy_peer() {
        let events = vec![
            InputEvent::MouseMoveAbsolute {
                x_norm: 10,
                y_norm: 20,
            },
            InputEvent::MouseMove { dx: 3, dy: -4 },
        ];
        let wire = input_events_to_wire_for_protocol(
            &events,
            ProtocolVersion {
                major: 1,
                minor: 1,
                patch: 0,
            },
        );

        assert_eq!(wire.len(), 1);
        assert!(matches!(
            wire.first(),
            Some(WireInputEvent::MouseMove { dx, dy }) if *dx == 3 && *dy == -4
        ));
    }

    #[test]
    fn input_wire_filter_keeps_absolute_move_for_supported_peer() {
        let events = vec![InputEvent::MouseMoveAbsolute {
            x_norm: 100,
            y_norm: 200,
        }];
        let wire = input_events_to_wire_for_protocol(
            &events,
            ProtocolVersion {
                major: 1,
                minor: 2,
                patch: 0,
            },
        );

        assert!(matches!(
            wire.first(),
            Some(WireInputEvent::MouseMoveAbsolute { x_norm, y_norm })
                if *x_norm == 100 && *y_norm == 200
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
            Some(OutboundPayload::ClipboardText { text }) if text == "two"
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text == "three"
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn flush_drops_clipboard_image_for_legacy_protocol_peer() {
        let (state, peer_id, root) = state_with_peer_for_queue_test().await;
        state
            .queue_clipboard_image(&peer_id, minimal_bmp_payload())
            .await
            .expect("queue image");

        let mut writer = FailAfterCallsWriter::new(1);
        flush_outgoing_payloads(
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
        .expect("legacy peer image should be dropped, not sent");

        let queued = state.drain_outgoing(&peer_id).await;
        assert!(queued.is_empty(), "dropped image must not be requeued");

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
