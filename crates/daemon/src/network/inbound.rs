use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use core_clipboard::{ClipboardPayload, ClipboardPolicy, payload_hash_hex};
use core_protocol::WireMessage;
use tokio::io::AsyncWrite;
use tracing::{info, warn};

use crate::state::{AppState, TransportEventRecord};
use peer_transport::{
    FILE_TRANSFER_INITIAL_CHUNK_CREDITS, InboundClipboardImageTransfer, InboundTransfer,
    MAX_INBOUND_TRANSFERS_PER_PEER,
};

use super::codec::flush_transport_writer;
use super::inbound_payload::enqueue_clipboard_image_payload;
use super::outbound::{send_file_chunk_credit, send_message};

const FILE_TRANSFER_CHUNK_CREDIT_LOW_WATERMARK: u32 = 2;

#[expect(
    clippy::too_many_arguments,
    reason = "file transfer start needs frame metadata plus mutable session IO state"
)]
pub(super) async fn handle_file_start<W>(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    machine_id: String,
    transfer_id: String,
    file_name: String,
    total_bytes: u64,
    inbound_transfers: &mut HashMap<String, InboundTransfer>,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if machine_id != authenticated_peer_id {
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            transfer_id = %transfer_id,
            "dropping file start with mismatched machine_id"
        );
        return Ok(());
    }

    if inbound_transfers.len() >= MAX_INBOUND_TRANSFERS_PER_PEER {
        state
            .record_incoming_file_transfer_failed(
                transfer_id.clone(),
                authenticated_peer_id.to_string(),
                file_name,
                total_bytes,
                "too_many_transfers".to_string(),
            )
            .await;
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!(
                "reason=too_many_transfers active={} limit={}",
                inbound_transfers.len(),
                MAX_INBOUND_TRANSFERS_PER_PEER
            ),
            0,
        )
        .await;
        send_file_transfer_rejected(writer, frame_buffer, &transfer_id, "too_many_transfers")
            .await?;
        return Ok(());
    }

    if inbound_transfers.contains_key(&transfer_id) {
        state
            .record_incoming_file_transfer_failed(
                transfer_id.clone(),
                authenticated_peer_id.to_string(),
                file_name,
                total_bytes,
                "duplicate_transfer_id".to_string(),
            )
            .await;
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=duplicate_transfer_id transfer_id={transfer_id}"),
            0,
        )
        .await;
        send_file_transfer_rejected(writer, frame_buffer, &transfer_id, "duplicate_transfer_id")
            .await?;
        return Ok(());
    }

    if !state.file_transfer_auto_accept_trusted_peers().await {
        state
            .record_incoming_file_transfer_failed(
                transfer_id.clone(),
                authenticated_peer_id.to_string(),
                file_name,
                total_bytes,
                "receive_policy_denied".to_string(),
            )
            .await;
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=receive_policy_denied transfer_id={transfer_id}"),
            total_bytes,
        )
        .await;
        send_file_transfer_rejected(writer, frame_buffer, &transfer_id, "receive_policy_denied")
            .await?;
        return Ok(());
    }

    if let Err(error) = core_transfer::validate_transfer_size_with_limit(
        total_bytes,
        state.file_transfer_max_bytes().await,
    ) {
        state
            .record_incoming_file_transfer_failed(
                transfer_id.clone(),
                authenticated_peer_id.to_string(),
                file_name,
                total_bytes,
                format!("invalid_total_size: {error}"),
            )
            .await;
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=invalid_total_size transfer_id={transfer_id} error={error}"),
            total_bytes,
        )
        .await;
        send_file_transfer_rejected(writer, frame_buffer, &transfer_id, "invalid_total_size")
            .await?;
        return Ok(());
    }

    let Some(peer_id) = remote_peer_id else {
        return Ok(());
    };

    let reserved = match state
        .reserve_incoming_file(peer_id, &file_name, total_bytes)
        .await
    {
        Ok(reserved) => reserved,
        Err(error) => {
            state
                .record_incoming_file_transfer_failed(
                    transfer_id.clone(),
                    authenticated_peer_id.to_string(),
                    file_name,
                    total_bytes,
                    format!("temp_reserve_failed: {error}"),
                )
                .await;
            record_transport_transfer_rejected(
                state,
                authenticated_peer_id,
                format!("reason=temp_reserve_failed transfer_id={transfer_id} error={error}"),
                total_bytes,
            )
            .await;
            send_file_transfer_rejected(writer, frame_buffer, &transfer_id, "temp_reserve_failed")
                .await?;
            return Ok(());
        }
    };

    let file_name = reserved.sanitized_name;
    let final_path = reserved.final_path.clone();
    inbound_transfers.insert(
        transfer_id.clone(),
        InboundTransfer {
            peer_id: peer_id.to_string(),
            file_name: file_name.clone(),
            total_bytes,
            bytes_received: 0,
            remaining_chunk_credits: FILE_TRANSFER_INITIAL_CHUNK_CREDITS,
            final_path: reserved.final_path,
            temp_path: reserved.temp_path,
            temp_file: reserved.temp_file,
        },
    );
    state
        .record_incoming_file_transfer_started(
            transfer_id.clone(),
            peer_id.to_string(),
            file_name.clone(),
            total_bytes,
            final_path,
        )
        .await;
    info!(
        peer_id = %peer_id,
        transfer_id = %transfer_id,
        file_name = %file_name,
        total_bytes,
        "started inbound file transfer"
    );
    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "incoming".to_string(),
        kind: "file_transfer_started".to_string(),
        peer_id: peer_id.to_string(),
        detail: format!(
            "transfer_id={transfer_id} file_name={file_name} total_bytes={total_bytes}"
        ),
        size_bytes: total_bytes,
    });
    let initial_credit_result = async {
        send_file_chunk_credit(
            writer,
            &transfer_id,
            FILE_TRANSFER_INITIAL_CHUNK_CREDITS,
            frame_buffer,
        )
        .await?;
        flush_transport_writer(writer, "flush inbound file transfer initial credit").await
    }
    .await;
    if let Err(error) = initial_credit_result {
        state
            .mark_file_transfer_failed(&transfer_id, &format!("initial_credit_failed: {error}"))
            .await;
        if let Some(transfer) = inbound_transfers.remove(&transfer_id) {
            discard_inbound_transfer(transfer).await;
        }
        return Err(error);
    }

    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "clipboard image start needs frame metadata plus transfer tracking state"
)]
pub(super) async fn handle_clipboard_image_start(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    machine_id: String,
    transfer_id: String,
    total_bytes: u64,
    hash_hex: String,
    inbound_clipboard_transfers: &mut HashMap<String, InboundClipboardImageTransfer>,
) -> Result<()> {
    if machine_id != authenticated_peer_id {
        warn!("dropping clipboard image start with mismatched authenticated identity");
        return Ok(());
    }

    if inbound_clipboard_transfers.len() >= MAX_INBOUND_TRANSFERS_PER_PEER {
        record_clipboard_image_rejected(
            state,
            authenticated_peer_id,
            "too_many_transfers",
            ClipboardImageRejectionMetrics::ActiveLimit {
                active_transfers: inbound_clipboard_transfers.len() as u64,
                transfer_limit: MAX_INBOUND_TRANSFERS_PER_PEER as u64,
            },
            0,
        )
        .await;
        return Ok(());
    }

    if inbound_clipboard_transfers.contains_key(&transfer_id) {
        record_clipboard_image_rejected(
            state,
            authenticated_peer_id,
            "duplicate_transfer",
            ClipboardImageRejectionMetrics::None,
            0,
        )
        .await;
        return Ok(());
    }

    let max_image_bytes = ClipboardPolicy::default().max_image_bytes as u64;
    if total_bytes > max_image_bytes {
        record_clipboard_image_rejected(
            state,
            authenticated_peer_id,
            "payload_too_large",
            ClipboardImageRejectionMetrics::AnnouncedLimit {
                announced_bytes: total_bytes,
                configured_limit_bytes: max_image_bytes,
            },
            total_bytes,
        )
        .await;
        return Ok(());
    }

    let Ok(capacity) = usize::try_from(total_bytes) else {
        record_clipboard_image_rejected(
            state,
            authenticated_peer_id,
            "size_overflow",
            ClipboardImageRejectionMetrics::Announced {
                announced_bytes: total_bytes,
            },
            total_bytes,
        )
        .await;
        return Ok(());
    };

    if let Some(peer_id) = remote_peer_id {
        inbound_clipboard_transfers.insert(
            transfer_id.clone(),
            InboundClipboardImageTransfer {
                peer_id: peer_id.to_string(),
                total_bytes,
                bytes_received: 0,
                hash_hex,
                data: Vec::with_capacity(capacity),
            },
        );
        info!(peer_id = %peer_id, total_bytes, "started inbound clipboard image transfer");
    }

    Ok(())
}

pub(super) async fn handle_file_chunk<W>(
    state: &AppState,
    transfer_id: String,
    data: Vec<u8>,
    inbound_transfers: &mut HashMap<String, InboundTransfer>,
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(mut transfer) = inbound_transfers.remove(&transfer_id) else {
        warn!(transfer_id = %transfer_id, "received file chunk for unknown transfer");
        return Ok(());
    };

    let chunk = data;

    let next_size = transfer.bytes_received.saturating_add(chunk.len() as u64);
    if let Err(error) = core_transfer::validate_transfer_size_with_limit(
        next_size,
        state.file_transfer_max_bytes().await,
    ) {
        state
            .mark_file_transfer_failed(&transfer_id, &format!("chunk_size_invalid: {error}"))
            .await;
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!("reason=chunk_size_invalid transfer_id={transfer_id} error={error}"),
            next_size,
        )
        .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    if next_size > transfer.total_bytes {
        warn!(
            transfer_id = %transfer_id,
            announced_total = transfer.total_bytes,
            attempted_total = next_size,
            "inbound file exceeded announced total bytes"
        );
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!(
                "reason=chunk_exceeds_total transfer_id={transfer_id} announced_total={} attempted_total={next_size}",
                transfer.total_bytes
            ),
            next_size,
        )
        .await;
        state
            .mark_file_transfer_failed(&transfer_id, "chunk_exceeds_total")
            .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    if let Err(error) = tokio::io::AsyncWriteExt::write_all(&mut transfer.temp_file, &chunk).await {
        state
            .mark_file_transfer_failed(&transfer_id, &format!("temp_write_failed: {error}"))
            .await;
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!("reason=temp_write_failed transfer_id={transfer_id} error={error}"),
            next_size,
        )
        .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    transfer.bytes_received = next_size;
    transfer.remaining_chunk_credits = transfer.remaining_chunk_credits.saturating_sub(1);
    let replenish_credits = if transfer.remaining_chunk_credits
        <= FILE_TRANSFER_CHUNK_CREDIT_LOW_WATERMARK
    {
        let credits = FILE_TRANSFER_INITIAL_CHUNK_CREDITS - transfer.remaining_chunk_credits;
        transfer.remaining_chunk_credits = transfer.remaining_chunk_credits.saturating_add(credits);
        credits
    } else {
        0
    };
    let peer_id = transfer.peer_id.clone();
    let total_bytes = transfer.total_bytes;
    inbound_transfers.insert(transfer_id.clone(), transfer);
    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "incoming".to_string(),
        kind: "file_transfer_progress".to_string(),
        peer_id,
        detail: format!(
            "transfer_id={transfer_id} bytes_received={next_size} total_bytes={total_bytes}"
        ),
        size_bytes: next_size,
    });
    state
        .mark_file_transfer_progress(&transfer_id, next_size)
        .await;
    if replenish_credits > 0 {
        let credit_result = async {
            send_file_chunk_credit(writer, &transfer_id, replenish_credits, frame_buffer).await?;
            flush_transport_writer(writer, "flush inbound file transfer chunk credit").await
        }
        .await;
        if let Err(error) = credit_result {
            if let Some(transfer) = inbound_transfers.remove(&transfer_id) {
                discard_inbound_transfer(transfer).await;
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(super) async fn handle_clipboard_image_chunk(
    state: &AppState,
    transfer_id: String,
    data: Vec<u8>,
    inbound_clipboard_transfers: &mut HashMap<String, InboundClipboardImageTransfer>,
) -> Result<()> {
    let Some(mut transfer) = inbound_clipboard_transfers.remove(&transfer_id) else {
        warn!("received clipboard image chunk for unknown transfer");
        return Ok(());
    };

    let next_size = transfer.bytes_received.saturating_add(data.len() as u64);
    if next_size > transfer.total_bytes {
        record_clipboard_image_rejected(
            state,
            &transfer.peer_id,
            "chunk_exceeds_total",
            ClipboardImageRejectionMetrics::AnnouncedAttempted {
                announced_bytes: transfer.total_bytes,
                attempted_bytes: next_size,
            },
            next_size,
        )
        .await;
        discard_inbound_clipboard_image_transfer(transfer).await;
        return Ok(());
    }

    let max_image_bytes = ClipboardPolicy::default().max_image_bytes as u64;
    if next_size > max_image_bytes {
        record_clipboard_image_rejected(
            state,
            &transfer.peer_id,
            "payload_too_large",
            ClipboardImageRejectionMetrics::AttemptedLimit {
                attempted_bytes: next_size,
                configured_limit_bytes: max_image_bytes,
            },
            next_size,
        )
        .await;
        discard_inbound_clipboard_image_transfer(transfer).await;
        return Ok(());
    }

    transfer.data.extend_from_slice(&data);
    transfer.bytes_received = next_size;
    inbound_clipboard_transfers.insert(transfer_id, transfer);
    Ok(())
}

pub(super) async fn handle_file_end(
    state: &AppState,
    transfer_id: String,
    inbound_transfers: &mut HashMap<String, InboundTransfer>,
) -> Result<()> {
    let Some(mut transfer) = inbound_transfers.remove(&transfer_id) else {
        warn!(transfer_id = %transfer_id, "received file end for unknown transfer");
        return Ok(());
    };

    if transfer.bytes_received != transfer.total_bytes {
        warn!(
            transfer_id = %transfer_id,
            expected = transfer.total_bytes,
            actual = transfer.bytes_received,
            "inbound file transfer ended with size mismatch"
        );
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!(
                "reason=size_mismatch transfer_id={transfer_id} expected={} actual={}",
                transfer.total_bytes, transfer.bytes_received
            ),
            transfer.bytes_received,
        )
        .await;
        state
            .mark_file_transfer_failed(&transfer_id, "size_mismatch")
            .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    if let Err(error) = tokio::io::AsyncWriteExt::flush(&mut transfer.temp_file).await {
        state
            .mark_file_transfer_failed(&transfer_id, &format!("temp_flush_failed: {error}"))
            .await;
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!("reason=temp_flush_failed transfer_id={transfer_id} error={error}"),
            transfer.bytes_received,
        )
        .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }
    if let Err(error) = transfer.temp_file.sync_all().await {
        state
            .mark_file_transfer_failed(&transfer_id, &format!("temp_sync_failed: {error}"))
            .await;
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!("reason=temp_sync_failed transfer_id={transfer_id} error={error}"),
            transfer.bytes_received,
        )
        .await;
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }
    drop(transfer.temp_file);

    match state
        .complete_incoming_file(
            &transfer.peer_id,
            transfer.file_name.clone(),
            &transfer.temp_path,
            &transfer.final_path,
            transfer.bytes_received,
        )
        .await
    {
        Ok(path) => {
            state
                .mark_file_transfer_completed(&transfer_id, Some(path.clone()))
                .await;
            info!(
                peer_id = %transfer.peer_id,
                transfer_id = %transfer_id,
                file_name = %transfer.file_name,
                path = %path.display(),
                "stored inbound file payload"
            );
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "incoming".to_string(),
                kind: "file_transfer_completed".to_string(),
                peer_id: transfer.peer_id.clone(),
                detail: format!("transfer_id={transfer_id} file_name={}", transfer.file_name),
                size_bytes: transfer.bytes_received,
            });
        }
        Err(error) => {
            state
                .mark_file_transfer_failed(&transfer_id, &format!("finalize_failed: {error}"))
                .await;
            warn!(
                peer_id = %transfer.peer_id,
                transfer_id = %transfer_id,
                error = ?error,
                "failed to store inbound file payload"
            );
            let _ = tokio::fs::remove_file(&transfer.temp_path).await;
        }
    }

    Ok(())
}

pub(super) async fn handle_clipboard_image_end(
    state: &AppState,
    transfer_id: String,
    inbound_clipboard_transfers: &mut HashMap<String, InboundClipboardImageTransfer>,
) -> Result<()> {
    let Some(transfer) = inbound_clipboard_transfers.remove(&transfer_id) else {
        warn!("received clipboard image end for unknown transfer");
        return Ok(());
    };

    if transfer.bytes_received != transfer.total_bytes {
        record_clipboard_image_rejected(
            state,
            &transfer.peer_id,
            "size_mismatch",
            ClipboardImageRejectionMetrics::ExpectedReceived {
                expected_bytes: transfer.total_bytes,
                received_bytes: transfer.bytes_received,
            },
            transfer.bytes_received,
        )
        .await;
        discard_inbound_clipboard_image_transfer(transfer).await;
        return Ok(());
    }

    let payload = ClipboardPayload::Image(transfer.data);
    let actual_hash = payload_hash_hex(&payload);
    if actual_hash != transfer.hash_hex {
        record_clipboard_image_rejected(
            state,
            &transfer.peer_id,
            "hash_mismatch",
            ClipboardImageRejectionMetrics::None,
            transfer.bytes_received,
        )
        .await;
        return Ok(());
    }

    let peer_id = transfer.peer_id;
    let total_bytes = transfer.total_bytes;
    let ClipboardPayload::Image(image_bmp) = payload else {
        unreachable!("clipboard image transfer payload must be an image")
    };
    enqueue_clipboard_image_payload(
        state,
        &peer_id,
        image_bmp,
        "received chunked clipboard image payload",
    )
    .await;
    info!(peer_id = %peer_id, total_bytes, "completed inbound clipboard image transfer");
    Ok(())
}

pub(super) async fn discard_inbound_transfer(transfer: InboundTransfer) {
    let InboundTransfer {
        temp_path,
        temp_file,
        ..
    } = transfer;
    drop(temp_file);
    let _ = tokio::fs::remove_file(temp_path).await;
}

async fn send_file_transfer_rejected<W>(
    writer: &mut W,
    frame_buffer: &mut Vec<u8>,
    transfer_id: &str,
    reason: &str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    send_message(
        writer,
        &WireMessage::FileTransferRejected {
            transfer_id: transfer_id.to_string(),
            reason: reason.to_string(),
        },
        frame_buffer,
    )
    .await?;
    flush_transport_writer(writer, "flush inbound file transfer rejection").await
}

pub(super) async fn discard_inbound_clipboard_image_transfer(
    _transfer: InboundClipboardImageTransfer,
) {
}

async fn record_transport_transfer_rejected(
    state: &AppState,
    peer_id: &str,
    detail: String,
    size_bytes: u64,
) {
    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "incoming".to_string(),
        kind: "transport_transfer_rejected".to_string(),
        peer_id: peer_id.to_string(),
        detail,
        size_bytes,
    });
}

#[derive(Debug, Clone, Copy)]
enum ClipboardImageRejectionMetrics {
    None,
    ActiveLimit {
        active_transfers: u64,
        transfer_limit: u64,
    },
    AnnouncedLimit {
        announced_bytes: u64,
        configured_limit_bytes: u64,
    },
    Announced {
        announced_bytes: u64,
    },
    AnnouncedAttempted {
        announced_bytes: u64,
        attempted_bytes: u64,
    },
    AttemptedLimit {
        attempted_bytes: u64,
        configured_limit_bytes: u64,
    },
    ExpectedReceived {
        expected_bytes: u64,
        received_bytes: u64,
    },
}

impl ClipboardImageRejectionMetrics {
    fn detail(self, reason: &str) -> String {
        let metadata = match self {
            Self::None => String::new(),
            Self::ActiveLimit {
                active_transfers,
                transfer_limit,
            } => format!(" active_transfers={active_transfers} transfer_limit={transfer_limit}"),
            Self::AnnouncedLimit {
                announced_bytes,
                configured_limit_bytes,
            } => format!(
                " announced_bytes={announced_bytes} configured_limit_bytes={configured_limit_bytes}"
            ),
            Self::Announced { announced_bytes } => {
                format!(" announced_bytes={announced_bytes}")
            }
            Self::AnnouncedAttempted {
                announced_bytes,
                attempted_bytes,
            } => format!(" announced_bytes={announced_bytes} attempted_bytes={attempted_bytes}"),
            Self::AttemptedLimit {
                attempted_bytes,
                configured_limit_bytes,
            } => format!(
                " attempted_bytes={attempted_bytes} configured_limit_bytes={configured_limit_bytes}"
            ),
            Self::ExpectedReceived {
                expected_bytes,
                received_bytes,
            } => format!(" expected_bytes={expected_bytes} received_bytes={received_bytes}"),
        };
        format!("payload_type=bmp disposition=rejected reason={reason}{metadata}")
    }
}

async fn record_clipboard_image_rejected(
    state: &AppState,
    peer_id: &str,
    reason: &str,
    metrics: ClipboardImageRejectionMetrics,
    size_bytes: u64,
) {
    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "incoming".to_string(),
        kind: "clipboard_image_rejected".to_string(),
        peer_id: peer_id.to_string(),
        detail: metrics.detail(reason),
        size_bytes,
    });
}
