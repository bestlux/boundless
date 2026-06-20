use std::collections::HashMap;

use chrono::Utc;
use peer_transport::{
    DEFAULT_TRANSPORT_TUNING, InboundClipboardImageTransfer, InboundTransfer,
    OutboundTransferFlows, apply_outbound_chunk_credits, reconnect_generation_advanced,
    remove_outbound_transfer_flow,
};

use crate::state::TransportEventRecord;

use super::codec::now_millis;
use super::control::{
    HelloHandling, handle_anti_idle_pulse_message, handle_heartbeat_message,
    handle_hello_ack_message, handle_hello_message,
};
use super::inbound::{
    discard_inbound_clipboard_image_transfer, discard_inbound_transfer,
    handle_clipboard_image_chunk, handle_clipboard_image_end, handle_clipboard_image_start,
    handle_file_chunk, handle_file_end, handle_file_start,
};
use super::inbound_payload::{
    handle_clipboard_image_message, handle_clipboard_text_message, handle_input_frame_message,
};
use super::outbound::{
    flush_outgoing_bulk_payloads_with_buffer, flush_outgoing_input_payloads_with_buffer,
};
use super::*;

pub(super) async fn connect_and_run_outbound(
    state: AppState,
    peer_id: &str,
    address: &str,
) -> Result<()> {
    let socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("tcp connect {address}"))?;
    configure_low_latency_socket(&socket).context("configure outbound low-latency socket")?;

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

pub(super) async fn handle_incoming_connection(
    state: AppState,
    socket: TcpStream,
    session_registration_id: Option<u64>,
) -> Result<()> {
    configure_low_latency_socket(&socket).context("configure inbound low-latency socket")?;
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

pub(super) fn configure_low_latency_socket(socket: &TcpStream) -> Result<()> {
    socket.set_nodelay(true).context("set TCP_NODELAY")?;
    Ok(())
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

    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let mut frame_payload = Vec::<u8>::with_capacity(4096);
    let mut write_frame_buffer = Vec::<u8>::with_capacity(4096);
    let mut heartbeat_interval = time::interval(DEFAULT_TRANSPORT_TUNING.heartbeat_interval);
    let mut outgoing_input_flush_interval =
        time::interval(DEFAULT_TRANSPORT_TUNING.outgoing_input_flush_interval);
    let mut outgoing_bulk_flush_interval =
        time::interval(DEFAULT_TRANSPORT_TUNING.outgoing_bulk_flush_interval);
    let mut outgoing_flush_signal = state.subscribe_outgoing_flush_signal();
    heartbeat_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    outgoing_input_flush_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    outgoing_bulk_flush_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let snapshot = state.snapshot().await;
    let local_hello = WireMessage::Hello {
        machine_id: snapshot.machine_id.clone(),
        display_name: snapshot.device_name.clone(),
        protocol: PROTOCOL_CURRENT,
        capability_count: core_protocol::default_capabilities().len(),
    };

    send_message(&mut writer, &local_hello, &mut write_frame_buffer).await?;
    writer.flush().await.context("flush local hello")?;

    let remote_peer_id = Some(authenticated_peer_id.clone());
    let mut observed_reconnect_generation = state
        .peer_reconnect_generation(&authenticated_peer_id)
        .await;
    let mut remote_protocol: Option<ProtocolVersion> = None;
    let mut inbound_transfers: HashMap<String, InboundTransfer> = HashMap::new();
    let mut inbound_clipboard_image_transfers: HashMap<String, InboundClipboardImageTransfer> =
        HashMap::new();
    let mut outbound_transfer_flow: OutboundTransferFlows = HashMap::new();
    let mut last_anti_idle_pulse_sent_at: Option<std::time::Instant> = None;

    let session_result: Result<()> = {
        async {
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
                send_message(&mut writer, &heartbeat, &mut write_frame_buffer).await?;
                if let Some(pulse) = state.anti_idle_outbound_pulse().await
                    && last_anti_idle_pulse_sent_at
                        .is_none_or(|last| last.elapsed() >= pulse.interval)
                {
                    send_message(
                        &mut writer,
                        &WireMessage::AntiIdlePulse {
                            keep_display_on: pulse.keep_display_on,
                        },
                        &mut write_frame_buffer,
                    )
                    .await?;
                    state.record_transport_event(TransportEventRecord {
                        timestamp: Utc::now(),
                        direction: "outgoing".to_string(),
                        kind: "anti_idle_pulse_sent".to_string(),
                        peer_id: authenticated_peer_id.clone(),
                        detail: format!("keep_display_on={}", pulse.keep_display_on),
                        size_bytes: 0,
                    });
                    last_anti_idle_pulse_sent_at = Some(std::time::Instant::now());
                }
                if let Some(remote_protocol) = remote_protocol {
                    flush_outgoing_input_payloads_with_buffer(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        &mut outbound_transfer_flow,
                        &mut writer,
                        &mut write_frame_buffer,
                    )
                    .await?;
                    flush_outgoing_bulk_payloads_with_buffer(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
                        &mut outbound_transfer_flow,
                        &mut writer,
                        &mut write_frame_buffer,
                    )
                    .await?;
                }
                writer.flush().await.context("flush heartbeat batch")?;
            }
            _ = outgoing_input_flush_interval.tick(), if remote_protocol.is_some() => {
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
                    flush_outgoing_input_payloads_with_buffer(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        &mut outbound_transfer_flow,
                        &mut writer,
                        &mut write_frame_buffer,
                    )
                    .await?;
                }
            }
            _ = outgoing_bulk_flush_interval.tick(), if remote_protocol.is_some() => {
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
                    flush_outgoing_bulk_payloads_with_buffer(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
                        &mut outbound_transfer_flow,
                        &mut writer,
                        &mut write_frame_buffer,
                    )
                    .await?;
                }
            }
            changed = outgoing_flush_signal.changed(), if remote_protocol.is_some() => {
                if changed.is_err() {
                    // State dropped; session will naturally unwind shortly.
                    break;
                }

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
                    flush_outgoing_input_payloads_with_buffer(
                        &state,
                        &snapshot.machine_id,
                        remote_peer_id.as_deref(),
                        remote_protocol,
                        &mut outbound_transfer_flow,
                        &mut writer,
                        &mut write_frame_buffer,
                    )
                    .await?;
                }
            }
            read = read_wire_frame_payload(&mut reader, &mut frame_payload) => {
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

                let Some(read) = (match read {
                    Ok(read) => read,
                    Err(error) => {
                        record_transport_frame_rejected(
                            &state,
                            &authenticated_peer_id,
                            format!("reason=invalid_frame error={error:#}"),
                            frame_payload.len() as u64,
                        )
                        .await;
                        warn!(
                            peer_id = %authenticated_peer_id,
                            error = ?error,
                            "transport frame rejected"
                        );
                        break;
                    }
                }) else {
                    break;
                };

                let message = match decode_frame_payload(&frame_payload) {
                    Ok(message) => message,
                    Err(error) => {
                        record_transport_frame_rejected(
                            &state,
                            &authenticated_peer_id,
                            format!("reason=decode_failed error={error}"),
                            read as u64,
                        )
                        .await;
                        warn!(
                            peer_id = %authenticated_peer_id,
                            error = ?error,
                            "dropping undecodable wire message"
                        );
                        continue;
                    }
                };

                match message {
                    WireMessage::Hello {
                        machine_id,
                        protocol,
                        ..
                    } => {
                        let handling = handle_hello_message(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            is_outbound,
                            &snapshot.machine_id,
                            machine_id,
                            protocol,
                            &mut remote_protocol,
                            &mut outbound_transfer_flow,
                            &mut writer,
                            &mut write_frame_buffer,
                        )
                        .await?;
                        if matches!(handling, HelloHandling::TerminateSession) {
                            break;
                        }
                    }
                    WireMessage::HelloAck { accepted, .. } => {
                        handle_hello_ack_message(
                            &state,
                            remote_peer_id.as_deref(),
                            &snapshot.machine_id,
                            remote_protocol,
                            &mut outbound_transfer_flow,
                            accepted,
                            &mut writer,
                            &mut write_frame_buffer,
                        )
                        .await?;
                    }
                    WireMessage::Heartbeat { .. } => {
                        handle_heartbeat_message(&state, remote_peer_id.as_deref()).await;
                    }
                    WireMessage::AntiIdlePulse { keep_display_on } => {
                        handle_anti_idle_pulse_message(
                            &state,
                            remote_peer_id.as_deref(),
                            keep_display_on,
                        )
                        .await;
                    }
                    WireMessage::ClipboardText { machine_id, text } => {
                        handle_clipboard_text_message(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            machine_id,
                            text,
                        )
                        .await;
                    }
                    WireMessage::ClipboardImage {
                        machine_id,
                        data,
                    } => {
                        handle_clipboard_image_message(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            machine_id,
                            data,
                        )
                        .await;
                    }
                    WireMessage::ClipboardImageStart {
                        machine_id,
                        transfer_id,
                        total_bytes,
                        hash_hex,
                        ..
                    } => {
                        handle_clipboard_image_start(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            machine_id,
                            transfer_id,
                            total_bytes,
                            hash_hex,
                            &mut inbound_clipboard_image_transfers,
                        )
                        .await?;
                    }
                    WireMessage::ClipboardImageChunk { transfer_id, data } => {
                        handle_clipboard_image_chunk(
                            &state,
                            transfer_id,
                            data,
                            &mut inbound_clipboard_image_transfers,
                        )
                        .await?;
                    }
                    WireMessage::ClipboardImageEnd { transfer_id, .. } => {
                        handle_clipboard_image_end(
                            &state,
                            transfer_id,
                            &mut inbound_clipboard_image_transfers,
                        )
                        .await?;
                    }
                    WireMessage::FileStart {
                        machine_id,
                        transfer_id,
                        file_name,
                        total_bytes,
                    } => {
                        handle_file_start(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            machine_id,
                            transfer_id,
                            file_name,
                            total_bytes,
                            &mut inbound_transfers,
                            &mut writer,
                            &mut write_frame_buffer,
                        )
                        .await?;
                    }
                    WireMessage::FileChunk {
                        transfer_id,
                        data,
                    } => {
                        handle_file_chunk(
                            &state,
                            transfer_id,
                            data,
                            &mut inbound_transfers,
                            &mut writer,
                            &mut write_frame_buffer,
                        )
                        .await?;
                    }
                    WireMessage::FileChunkCredit {
                        transfer_id,
                        chunk_credits,
                    } => {
                        if chunk_credits == 0 {
                            continue;
                        }

                        let Some(_current_credits) = apply_outbound_chunk_credits(
                            &mut outbound_transfer_flow,
                            &transfer_id,
                            chunk_credits,
                        ) else {
                            warn!(
                                transfer_id = %transfer_id,
                                chunk_credits,
                                "dropping file chunk credit for unknown outbound transfer"
                            );
                            continue;
                        };

                        if let Some(remote_protocol) = remote_protocol {
                            flush_outgoing_bulk_payloads_with_buffer(
                                &state,
                                &snapshot.machine_id,
                                remote_peer_id.as_deref(),
                                remote_protocol,
                                DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
                                &mut outbound_transfer_flow,
                                &mut writer,
                                &mut write_frame_buffer,
                            )
                            .await?;
                            writer
                                .flush()
                                .await
                                .context("flush outbound bulk after receiving file chunk credit")?;
                        }
                    }
                    WireMessage::FileTransferRejected {
                        transfer_id,
                        reason,
                    } => {
                        handle_file_transfer_rejected(
                            &state,
                            remote_peer_id.as_deref(),
                            transfer_id,
                            reason,
                            &mut outbound_transfer_flow,
                        )
                        .await;
                    }
                    WireMessage::FileEnd { transfer_id } => {
                        handle_file_end(&state, transfer_id, &mut inbound_transfers).await?;
                    }
                    WireMessage::InputFrame {
                        machine_id,
                        sequence,
                        timestamp_unix_ms,
                        events,
                    } => {
                        handle_input_frame_message(
                            &state,
                            &authenticated_peer_id,
                            remote_peer_id.as_deref(),
                            machine_id,
                            sequence,
                            timestamp_unix_ms,
                            events,
                        )
                        .await;
                    }
                    WireMessage::Error { message } => {
                        warn!(%message, "remote error frame");
                    }
                }
            }
        }
            }

            Ok(())
        }
        .await
    };

    for transfer in inbound_transfers.into_values() {
        discard_inbound_transfer(transfer).await;
    }
    for transfer in inbound_clipboard_image_transfers.into_values() {
        discard_inbound_clipboard_image_transfer(transfer).await;
    }

    if let Some(peer_id) = &remote_peer_id {
        let _ = state.set_peer_connected(peer_id, false).await;
    }

    session_result
}

pub(super) async fn reconnect_requested_for_peer(
    state: &AppState,
    peer_id: &str,
    observed_generation: &mut u64,
) -> bool {
    let current_generation = state.peer_reconnect_generation(peer_id).await;
    reconnect_generation_advanced(observed_generation, current_generation)
}

pub(super) async fn handle_file_transfer_rejected(
    state: &AppState,
    remote_peer_id: Option<&str>,
    transfer_id: String,
    reason: String,
    outbound_transfer_flow: &mut OutboundTransferFlows,
) {
    remove_outbound_transfer_flow(outbound_transfer_flow, &transfer_id);
    if let Some(peer_id) = remote_peer_id {
        state
            .remove_queued_file_transfer(peer_id, &transfer_id)
            .await;
        state.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file_transfer_rejected".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("transfer_id={transfer_id} reason={reason}"),
            size_bytes: 0,
        });
    }
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

async fn send_message<W>(
    writer: &mut W,
    message: &WireMessage,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    encode_frame_to_vec(message, frame_buffer)?;
    writer
        .write_all(frame_buffer.as_slice())
        .await
        .context("write transport frame")?;
    Ok(())
}

async fn read_wire_frame_payload<R>(reader: &mut R, payload: &mut Vec<u8>) -> Result<Option<usize>>
where
    R: AsyncRead + Unpin,
{
    payload.clear();
    let mut length_prefix = [0u8; WIRE_FRAME_LENGTH_PREFIX_BYTES];
    match reader.read_exact(&mut length_prefix).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(anyhow::Error::from(error).context("read transport frame header")),
    }

    let declared_len = u32::from_be_bytes(length_prefix) as usize;
    if declared_len > MAX_WIRE_FRAME_BYTES {
        bail!(
            "wire frame exceeds max payload length: {} > {}",
            declared_len,
            MAX_WIRE_FRAME_BYTES
        );
    }

    payload.clear();
    payload.resize(declared_len, 0);
    reader
        .read_exact(payload)
        .await
        .context("read transport frame payload")?;

    Ok(Some(declared_len))
}

async fn record_transport_frame_rejected(
    state: &AppState,
    peer_id: &str,
    detail: String,
    size_bytes: u64,
) {
    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "incoming".to_string(),
        kind: "transport_frame_rejected".to_string(),
        peer_id: peer_id.to_string(),
        detail,
        size_bytes,
    });
}
