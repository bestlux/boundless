use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio::io::AsyncWrite;
use tracing::warn;

use super::outbound::{
    OutboundTransferFlow, flush_outgoing_bulk_payloads_with_buffer,
    flush_outgoing_input_payloads_with_buffer,
};
use super::*;

pub(super) enum HelloHandling {
    Continue,
    TerminateSession,
}

pub(super) async fn handle_hello_message<W>(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    is_outbound: bool,
    local_machine_id: &str,
    machine_id: String,
    protocol: ProtocolVersion,
    remote_protocol: &mut Option<ProtocolVersion>,
    outbound_transfer_flow: &mut HashMap<String, OutboundTransferFlow>,
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
        let _ = tokio::io::AsyncWriteExt::flush(writer).await;
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
        let _ = tokio::io::AsyncWriteExt::flush(writer).await;
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

    flush_pending_after_control_frame(
        state,
        local_machine_id,
        remote_peer_id,
        *remote_protocol,
        outbound_transfer_flow,
        writer,
        frame_buffer,
    )
    .await?;
    tokio::io::AsyncWriteExt::flush(writer)
        .await
        .context("flush hello/ack batch")?;
    Ok(HelloHandling::Continue)
}

pub(super) async fn handle_hello_ack_message<W>(
    state: &AppState,
    remote_peer_id: Option<&str>,
    local_machine_id: &str,
    remote_protocol: Option<ProtocolVersion>,
    outbound_transfer_flow: &mut HashMap<String, OutboundTransferFlow>,
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

    flush_pending_after_control_frame(
        state,
        local_machine_id,
        remote_peer_id,
        remote_protocol,
        outbound_transfer_flow,
        writer,
        frame_buffer,
    )
    .await?;
    tokio::io::AsyncWriteExt::flush(writer)
        .await
        .context("flush hello-ack batch")?;
    Ok(())
}

pub(super) async fn handle_heartbeat_message(state: &AppState, remote_peer_id: Option<&str>) {
    if let Some(peer_id) = remote_peer_id {
        let _ = state.touch_peer(peer_id).await;
    }
}

async fn flush_pending_after_control_frame<W>(
    state: &AppState,
    local_machine_id: &str,
    remote_peer_id: Option<&str>,
    remote_protocol: Option<ProtocolVersion>,
    outbound_transfer_flow: &mut HashMap<String, OutboundTransferFlow>,
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
        flush_outgoing_bulk_payloads_with_buffer(
            state,
            local_machine_id,
            remote_peer_id,
            remote_protocol,
            OUTGOING_BULK_MAX_PAYLOADS_PER_FLUSH,
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
    writer
        .write_all(frame_buffer.as_slice())
        .await
        .context("write transport frame")?;
    Ok(())
}
