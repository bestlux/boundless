use std::collections::VecDeque;

use crate::state::TransportEventRecord;
use chrono::Utc;
use core_clipboard::{ClipboardPayload, payload_hash_hex};
use peer_transport::{
    CLIPBOARD_IMAGE_CHUNK_BYTES, OutboundTransferFlows, consume_outbound_chunk_credit,
    has_available_outbound_chunk_credit, register_outbound_transfer_flow,
    remove_outbound_transfer_flow, restore_outbound_chunk_credits_for_payloads,
};

use super::codec::input_events_to_wire;
use super::*;

#[derive(Debug)]
enum SendPayloadOutcome {
    Sent,
    SentCursor {
        post_flush: CursorPostFlushAction,
        requeue: Option<OutboundPayload>,
    },
    Dropped,
    DeferredForBackpressure,
}

#[derive(Debug)]
struct CursorPostFlushAction {
    transfer_id: String,
    offset_bytes: u64,
    length_bytes: usize,
    finished: bool,
}

struct OutboundPayloadWriter<'a, W> {
    outbound_transfer_flow: &'a mut OutboundTransferFlows,
    writer: &'a mut W,
    frame_buffer: &'a mut Vec<u8>,
}

#[cfg(test)]
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
    let mut frame_buffer = Vec::with_capacity(4096);
    let mut outbound_transfer_flow = OutboundTransferFlows::new();
    flush_outgoing_input_payloads_with_buffer(
        state,
        local_machine_id,
        remote_peer_id,
        remote_protocol,
        &mut outbound_transfer_flow,
        writer,
        &mut frame_buffer,
    )
    .await?;
    flush_outgoing_bulk_payloads_with_buffer(
        state,
        local_machine_id,
        remote_peer_id,
        remote_protocol,
        usize::MAX,
        &mut outbound_transfer_flow,
        writer,
        &mut frame_buffer,
    )
    .await
}

pub(super) async fn flush_outgoing_input_payloads_with_buffer<W>(
    state: &AppState,
    local_machine_id: &str,
    remote_peer_id: Option<&str>,
    remote_protocol: ProtocolVersion,
    outbound_transfer_flow: &mut OutboundTransferFlows,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(peer_id) = remote_peer_id else {
        return Ok(());
    };
    let pending = state.drain_outgoing_input(peer_id, usize::MAX).await;
    let mut writer_ctx = OutboundPayloadWriter {
        outbound_transfer_flow,
        writer,
        frame_buffer,
    };
    flush_pending_payloads_with_buffer(
        state,
        local_machine_id,
        peer_id,
        remote_protocol,
        pending,
        &mut writer_ctx,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "bulk flush coordinates protocol state, credits, and writer buffers in one step"
)]
pub(super) async fn flush_outgoing_bulk_payloads_with_buffer<W>(
    state: &AppState,
    local_machine_id: &str,
    remote_peer_id: Option<&str>,
    remote_protocol: ProtocolVersion,
    max_payloads: usize,
    outbound_transfer_flow: &mut OutboundTransferFlows,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if max_payloads == 0 {
        return Ok(());
    }
    let Some(peer_id) = remote_peer_id else {
        return Ok(());
    };
    let pending = state.drain_outgoing_bulk(peer_id, max_payloads).await;
    let mut writer_ctx = OutboundPayloadWriter {
        outbound_transfer_flow,
        writer,
        frame_buffer,
    };
    flush_pending_payloads_with_buffer(
        state,
        local_machine_id,
        peer_id,
        remote_protocol,
        pending,
        &mut writer_ctx,
    )
    .await
}

async fn flush_pending_payloads_with_buffer<W>(
    state: &AppState,
    local_machine_id: &str,
    peer_id: &str,
    remote_protocol: ProtocolVersion,
    pending_payloads: Vec<OutboundPayload>,
    writer_ctx: &mut OutboundPayloadWriter<'_, W>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if pending_payloads.is_empty() {
        return Ok(());
    }

    let mut pending = VecDeque::from(pending_payloads);
    let mut sent_for_flush = Vec::<OutboundPayload>::new();
    let mut requeue_after_flush = Vec::<OutboundPayload>::new();
    let mut cursor_post_flush = Vec::<CursorPostFlushAction>::new();
    let mut sent_any = false;
    while let Some(payload) = pending.pop_front() {
        match send_outbound_payload(
            state,
            local_machine_id,
            peer_id,
            remote_protocol,
            &payload,
            writer_ctx,
        )
        .await
        {
            Ok(SendPayloadOutcome::Sent) => {
                sent_any = true;
                sent_for_flush.push(payload);
            }
            Ok(SendPayloadOutcome::SentCursor {
                post_flush,
                requeue,
            }) => {
                sent_any = true;
                sent_for_flush.push(payload);
                cursor_post_flush.push(post_flush);
                if let Some(requeue_payload) = requeue {
                    requeue_after_flush.push(requeue_payload);
                }
            }
            Ok(SendPayloadOutcome::Dropped) => {}
            Ok(SendPayloadOutcome::DeferredForBackpressure) => {
                let mut unsent = Vec::with_capacity(pending.len() + 1);
                unsent.push(payload);
                unsent.extend(pending);
                state.requeue_outgoing_front(peer_id, unsent).await;
                break;
            }
            Err(error) => {
                let mut unsent = Vec::with_capacity(pending.len() + 1);
                unsent.push(payload);
                unsent.extend(pending);
                state.requeue_outgoing_front(peer_id, unsent).await;
                if !sent_for_flush.is_empty() {
                    restore_outbound_chunk_credits_for_payloads(
                        writer_ctx.outbound_transfer_flow,
                        &sent_for_flush,
                    );
                }
                return Err(error);
            }
        }
    }

    if sent_any {
        if let Err(error) = writer_ctx
            .writer
            .flush()
            .await
            .context("flush outbound payload batch")
        {
            if !sent_for_flush.is_empty() {
                restore_outbound_chunk_credits_for_payloads(
                    writer_ctx.outbound_transfer_flow,
                    &sent_for_flush,
                );
                state.requeue_outgoing_front(peer_id, sent_for_flush).await;
            }
            return Err(error);
        }

        for action in cursor_post_flush {
            apply_cursor_post_flush_action(state, peer_id, writer_ctx, action).await;
        }

        if !requeue_after_flush.is_empty() {
            state
                .requeue_outgoing_front(peer_id, requeue_after_flush)
                .await;
        }
    }

    Ok(())
}

async fn apply_cursor_post_flush_action<W>(
    state: &AppState,
    peer_id: &str,
    writer_ctx: &mut OutboundPayloadWriter<'_, W>,
    action: CursorPostFlushAction,
) where
    W: AsyncWrite + Unpin,
{
    if action.length_bytes > 0 {
        if !state
            .commit_outbound_file_chunk(
                peer_id,
                &action.transfer_id,
                action.offset_bytes,
                action.length_bytes,
            )
            .await
        {
            return;
        }
        state.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file_transfer_progress".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "transfer_id={} offset_bytes={} length_bytes={}",
                action.transfer_id, action.offset_bytes, action.length_bytes
            ),
            size_bytes: action.length_bytes as u64,
        });
    }

    if action.finished {
        remove_outbound_transfer_flow(writer_ctx.outbound_transfer_flow, &action.transfer_id);

        if let Some((file_name, total_bytes)) = state
            .complete_outbound_file_transfer(peer_id, &action.transfer_id)
            .await
        {
            state
                .record_outgoing_file(peer_id, &file_name, total_bytes)
                .await;
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "file_transfer_completed".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!("transfer_id={} file_name={file_name}", action.transfer_id),
                size_bytes: total_bytes,
            });
        }
    }
}

async fn send_outbound_payload<W>(
    state: &AppState,
    local_machine_id: &str,
    peer_id: &str,
    remote_protocol: ProtocolVersion,
    payload: &OutboundPayload,
    writer_ctx: &mut OutboundPayloadWriter<'_, W>,
) -> Result<SendPayloadOutcome>
where
    W: AsyncWrite + Unpin,
{
    if remote_protocol != PROTOCOL_CURRENT {
        bail!(
            "unsupported peer protocol for canonical v1: remote={} expected={}",
            remote_protocol,
            PROTOCOL_CURRENT
        );
    }

    match payload {
        OutboundPayload::ClipboardText { text } => {
            let message = WireMessage::ClipboardText {
                machine_id: local_machine_id.to_string(),
                text: text.clone(),
            };
            if let Err(error) = encode_frame_to_vec(&message, writer_ctx.frame_buffer) {
                if matches!(error, WireCodecError::FrameTooLargeToEncode { .. }) {
                    warn!(
                        peer_id = %peer_id,
                        payload_bytes = text.len(),
                        limit_bytes = MAX_WIRE_FRAME_BYTES,
                        "dropping clipboard text payload that exceeds wire frame cap"
                    );
                    return Ok(SendPayloadOutcome::Dropped);
                }
                return Err(anyhow::Error::from(error));
            }
            let payload_bytes = writer_ctx
                .frame_buffer
                .len()
                .saturating_sub(WIRE_FRAME_LENGTH_PREFIX_BYTES);
            if payload_bytes > MAX_WIRE_FRAME_BYTES {
                warn!(
                    peer_id = %peer_id,
                    size_bytes = payload_bytes,
                    limit_bytes = MAX_WIRE_FRAME_BYTES,
                    "dropping clipboard text payload that exceeds wire frame cap"
                );
                return Ok(SendPayloadOutcome::Dropped);
            }
            writer_ctx
                .writer
                .write_all(writer_ctx.frame_buffer.as_slice())
                .await
                .context("write transport frame")?;
            state.record_outgoing_clipboard_text(peer_id, text).await;
            Ok(SendPayloadOutcome::Sent)
        }
        OutboundPayload::ClipboardImage { image_bmp } => {
            let message = WireMessage::ClipboardImage {
                machine_id: local_machine_id.to_string(),
                data: image_bmp.clone(),
            };
            if let Err(error) = encode_frame_to_vec(&message, writer_ctx.frame_buffer) {
                if matches!(error, WireCodecError::FrameTooLargeToEncode { .. }) {
                    send_chunked_clipboard_image(
                        writer_ctx.writer,
                        writer_ctx.frame_buffer,
                        local_machine_id,
                        image_bmp,
                    )
                    .await?;
                    state
                        .record_outgoing_clipboard_image(peer_id, image_bmp.len())
                        .await;
                    return Ok(SendPayloadOutcome::Sent);
                }
                return Err(anyhow::Error::from(error));
            }
            let payload_bytes = writer_ctx
                .frame_buffer
                .len()
                .saturating_sub(WIRE_FRAME_LENGTH_PREFIX_BYTES);
            if payload_bytes > MAX_WIRE_FRAME_BYTES {
                send_chunked_clipboard_image(
                    writer_ctx.writer,
                    writer_ctx.frame_buffer,
                    local_machine_id,
                    image_bmp,
                )
                .await?;
                state
                    .record_outgoing_clipboard_image(peer_id, image_bmp.len())
                    .await;
                return Ok(SendPayloadOutcome::Sent);
            }
            writer_ctx
                .writer
                .write_all(writer_ctx.frame_buffer.as_slice())
                .await
                .context("write transport frame")?;
            state
                .record_outgoing_clipboard_image(peer_id, image_bmp.len())
                .await;
            Ok(SendPayloadOutcome::Sent)
        }
        OutboundPayload::FileStart {
            transfer_id,
            file_name,
            total_bytes,
        } => {
            core_transfer::validate_transfer_size_with_limit(
                *total_bytes,
                state.file_transfer_max_bytes().await,
            )?;
            send_message(
                writer_ctx.writer,
                &WireMessage::FileStart {
                    machine_id: local_machine_id.to_string(),
                    transfer_id: transfer_id.clone(),
                    file_name: file_name.clone(),
                    total_bytes: *total_bytes,
                },
                writer_ctx.frame_buffer,
            )
            .await?;
            register_outbound_transfer_flow(writer_ctx.outbound_transfer_flow, transfer_id.clone());
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "file_transfer_started".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!(
                    "transfer_id={transfer_id} file_name={file_name} total_bytes={total_bytes}"
                ),
                size_bytes: *total_bytes,
            });
            Ok(SendPayloadOutcome::Sent)
        }
        OutboundPayload::FileChunk {
            transfer_id,
            source_path,
            offset_bytes,
            length_bytes,
        } => {
            let Some(has_credit) =
                has_available_outbound_chunk_credit(writer_ctx.outbound_transfer_flow, transfer_id)
            else {
                warn!(
                    peer_id = %peer_id,
                    transfer_id = %transfer_id,
                    "dropping outbound file chunk without active transfer flow state"
                );
                return Ok(SendPayloadOutcome::Dropped);
            };

            if !has_credit {
                return Ok(SendPayloadOutcome::DeferredForBackpressure);
            }

            let mut source_file = tokio::fs::File::open(source_path).await.with_context(|| {
                format!("open outbound file chunk source {}", source_path.display())
            })?;
            source_file
                .seek(std::io::SeekFrom::Start(*offset_bytes))
                .await
                .with_context(|| {
                    format!(
                        "seek outbound file chunk source {} to offset {}",
                        source_path.display(),
                        offset_bytes
                    )
                })?;

            let mut data = vec![0u8; *length_bytes];
            source_file.read_exact(&mut data).await.with_context(|| {
                format!(
                    "read outbound file chunk source {} offset {} length {}",
                    source_path.display(),
                    offset_bytes,
                    length_bytes
                )
            })?;

            send_message(
                writer_ctx.writer,
                &WireMessage::FileChunk {
                    transfer_id: transfer_id.clone(),
                    data,
                },
                writer_ctx.frame_buffer,
            )
            .await?;
            consume_outbound_chunk_credit(writer_ctx.outbound_transfer_flow, transfer_id);
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "file_transfer_progress".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!(
                    "transfer_id={transfer_id} offset_bytes={offset_bytes} length_bytes={length_bytes}"
                ),
                size_bytes: *length_bytes as u64,
            });
            Ok(SendPayloadOutcome::Sent)
        }
        OutboundPayload::FileTransferCursor { transfer_id } => {
            let Some(remaining_bytes) = state
                .outbound_file_transfer_remaining_bytes(peer_id, transfer_id)
                .await
            else {
                warn!(
                    peer_id = %peer_id,
                    transfer_id = %transfer_id,
                    "dropping outbound file cursor without active transfer state"
                );
                remove_outbound_transfer_flow(writer_ctx.outbound_transfer_flow, transfer_id);
                return Ok(SendPayloadOutcome::Dropped);
            };

            if remaining_bytes > 0 {
                let Some(has_credit) = has_available_outbound_chunk_credit(
                    writer_ctx.outbound_transfer_flow,
                    transfer_id,
                ) else {
                    warn!(
                        peer_id = %peer_id,
                        transfer_id = %transfer_id,
                        "dropping outbound file cursor without active transfer flow state"
                    );
                    state
                        .fail_outbound_file_transfer(peer_id, transfer_id, "missing_transfer_flow")
                        .await;
                    return Ok(SendPayloadOutcome::Dropped);
                };

                if !has_credit {
                    return Ok(SendPayloadOutcome::DeferredForBackpressure);
                }
            }

            let chunk = match state
                .materialize_outbound_file_chunk(peer_id, transfer_id)
                .await
            {
                Ok(chunk) => chunk,
                Err(error) => {
                    warn!(
                        peer_id = %peer_id,
                        transfer_id = %transfer_id,
                        error = %error,
                        "failing outbound file transfer cursor"
                    );
                    remove_outbound_transfer_flow(writer_ctx.outbound_transfer_flow, transfer_id);
                    state
                        .fail_outbound_file_transfer(peer_id, transfer_id, &error.to_string())
                        .await;
                    return Ok(SendPayloadOutcome::Dropped);
                }
            };

            let chunk_len = chunk.data.len();
            if !chunk.data.is_empty() {
                send_message(
                    writer_ctx.writer,
                    &WireMessage::FileChunk {
                        transfer_id: chunk.transfer_id.clone(),
                        data: chunk.data,
                    },
                    writer_ctx.frame_buffer,
                )
                .await?;
                consume_outbound_chunk_credit(writer_ctx.outbound_transfer_flow, transfer_id);
            }
            let post_flush = CursorPostFlushAction {
                transfer_id: chunk.transfer_id.clone(),
                offset_bytes: chunk.offset_bytes,
                length_bytes: chunk_len,
                finished: chunk.finished,
            };

            if chunk.finished {
                send_message(
                    writer_ctx.writer,
                    &WireMessage::FileEnd {
                        transfer_id: chunk.transfer_id.clone(),
                    },
                    writer_ctx.frame_buffer,
                )
                .await?;
                Ok(SendPayloadOutcome::SentCursor {
                    post_flush,
                    requeue: None,
                })
            } else {
                Ok(SendPayloadOutcome::SentCursor {
                    post_flush,
                    requeue: Some(OutboundPayload::FileTransferCursor {
                        transfer_id: transfer_id.clone(),
                    }),
                })
            }
        }
        OutboundPayload::FileEnd {
            transfer_id,
            file_name,
            total_bytes,
        } => {
            send_message(
                writer_ctx.writer,
                &WireMessage::FileEnd {
                    transfer_id: transfer_id.clone(),
                },
                writer_ctx.frame_buffer,
            )
            .await?;
            remove_outbound_transfer_flow(writer_ctx.outbound_transfer_flow, transfer_id);

            state
                .record_outgoing_file(peer_id, file_name, *total_bytes)
                .await;
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "file_transfer_completed".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!("transfer_id={transfer_id} file_name={file_name}"),
                size_bytes: *total_bytes,
            });
            Ok(SendPayloadOutcome::Sent)
        }
        OutboundPayload::InputFrame {
            sequence,
            timestamp_unix_ms,
            events,
        } => {
            send_message(
                writer_ctx.writer,
                &WireMessage::InputFrame {
                    machine_id: local_machine_id.to_string(),
                    sequence: *sequence,
                    timestamp_unix_ms: *timestamp_unix_ms,
                    events: input_events_to_wire(events),
                },
                writer_ctx.frame_buffer,
            )
            .await?;

            state
                .record_outgoing_input_frame(peer_id, *sequence, events.len(), *timestamp_unix_ms)
                .await;
            Ok(SendPayloadOutcome::Sent)
        }
    }
}

async fn send_chunked_clipboard_image<W>(
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
    local_machine_id: &str,
    image_bmp: &[u8],
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let hash_hex = payload_hash_hex(&ClipboardPayload::Image(image_bmp.to_vec()));
    send_message(
        writer,
        &WireMessage::ClipboardImageStart {
            machine_id: local_machine_id.to_string(),
            transfer_id: transfer_id.clone(),
            total_bytes: image_bmp.len() as u64,
            hash_hex,
        },
        frame_buffer,
    )
    .await?;

    for chunk in image_bmp.chunks(CLIPBOARD_IMAGE_CHUNK_BYTES) {
        send_message(
            writer,
            &WireMessage::ClipboardImageChunk {
                transfer_id: transfer_id.clone(),
                data: chunk.to_vec(),
            },
            frame_buffer,
        )
        .await?;
    }

    send_message(
        writer,
        &WireMessage::ClipboardImageEnd { transfer_id },
        frame_buffer,
    )
    .await
}

pub(super) async fn send_file_chunk_credit<W>(
    writer: &mut W,
    transfer_id: &str,
    chunk_credits: u32,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if chunk_credits == 0 {
        return Ok(());
    }

    send_message(
        writer,
        &WireMessage::FileChunkCredit {
            transfer_id: transfer_id.to_string(),
            chunk_credits,
        },
        frame_buffer,
    )
    .await
}

pub(super) async fn send_message<W>(
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
