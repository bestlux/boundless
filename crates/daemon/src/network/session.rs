use std::{cmp::Ordering, collections::HashMap, sync::Arc, time::Instant};

use chrono::Utc;
use peer_transport::{
    CLIPBOARD_IMAGE_INITIAL_CHUNK_CREDITS, DEFAULT_TRANSPORT_TUNING, InboundClipboardImageTransfer,
    InboundTransfer, OutboundTransferFlows, OutboundTransferKind,
    apply_outbound_chunk_credits_for_kind, reconnect_generation_advanced,
    remove_outbound_transfer_flow,
};

use crate::state::{RuntimeWakeSignal, TransportEventRecord};

use super::codec::{flush_transport_writer, now_millis, write_transport_bytes};
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
    handle_layout_matrix_message,
};
use super::outbound::{
    flush_outgoing_bulk_payloads_with_buffer, flush_outgoing_input_payloads_with_buffer,
    send_clipboard_image_chunk_credit,
};
use super::*;

struct AuthenticatedSession {
    session_id: u64,
    peer_id: String,
    remote_peer_id: Option<String>,
    is_outbound: bool,
    local_machine_id: String,
    local_device_name: String,
}

impl AuthenticatedSession {
    async fn new(state: &AppState, session_id: u64, peer_id: String, is_outbound: bool) -> Self {
        let snapshot = state.snapshot().await;
        Self {
            session_id,
            remote_peer_id: Some(peer_id.clone()),
            peer_id,
            is_outbound,
            local_machine_id: snapshot.machine_id,
            local_device_name: snapshot.device_name,
        }
    }

    fn remote_peer_id(&self) -> Option<&str> {
        self.remote_peer_id.as_deref()
    }

    fn local_hello(&self) -> WireMessage {
        WireMessage::Hello {
            machine_id: self.local_machine_id.clone(),
            display_name: self.local_device_name.clone(),
            protocol: PROTOCOL_CURRENT,
            capability_count: core_protocol::default_capabilities().len(),
        }
    }
}

struct ActiveTransportSessionGuard {
    state: AppState,
    peer_id: String,
    session_id: u64,
    armed: bool,
}

impl ActiveTransportSessionGuard {
    fn new(state: AppState, peer_id: String, session_id: u64) -> Self {
        Self {
            state,
            peer_id,
            session_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveTransportSessionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state
                .clear_active_transport_session(&self.peer_id, self.session_id);
        }
    }
}

enum SessionExitReason {
    ReconnectRequested,
    StateDropped,
    PeerClosed,
    InvalidFrame,
    ProtocolRejected,
    Superseded,
}

enum SessionBranchOutcome {
    Continue,
    Exit(SessionExitReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupBulkState {
    AwaitingTurn,
    AwaitingPeerCompletion,
    Ready,
}

struct SessionRuntime {
    observed_reconnect_generation: u64,
    remote_protocol: Option<ProtocolVersion>,
    inbound_transfers: HashMap<String, InboundTransfer>,
    inbound_clipboard_image_transfers: HashMap<String, InboundClipboardImageTransfer>,
    outbound_transfer_flow: OutboundTransferFlows,
    write_frame_buffer: Vec<u8>,
    last_anti_idle_pulse_sent_at: Option<Instant>,
    startup_bulk_state: StartupBulkState,
}

impl SessionRuntime {
    fn new(observed_reconnect_generation: u64, write_frame_buffer: Vec<u8>) -> Self {
        Self {
            observed_reconnect_generation,
            remote_protocol: None,
            inbound_transfers: HashMap::new(),
            inbound_clipboard_image_transfers: HashMap::new(),
            outbound_transfer_flow: HashMap::new(),
            write_frame_buffer,
            last_anti_idle_pulse_sent_at: None,
            startup_bulk_state: StartupBulkState::AwaitingTurn,
        }
    }

    fn startup_bulk_ready(&self) -> bool {
        self.startup_bulk_state == StartupBulkState::Ready
    }

    fn retire_inbound_clipboard_images(
        &mut self,
        state: &AppState,
        peer_id: &str,
        successor: &'static str,
    ) {
        let retired_transfers = self.inbound_clipboard_image_transfers.len();
        if retired_transfers == 0 {
            return;
        }
        self.inbound_clipboard_image_transfers.clear();
        state.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_image_superseded".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("payload_type=bmp disposition=superseded reason={successor}"),
            size_bytes: 0,
        });
    }

    async fn reconnect_requested_exit(
        &mut self,
        state: &AppState,
        peer_id: &str,
    ) -> Option<SessionExitReason> {
        if reconnect_requested_for_peer(state, peer_id, &mut self.observed_reconnect_generation)
            .await
        {
            info!(
                peer_id = %peer_id,
                "ending session due to explicit reconnect request"
            );
            Some(SessionExitReason::ReconnectRequested)
        } else {
            None
        }
    }

    async fn handle_heartbeat_tick<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if let Some(exit_reason) = self.reconnect_requested_exit(state, &session.peer_id).await {
            return Ok(SessionBranchOutcome::Exit(exit_reason));
        }

        let heartbeat = WireMessage::Heartbeat {
            machine_id: session.local_machine_id.clone(),
            timestamp_unix_ms: now_millis(),
        };
        send_message(writer, &heartbeat, &mut self.write_frame_buffer).await?;
        if let Some(pulse) = state.anti_idle_outbound_pulse().await
            && self
                .last_anti_idle_pulse_sent_at
                .is_none_or(|last| last.elapsed() >= pulse.interval)
        {
            send_message(
                writer,
                &WireMessage::AntiIdlePulse {
                    keep_display_on: pulse.keep_display_on,
                },
                &mut self.write_frame_buffer,
            )
            .await?;
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "anti_idle_pulse_sent".to_string(),
                peer_id: session.peer_id.clone(),
                detail: format!("keep_display_on={}", pulse.keep_display_on),
                size_bytes: 0,
            });
            self.last_anti_idle_pulse_sent_at = Some(std::time::Instant::now());
        }
        if let Some(remote_protocol) = self.remote_protocol {
            self.flush_outgoing_input(state, session, remote_protocol, writer)
                .await?;
            if self.startup_bulk_ready() {
                self.flush_outgoing_bulk(state, session, remote_protocol, writer)
                    .await?;
            }
        }
        flush_transport_writer(writer, "flush heartbeat batch").await?;
        Ok(SessionBranchOutcome::Continue)
    }

    async fn handle_outgoing_input_flush_tick<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if let Some(exit_reason) = self.reconnect_requested_exit(state, &session.peer_id).await {
            return Ok(SessionBranchOutcome::Exit(exit_reason));
        }

        if let Some(remote_protocol) = self.remote_protocol {
            self.flush_outgoing_input(state, session, remote_protocol, writer)
                .await?;
        }
        Ok(SessionBranchOutcome::Continue)
    }

    async fn handle_outgoing_bulk_flush_tick<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if let Some(exit_reason) = self.reconnect_requested_exit(state, &session.peer_id).await {
            return Ok(SessionBranchOutcome::Exit(exit_reason));
        }

        if self.startup_bulk_ready()
            && let Some(remote_protocol) = self.remote_protocol
        {
            self.flush_outgoing_bulk(state, session, remote_protocol, writer)
                .await?;
        }
        Ok(SessionBranchOutcome::Continue)
    }

    async fn handle_outgoing_flush_signal<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if let Some(exit_reason) = self.reconnect_requested_exit(state, &session.peer_id).await {
            return Ok(SessionBranchOutcome::Exit(exit_reason));
        }

        if let Some(remote_protocol) = self.remote_protocol {
            self.flush_outgoing_input(state, session, remote_protocol, writer)
                .await?;
        }
        Ok(SessionBranchOutcome::Continue)
    }

    async fn handle_inbound_read_result<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        frame_payload: &[u8],
        read: Result<Option<usize>>,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if let Some(exit_reason) = self.reconnect_requested_exit(state, &session.peer_id).await {
            return Ok(SessionBranchOutcome::Exit(exit_reason));
        }

        let Some(read) = (match read {
            Ok(read) => read,
            Err(error) => {
                record_transport_frame_rejected(
                    state,
                    &session.peer_id,
                    format!("reason=invalid_frame error={error:#}"),
                    frame_payload.len() as u64,
                )
                .await;
                warn!(
                    peer_id = %session.peer_id,
                    error = ?error,
                    "transport frame rejected"
                );
                return Ok(SessionBranchOutcome::Exit(SessionExitReason::InvalidFrame));
            }
        }) else {
            return Ok(SessionBranchOutcome::Exit(SessionExitReason::PeerClosed));
        };

        let message = match decode_frame_payload(frame_payload) {
            Ok(message) => message,
            Err(error) => {
                record_transport_frame_rejected(
                    state,
                    &session.peer_id,
                    format!("reason=decode_failed error={error}"),
                    read as u64,
                )
                .await;
                warn!(
                    peer_id = %session.peer_id,
                    error = ?error,
                    "dropping undecodable wire message"
                );
                return Ok(SessionBranchOutcome::Continue);
            }
        };

        self.dispatch_inbound_message(state, session, message, writer)
            .await
    }

    async fn dispatch_inbound_message<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        message: WireMessage,
        writer: &mut W,
    ) -> Result<SessionBranchOutcome>
    where
        W: AsyncWrite + Unpin,
    {
        if self.remote_protocol != Some(PROTOCOL_CURRENT)
            && !matches!(
                &message,
                WireMessage::Hello { .. } | WireMessage::Error { .. }
            )
        {
            record_transport_frame_rejected(
                state,
                &session.peer_id,
                "reason=protocol_not_negotiated expected=initial_hello".to_string(),
                0,
            )
            .await;
            send_message(
                writer,
                &WireMessage::Error {
                    message: "protocol not negotiated: initial Hello required".to_string(),
                },
                &mut self.write_frame_buffer,
            )
            .await?;
            flush_transport_writer(writer, "flush pre-Hello protocol rejection").await?;
            return Ok(SessionBranchOutcome::Exit(
                SessionExitReason::ProtocolRejected,
            ));
        }

        match message {
            WireMessage::Hello {
                machine_id,
                protocol,
                ..
            } => {
                let handling = {
                    let Some(_egress) = state
                        .acquire_transport_session_egress(&session.peer_id, session.session_id)
                        .await
                    else {
                        return Ok(SessionBranchOutcome::Exit(SessionExitReason::Superseded));
                    };
                    handle_hello_message(
                        state,
                        &session.peer_id,
                        session.remote_peer_id(),
                        session.is_outbound,
                        &session.local_machine_id,
                        machine_id,
                        protocol,
                        &mut self.remote_protocol,
                        &mut self.outbound_transfer_flow,
                        writer,
                        &mut self.write_frame_buffer,
                    )
                    .await?
                };
                if matches!(handling, HelloHandling::TerminateSession) {
                    return Ok(SessionBranchOutcome::Exit(
                        SessionExitReason::ProtocolRejected,
                    ));
                }
            }
            WireMessage::HelloAck { accepted, .. } => {
                {
                    let Some(_egress) = state
                        .acquire_transport_session_egress(&session.peer_id, session.session_id)
                        .await
                    else {
                        return Ok(SessionBranchOutcome::Exit(SessionExitReason::Superseded));
                    };
                    handle_hello_ack_message(
                        state,
                        session.remote_peer_id(),
                        &session.local_machine_id,
                        self.remote_protocol,
                        &mut self.outbound_transfer_flow,
                        accepted,
                        writer,
                        &mut self.write_frame_buffer,
                    )
                    .await?;
                }
                if accepted
                    && session.is_outbound
                    && self.startup_bulk_state == StartupBulkState::AwaitingTurn
                    && let Some(remote_protocol) = self.remote_protocol
                {
                    if !self
                        .complete_startup_bulk_turn(
                            state,
                            session,
                            remote_protocol,
                            writer,
                            "flush outbound startup bulk handoff",
                        )
                        .await?
                    {
                        return Ok(SessionBranchOutcome::Exit(SessionExitReason::Superseded));
                    }
                    self.startup_bulk_state = StartupBulkState::AwaitingPeerCompletion;
                }
            }
            WireMessage::StartupSyncComplete => match self.startup_bulk_state {
                StartupBulkState::AwaitingTurn if !session.is_outbound => {
                    if let Some(remote_protocol) = self.remote_protocol
                        && !self
                            .complete_startup_bulk_turn(
                                state,
                                session,
                                remote_protocol,
                                writer,
                                "flush inbound startup bulk handoff",
                            )
                            .await?
                    {
                        return Ok(SessionBranchOutcome::Exit(SessionExitReason::Superseded));
                    }
                    self.startup_bulk_state = StartupBulkState::Ready;
                }
                StartupBulkState::AwaitingPeerCompletion if session.is_outbound => {
                    self.startup_bulk_state = StartupBulkState::Ready;
                }
                state => {
                    warn!(
                        peer_id = %session.peer_id,
                        is_outbound = session.is_outbound,
                        startup_state = ?state,
                        "ignoring unexpected startup bulk handoff"
                    );
                }
            },
            WireMessage::Heartbeat { .. } => {
                handle_heartbeat_message(state, session.remote_peer_id()).await;
            }
            WireMessage::AntiIdlePulse { keep_display_on } => {
                handle_anti_idle_pulse_message(state, session.remote_peer_id(), keep_display_on)
                    .await;
            }
            WireMessage::ClipboardText { machine_id, text } => {
                if machine_id == session.peer_id {
                    self.retire_inbound_clipboard_images(state, &session.peer_id, "clipboard_text");
                }
                handle_clipboard_text_message(
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
                    machine_id,
                    text,
                )
                .await;
            }
            WireMessage::ClipboardImage { machine_id, data } => {
                if machine_id == session.peer_id {
                    self.retire_inbound_clipboard_images(
                        state,
                        &session.peer_id,
                        "clipboard_image_inline",
                    );
                }
                handle_clipboard_image_message(
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
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
                let credit_transfer_id = transfer_id.clone();
                let accepted = handle_clipboard_image_start(
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
                    machine_id,
                    transfer_id,
                    total_bytes,
                    hash_hex,
                    &mut self.inbound_clipboard_image_transfers,
                )
                .await?;
                if accepted {
                    send_clipboard_image_chunk_credit(
                        writer,
                        &credit_transfer_id,
                        CLIPBOARD_IMAGE_INITIAL_CHUNK_CREDITS,
                        &mut self.write_frame_buffer,
                    )
                    .await?;
                    flush_transport_writer(writer, "flush initial clipboard image chunk credit")
                        .await?;
                }
            }
            WireMessage::ClipboardImageChunk { transfer_id, data } => {
                let credit_transfer_id = transfer_id.clone();
                let previous_bytes_received = self
                    .inbound_clipboard_image_transfers
                    .get(&credit_transfer_id)
                    .map(|transfer| transfer.bytes_received);
                handle_clipboard_image_chunk(
                    state,
                    transfer_id,
                    data,
                    &mut self.inbound_clipboard_image_transfers,
                )
                .await?;
                let should_replenish = self
                    .inbound_clipboard_image_transfers
                    .get(&credit_transfer_id)
                    .is_some_and(|transfer| {
                        previous_bytes_received
                            .is_some_and(|previous| transfer.bytes_received > previous)
                            && transfer.bytes_received < transfer.total_bytes
                    });
                if should_replenish {
                    send_clipboard_image_chunk_credit(
                        writer,
                        &credit_transfer_id,
                        1,
                        &mut self.write_frame_buffer,
                    )
                    .await?;
                    flush_transport_writer(writer, "flush clipboard image chunk credit").await?;
                }
            }
            WireMessage::ClipboardImageEnd { transfer_id, .. } => {
                handle_clipboard_image_end(
                    state,
                    transfer_id,
                    &mut self.inbound_clipboard_image_transfers,
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
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
                    machine_id,
                    transfer_id,
                    file_name,
                    total_bytes,
                    &mut self.inbound_transfers,
                    writer,
                    &mut self.write_frame_buffer,
                )
                .await?;
            }
            WireMessage::FileChunk { transfer_id, data } => {
                handle_file_chunk(
                    state,
                    transfer_id,
                    data,
                    &mut self.inbound_transfers,
                    writer,
                    &mut self.write_frame_buffer,
                )
                .await?;
            }
            WireMessage::FileChunkCredit {
                transfer_id,
                chunk_credits,
            } => {
                self.handle_chunk_credit(
                    state,
                    session,
                    writer,
                    transfer_id,
                    chunk_credits,
                    OutboundTransferKind::File,
                )
                .await?;
            }
            WireMessage::ClipboardImageChunkCredit {
                transfer_id,
                chunk_credits,
            } => {
                self.handle_chunk_credit(
                    state,
                    session,
                    writer,
                    transfer_id,
                    chunk_credits,
                    OutboundTransferKind::ClipboardImage,
                )
                .await?;
            }
            WireMessage::FileTransferRejected {
                transfer_id,
                reason,
            } => {
                handle_file_transfer_rejected(
                    state,
                    session.remote_peer_id(),
                    transfer_id,
                    reason,
                    &mut self.outbound_transfer_flow,
                )
                .await;
            }
            WireMessage::FileEnd { transfer_id } => {
                handle_file_end(state, transfer_id, &mut self.inbound_transfers).await?;
            }
            WireMessage::InputFrame {
                machine_id,
                sequence,
                timestamp_unix_ms,
                events,
            } => {
                handle_input_frame_message(
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
                    machine_id,
                    sequence,
                    timestamp_unix_ms,
                    events,
                )
                .await;
            }
            WireMessage::LayoutMatrix {
                machine_id,
                matrix_spec,
            } => {
                handle_layout_matrix_message(
                    state,
                    &session.peer_id,
                    session.remote_peer_id(),
                    machine_id,
                    matrix_spec,
                )
                .await;
            }
            WireMessage::Error { message } => {
                warn!(%message, "remote error frame");
                return Ok(SessionBranchOutcome::Exit(
                    SessionExitReason::ProtocolRejected,
                ));
            }
        }

        Ok(SessionBranchOutcome::Continue)
    }

    async fn handle_chunk_credit<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        writer: &mut W,
        transfer_id: String,
        chunk_credits: u32,
        expected_kind: OutboundTransferKind,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        if chunk_credits == 0 {
            return Ok(());
        }

        if expected_kind == OutboundTransferKind::ClipboardImage
            && !state
                .has_outgoing_clipboard_image_cursor(&session.peer_id, &transfer_id)
                .await
        {
            remove_outbound_transfer_flow(&mut self.outbound_transfer_flow, &transfer_id);
            return Ok(());
        }

        let Some(_current_credits) = apply_outbound_chunk_credits_for_kind(
            &mut self.outbound_transfer_flow,
            &transfer_id,
            chunk_credits,
            expected_kind,
        ) else {
            warn!(
                transfer_id = %transfer_id,
                chunk_credits,
                "dropping chunk credit for unknown outbound transfer"
            );
            return Ok(());
        };

        if self.startup_bulk_ready()
            && let Some(remote_protocol) = self.remote_protocol
        {
            self.flush_outgoing_bulk(state, session, remote_protocol, writer)
                .await?;
            flush_transport_writer(writer, "flush outbound bulk after receiving chunk credit")
                .await?;
        }

        if expected_kind == OutboundTransferKind::ClipboardImage
            && !state
                .has_outgoing_clipboard_image_cursor(&session.peer_id, &transfer_id)
                .await
        {
            remove_outbound_transfer_flow(&mut self.outbound_transfer_flow, &transfer_id);
        }

        Ok(())
    }

    async fn flush_outgoing_input<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        remote_protocol: ProtocolVersion,
        writer: &mut W,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let Some(_egress) = state
            .acquire_transport_session_egress(&session.peer_id, session.session_id)
            .await
        else {
            return Ok(());
        };
        flush_outgoing_input_payloads_with_buffer(
            state,
            &session.local_machine_id,
            session.remote_peer_id(),
            remote_protocol,
            &mut self.outbound_transfer_flow,
            writer,
            &mut self.write_frame_buffer,
        )
        .await
    }

    async fn flush_outgoing_bulk<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        remote_protocol: ProtocolVersion,
        writer: &mut W,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let Some(_egress) = state
            .acquire_transport_session_egress(&session.peer_id, session.session_id)
            .await
        else {
            return Ok(());
        };
        flush_outgoing_bulk_payloads_with_buffer(
            state,
            &session.local_machine_id,
            session.remote_peer_id(),
            remote_protocol,
            DEFAULT_TRANSPORT_TUNING.outgoing_bulk_max_payloads_per_flush,
            &mut self.outbound_transfer_flow,
            writer,
            &mut self.write_frame_buffer,
        )
        .await
    }

    async fn complete_startup_bulk_turn<W>(
        &mut self,
        state: &AppState,
        session: &AuthenticatedSession,
        remote_protocol: ProtocolVersion,
        writer: &mut W,
        flush_operation: &'static str,
    ) -> Result<bool>
    where
        W: AsyncWrite + Unpin,
    {
        let Some(_egress) = state
            .acquire_transport_session_egress(&session.peer_id, session.session_id)
            .await
        else {
            return Ok(false);
        };
        flush_outgoing_bulk_payloads_with_buffer(
            state,
            &session.local_machine_id,
            session.remote_peer_id(),
            remote_protocol,
            usize::MAX,
            &mut self.outbound_transfer_flow,
            writer,
            &mut self.write_frame_buffer,
        )
        .await?;
        send_message(
            writer,
            &WireMessage::StartupSyncComplete,
            &mut self.write_frame_buffer,
        )
        .await?;
        flush_transport_writer(writer, flush_operation).await?;
        Ok(true)
    }

    async fn discard_inbound_state(self, state: &AppState) {
        for (transfer_id, transfer) in self.inbound_transfers {
            state
                .mark_file_transfer_failed(&transfer_id, "session_closed")
                .await;
            discard_inbound_transfer(transfer).await;
        }
        for transfer in self.inbound_clipboard_image_transfers.into_values() {
            discard_inbound_clipboard_image_transfer(transfer).await;
        }
    }
}

pub(super) async fn connect_outbound_authenticated(
    state: AppState,
    peer_id: &str,
    address: &str,
) -> Result<tokio_rustls::TlsStream<TcpStream>> {
    let socket = tcp_connect_with_timeout(address).await?;
    configure_low_latency_socket(&socket).context("configure outbound low-latency socket")?;

    let connector = build_tls_connector(&state).await?;
    let server_name = parse_server_name_for_peer(peer_id, address)?;
    let stream = connector
        .connect(server_name, socket)
        .await
        .with_context(|| format!("tls connect {address}"))?;
    let stream = tokio_rustls::TlsStream::Client(stream);
    let authenticated_peer_id = authenticated_peer_machine_id(&state, &stream).await?;
    if peer_id != authenticated_peer_id {
        bail!(
            "peer identity mismatch: expected {} from topology, authenticated {} from TLS",
            peer_id,
            authenticated_peer_id
        );
    }

    Ok(stream)
}

pub(super) async fn run_authenticated_outbound_session(
    state: AppState,
    peer_id: String,
    stream: tokio_rustls::TlsStream<TcpStream>,
    session_registration_id: Option<u64>,
) -> Result<()> {
    run_authenticated_session(state, peer_id, stream, true, session_registration_id).await
}

async fn tcp_connect_with_timeout(address: &str) -> Result<TcpStream> {
    let timeout = outbound_tcp_connect_timeout();
    time::timeout(timeout, tcp_connect_future(address))
        .await
        .with_context(|| format!("tcp connect timed out after {}ms", timeout.as_millis()))?
        .with_context(|| format!("tcp connect {address}"))
}

fn outbound_tcp_connect_timeout() -> std::time::Duration {
    #[cfg(test)]
    {
        let override_ms =
            TEST_OUTBOUND_TCP_CONNECT_TIMEOUT_MS.load(std::sync::atomic::Ordering::SeqCst);
        if override_ms > 0 {
            return std::time::Duration::from_millis(override_ms);
        }
    }

    OUTBOUND_TCP_CONNECT_TIMEOUT
}

fn tcp_connect_future(
    address: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<TcpStream>> + Send>> {
    #[cfg(test)]
    {
        if let Some(hook) = TEST_TCP_CONNECT_HOOK
            .get_or_init(Default::default)
            .lock()
            .expect("test tcp connect hook mutex poisoned")
            .clone()
        {
            return hook(address.to_string());
        }
    }

    let address = address.to_string();
    Box::pin(async move { TcpStream::connect(address).await })
}

#[cfg(test)]
type TestTcpConnectFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<TcpStream>> + Send>>;

#[cfg(test)]
type TestTcpConnectHook =
    std::sync::Arc<dyn Fn(String) -> TestTcpConnectFuture + Send + Sync + 'static>;

#[cfg(test)]
static TEST_TCP_CONNECT_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<TestTcpConnectHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
static TEST_OUTBOUND_TCP_CONNECT_TIMEOUT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) struct TestTcpConnectHookGuard;

#[cfg(test)]
impl Drop for TestTcpConnectHookGuard {
    fn drop(&mut self) {
        *TEST_TCP_CONNECT_HOOK
            .get_or_init(Default::default)
            .lock()
            .expect("test tcp connect hook mutex poisoned") = None;
        TEST_OUTBOUND_TCP_CONNECT_TIMEOUT_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) fn install_test_tcp_connect_hook(
    timeout: std::time::Duration,
    hook: impl Fn(String) -> TestTcpConnectFuture + Send + Sync + 'static,
) -> TestTcpConnectHookGuard {
    let timeout_ms = timeout.as_millis().try_into().unwrap_or(u64::MAX).max(1);
    TEST_OUTBOUND_TCP_CONNECT_TIMEOUT_MS.store(timeout_ms, std::sync::atomic::Ordering::SeqCst);
    *TEST_TCP_CONNECT_HOOK
        .get_or_init(Default::default)
        .lock()
        .expect("test tcp connect hook mutex poisoned") = Some(std::sync::Arc::new(hook));
    TestTcpConnectHookGuard
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
    if let Some(expected_peer_id) = peer_hint.as_deref()
        && expected_peer_id != authenticated_peer_id
    {
        bail!(
            "peer identity mismatch: expected {} from topology, authenticated {} from TLS",
            expected_peer_id,
            authenticated_peer_id
        );
    }

    run_authenticated_session(
        state,
        authenticated_peer_id,
        stream,
        is_outbound,
        session_registration_id,
    )
    .await
}

pub(super) async fn run_authenticated_session<S>(
    state: AppState,
    authenticated_peer_id: String,
    stream: S,
    is_outbound: bool,
    session_registration_id: Option<u64>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Some(session_id) = session_registration_id.filter(|session_id| *session_id != 0) {
        state.bind_pending_transport_session_to_peer(session_id, &authenticated_peer_id);
    }
    let ownership_session_id = session_registration_id
        .filter(|session_id| *session_id != 0)
        .unwrap_or_else(|| state.allocate_transport_session_id());
    let session = AuthenticatedSession::new(
        &state,
        ownership_session_id,
        authenticated_peer_id,
        is_outbound,
    )
    .await;
    let preferred = transport_session_direction_is_preferred(
        &session.local_machine_id,
        &session.peer_id,
        is_outbound,
    );
    let session_cancellation = Arc::new(RuntimeWakeSignal::default());
    match state
        .claim_transport_session(
            &session.peer_id,
            ownership_session_id,
            preferred,
            session_cancellation.clone(),
        )
        .await
    {
        crate::state::TransportSessionClaim::Claimed => {
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: transport_session_direction(is_outbound).to_string(),
                kind: "transport_session_authenticated".to_string(),
                peer_id: session.peer_id.clone(),
                detail: format!(
                    "transport={} ownership=claimed preferred={preferred}",
                    transport_initiation_label(is_outbound),
                ),
                size_bytes: 0,
            });
        }
        crate::state::TransportSessionClaim::Replaced { active_session_id } => {
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: transport_session_direction(is_outbound).to_string(),
                kind: "transport_session_replaced".to_string(),
                peer_id: session.peer_id.clone(),
                detail: format!(
                    "transport={} ownership=replaced_nonpreferred active_session_id={active_session_id}",
                    transport_initiation_label(is_outbound),
                ),
                size_bytes: 0,
            });
        }
        crate::state::TransportSessionClaim::Duplicate { .. } => {
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: transport_session_direction(is_outbound).to_string(),
                kind: "transport_session_duplicate".to_string(),
                peer_id: session.peer_id,
                detail: format!(
                    "transport={} ownership=duplicate active_session=present preferred={preferred}",
                    transport_initiation_label(is_outbound),
                ),
                size_bytes: 0,
            });
            return Ok(());
        }
        crate::state::TransportSessionClaim::Closed => {
            bail!("transport session registry closed");
        }
    }
    let mut active_session_guard = ActiveTransportSessionGuard::new(
        state.clone(),
        session.peer_id.clone(),
        ownership_session_id,
    );

    let (reader, writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    let write_frame_buffer = Vec::<u8>::with_capacity(4096);
    let mut heartbeat_interval = time::interval(DEFAULT_TRANSPORT_TUNING.heartbeat_interval);
    let mut outgoing_input_flush_interval =
        time::interval(DEFAULT_TRANSPORT_TUNING.outgoing_input_flush_interval);
    let mut outgoing_bulk_flush_interval =
        time::interval(DEFAULT_TRANSPORT_TUNING.outgoing_bulk_flush_interval);
    let mut outgoing_flush_signal = state.subscribe_outgoing_flush_signal();
    heartbeat_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    outgoing_input_flush_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    outgoing_bulk_flush_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let local_hello = session.local_hello();
    let observed_reconnect_generation = state.peer_reconnect_generation(&session.peer_id).await;
    let mut runtime = SessionRuntime::new(observed_reconnect_generation, write_frame_buffer);
    let mut frame_reader = WireFrameReader::default();

    let session_result: Result<SessionExitReason> = {
        async {
            send_message(
                &mut writer,
                &local_hello,
                &mut runtime.write_frame_buffer,
            )
            .await?;
            flush_transport_writer(&mut writer, "flush local hello").await?;

            loop {
                let superseded = session_cancellation.notified();
                tokio::pin!(superseded);
                if session_cancellation.take_pending() {
                    break Ok(SessionExitReason::Superseded);
                }
        tokio::select! {
            biased;
            _ = &mut superseded => {
                let _ = session_cancellation.take_pending();
                break Ok(SessionExitReason::Superseded);
            }
            _ = heartbeat_interval.tick() => {
                if let SessionBranchOutcome::Exit(exit_reason) = runtime
                    .handle_heartbeat_tick(&state, &session, &mut writer)
                    .await?
                {
                    break Ok(exit_reason);
                }
            }
            _ = outgoing_input_flush_interval.tick(), if runtime.remote_protocol.is_some() => {
                if let SessionBranchOutcome::Exit(exit_reason) = runtime
                    .handle_outgoing_input_flush_tick(&state, &session, &mut writer)
                    .await?
                {
                    break Ok(exit_reason);
                }
            }
            _ = outgoing_bulk_flush_interval.tick(), if runtime.remote_protocol.is_some() && runtime.startup_bulk_ready() => {
                if let SessionBranchOutcome::Exit(exit_reason) = runtime
                    .handle_outgoing_bulk_flush_tick(&state, &session, &mut writer)
                    .await?
                {
                    break Ok(exit_reason);
                }
            }
            changed = outgoing_flush_signal.changed(), if runtime.remote_protocol.is_some() => {
                if changed.is_err() {
                    break Ok(SessionExitReason::StateDropped);
                }

                if let SessionBranchOutcome::Exit(exit_reason) = runtime
                    .handle_outgoing_flush_signal(&state, &session, &mut writer)
                    .await?
                {
                    break Ok(exit_reason);
                }
            }
            read = frame_reader.read_next(&mut reader) => {
                if let SessionBranchOutcome::Exit(exit_reason) = runtime
                    .handle_inbound_read_result(
                        &state,
                        &session,
                        frame_reader.payload(),
                        read,
                        &mut writer,
                    )
                    .await?
                {
                    break Ok(exit_reason);
                }
            }
        }
            }
        }
        .await
    };

    runtime.discard_inbound_state(&state).await;
    let _was_current = state
        .close_active_transport_session(&session.peer_id, session.session_id)
        .await;
    active_session_guard.disarm();

    session_result.map(|_| ())
}

fn transport_session_direction_is_preferred(
    local_machine_id: &str,
    remote_machine_id: &str,
    is_outbound: bool,
) -> bool {
    match local_machine_id.cmp(remote_machine_id) {
        Ordering::Less => is_outbound,
        Ordering::Greater => !is_outbound,
        Ordering::Equal => is_outbound,
    }
}

fn transport_session_direction(is_outbound: bool) -> &'static str {
    if is_outbound { "outbound" } else { "incoming" }
}

fn transport_initiation_label(is_outbound: bool) -> &'static str {
    if is_outbound {
        "direct_initiated"
    } else {
        "reverse_initiated"
    }
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
            .reject_outbound_file_transfer(peer_id, &transfer_id, &reason)
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
    write_transport_bytes(writer, frame_buffer.as_slice(), "write transport frame").await
}

/// Cancellation-safe framed reader for the session `select!` loop.
///
/// Timer and queue branches may cancel `read_next` after any socket read. The
/// offsets therefore live on the session instead of inside one future; the
/// next call resumes the same header or payload rather than treating the
/// remaining bytes as a new frame.
#[derive(Default)]
pub(super) struct WireFrameReader {
    length_prefix: [u8; WIRE_FRAME_LENGTH_PREFIX_BYTES],
    length_prefix_read: usize,
    declared_len: Option<usize>,
    payload: Vec<u8>,
    payload_read: usize,
}

impl WireFrameReader {
    pub(super) fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn finish_frame(&mut self) {
        self.length_prefix_read = 0;
        self.declared_len = None;
        self.payload_read = 0;
    }

    pub(super) async fn read_next<R>(&mut self, reader: &mut R) -> Result<Option<usize>>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            if self.declared_len.is_none() {
                let read = reader
                    .read(&mut self.length_prefix[self.length_prefix_read..])
                    .await
                    .context("read transport frame header")?;
                if read == 0 {
                    if self.length_prefix_read == 0 {
                        return Ok(None);
                    }
                    bail!("transport closed during frame header");
                }
                self.length_prefix_read += read;
                if self.length_prefix_read < WIRE_FRAME_LENGTH_PREFIX_BYTES {
                    continue;
                }

                let declared_len = u32::from_be_bytes(self.length_prefix) as usize;
                if declared_len > MAX_WIRE_FRAME_BYTES {
                    bail!(
                        "wire frame exceeds max payload length: {} > {}",
                        declared_len,
                        MAX_WIRE_FRAME_BYTES
                    );
                }
                self.payload.clear();
                self.payload.resize(declared_len, 0);
                self.payload_read = 0;
                self.declared_len = Some(declared_len);
                if declared_len == 0 {
                    self.finish_frame();
                    return Ok(Some(0));
                }
            }

            let declared_len = self.declared_len.expect("declared frame length");
            let read = reader
                .read(&mut self.payload[self.payload_read..declared_len])
                .await
                .context("read transport frame payload")?;
            if read == 0 {
                bail!("transport closed during frame payload");
            }
            self.payload_read += read;
            if self.payload_read == declared_len {
                self.finish_frame();
                return Ok(Some(declared_len));
            }
        }
    }
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
