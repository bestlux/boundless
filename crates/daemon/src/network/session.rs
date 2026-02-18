use super::*;

#[derive(Debug)]
struct InboundTransfer {
    peer_id: String,
    file_name: String,
    total_bytes: u64,
    bytes: Vec<u8>,
}

pub(super) async fn connect_and_run_outbound(
    state: AppState,
    peer_id: &str,
    address: &str,
) -> Result<()> {
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

pub(super) async fn handle_incoming_connection(
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

pub(super) async fn reconnect_requested_for_peer(
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

pub(super) async fn flush_outgoing_payloads<W>(
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
