use anyhow::Result;
use tokio::io::AsyncWrite;
use tracing::warn;

use super::codec::{flush_transport_writer, write_transport_bytes};
use super::outbound::flush_outgoing_input_payloads_with_buffer;
use super::*;
use peer_transport::OutboundTransferFlows;

pub(super) enum HelloHandling {
    Continue,
    TerminateSession,
}

#[expect(
    clippy::too_many_arguments,
    reason = "wire control handlers operate on explicit session state and IO buffers"
)]
pub(super) async fn handle_hello_message<W>(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    is_outbound: bool,
    local_machine_id: &str,
    machine_id: String,
    protocol: ProtocolVersion,
    remote_protocol: &mut Option<ProtocolVersion>,
    outbound_transfer_flow: &mut OutboundTransferFlows,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<HelloHandling>
where
    W: AsyncWrite + Unpin,
{
    if machine_id != authenticated_peer_id {
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            "hello machine_id mismatch from authenticated peer"
        );
        let _ = send_message(
            writer,
            &WireMessage::Error {
                message: "hello machine_id mismatch".to_string(),
            },
            frame_buffer,
        )
        .await;
        let _ = flush_transport_writer(writer, "flush hello identity rejection").await;
        return Ok(HelloHandling::TerminateSession);
    }

    if protocol != PROTOCOL_CURRENT {
        warn!(
            peer_id = %authenticated_peer_id,
            remote_protocol = %protocol,
            expected_protocol = %PROTOCOL_CURRENT,
            "rejecting peer with non-canonical protocol version"
        );
        let _ = send_message(
            writer,
            &WireMessage::Error {
                message: format!(
                    "unsupported protocol version: remote={} expected={}",
                    protocol, PROTOCOL_CURRENT
                ),
            },
            frame_buffer,
        )
        .await;
        let _ = flush_transport_writer(writer, "flush hello protocol rejection").await;
        return Ok(HelloHandling::TerminateSession);
    }

    *remote_protocol = Some(protocol);

    if let Some(peer_id) = remote_peer_id {
        let _ = state.set_peer_connected(peer_id, true).await;
    }

    if !is_outbound {
        let ack = WireMessage::HelloAck {
            machine_id: local_machine_id.to_string(),
            accepted: true,
        };
        send_message(writer, &ack, frame_buffer).await?;
    }

    flush_pending_input_after_control_frame(
        state,
        local_machine_id,
        remote_peer_id,
        *remote_protocol,
        outbound_transfer_flow,
        writer,
        frame_buffer,
    )
    .await?;
    flush_transport_writer(writer, "flush hello/ack batch").await?;
    Ok(HelloHandling::Continue)
}

#[expect(
    clippy::too_many_arguments,
    reason = "wire control handlers operate on explicit session state and IO buffers"
)]
pub(super) async fn handle_hello_ack_message<W>(
    state: &AppState,
    remote_peer_id: Option<&str>,
    local_machine_id: &str,
    remote_protocol: Option<ProtocolVersion>,
    outbound_transfer_flow: &mut OutboundTransferFlows,
    accepted: bool,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if accepted && let Some(peer_id) = remote_peer_id {
        let _ = state.set_peer_connected(peer_id, true).await;
    }

    flush_pending_input_after_control_frame(
        state,
        local_machine_id,
        remote_peer_id,
        remote_protocol,
        outbound_transfer_flow,
        writer,
        frame_buffer,
    )
    .await?;
    flush_transport_writer(writer, "flush hello-ack batch").await?;
    Ok(())
}

pub(super) async fn handle_heartbeat_message(state: &AppState, remote_peer_id: Option<&str>) {
    if let Some(peer_id) = remote_peer_id {
        let _ = state.touch_peer(peer_id).await;
    }
}

pub(super) async fn handle_anti_idle_pulse_message(
    state: &AppState,
    remote_peer_id: Option<&str>,
    keep_display_on: bool,
) {
    if let Some(peer_id) = remote_peer_id {
        state
            .note_remote_anti_idle_pulse(peer_id, keep_display_on)
            .await;
    }
}

async fn flush_pending_input_after_control_frame<W>(
    state: &AppState,
    local_machine_id: &str,
    remote_peer_id: Option<&str>,
    remote_protocol: Option<ProtocolVersion>,
    outbound_transfer_flow: &mut OutboundTransferFlows,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if let Some(remote_protocol) = remote_protocol {
        flush_outgoing_input_payloads_with_buffer(
            state,
            local_machine_id,
            remote_peer_id,
            remote_protocol,
            outbound_transfer_flow,
            writer,
            frame_buffer,
        )
        .await?;
    }

    Ok(())
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
