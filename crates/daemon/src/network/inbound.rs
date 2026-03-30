use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::Utc;
use core_clipboard::{ClipboardPayload, ClipboardPolicy, payload_hash_hex};
use tokio::io::AsyncWrite;
use tracing::{info, warn};

use crate::state::{AppState, TransportEventRecord};
use peer_transport::FILE_TRANSFER_INITIAL_CHUNK_CREDITS;

use super::codec::now_millis;
use super::inbound_payload::enqueue_clipboard_image_payload;
use super::outbound::send_file_chunk_credit;
use super::{MAX_INBOUND_TRANSFERS_PER_PEER, validate_transfer_size};

pub(super) struct InboundTransfer {
    pub(super) peer_id: String,
    pub(super) file_name: String,
    pub(super) total_bytes: u64,
    pub(super) bytes_received: u64,
    pub(super) temp_path: std::path::PathBuf,
    pub(super) temp_file: tokio::fs::File,
}

pub(super) struct InboundClipboardImageTransfer {
    pub(super) peer_id: String,
    pub(super) total_bytes: u64,
    pub(super) bytes_received: u64,
    pub(super) hash_hex: String,
    pub(super) data: Vec<u8>,
}

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
        return Ok(());
    }

    if inbound_transfers.contains_key(&transfer_id) {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=duplicate_transfer_id transfer_id={transfer_id}"),
            0,
        )
        .await;
        return Ok(());
    }

    if let Err(error) = validate_transfer_size(total_bytes) {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=invalid_total_size transfer_id={transfer_id} error={error}"),
            total_bytes,
        )
        .await;
        return Ok(());
    }

    let temp_path = std::env::temp_dir().join(format!(
        "boundless-inbound-{}-{}-{}.part",
        authenticated_peer_id,
        now_millis(),
        transfer_id
    ));
    let temp_file = match tokio::fs::File::create(&temp_path).await {
        Ok(file) => file,
        Err(error) => {
            record_transport_transfer_rejected(
                state,
                authenticated_peer_id,
                format!("reason=temp_create_failed transfer_id={transfer_id} error={error}"),
                total_bytes,
            )
            .await;
            return Ok(());
        }
    };

    if let Some(peer_id) = remote_peer_id {
        inbound_transfers.insert(
            transfer_id.clone(),
            InboundTransfer {
                peer_id: peer_id.to_string(),
                file_name: file_name.clone(),
                total_bytes,
                bytes_received: 0,
                temp_path,
                temp_file,
            },
        );
        info!(
            peer_id = %peer_id,
            transfer_id = %transfer_id,
            file_name = %file_name,
            total_bytes,
            "started inbound file transfer"
        );
        send_file_chunk_credit(
            writer,
            &transfer_id,
            FILE_TRANSFER_INITIAL_CHUNK_CREDITS,
            frame_buffer,
        )
        .await?;
        tokio::io::AsyncWriteExt::flush(writer)
            .await
            .context("flush inbound file transfer initial credit")?;
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
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            transfer_id = %transfer_id,
            "dropping clipboard image start with mismatched machine_id"
        );
        return Ok(());
    }

    if inbound_clipboard_transfers.len() >= MAX_INBOUND_TRANSFERS_PER_PEER {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!(
                "reason=too_many_clipboard_image_transfers active={} limit={}",
                inbound_clipboard_transfers.len(),
                MAX_INBOUND_TRANSFERS_PER_PEER
            ),
            0,
        )
        .await;
        return Ok(());
    }

    if inbound_clipboard_transfers.contains_key(&transfer_id) {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=duplicate_transfer_id transfer_id={transfer_id}"),
            0,
        )
        .await;
        return Ok(());
    }

    let max_image_bytes = ClipboardPolicy::default().max_image_bytes as u64;
    if total_bytes > max_image_bytes {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!(
                "reason=clipboard_image_too_large transfer_id={transfer_id} total_bytes={total_bytes} limit={max_image_bytes}"
            ),
            total_bytes,
        )
        .await;
        return Ok(());
    }

    let Ok(capacity) = usize::try_from(total_bytes) else {
        record_transport_transfer_rejected(
            state,
            authenticated_peer_id,
            format!("reason=clipboard_image_size_overflow transfer_id={transfer_id}"),
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
        info!(
            peer_id = %peer_id,
            transfer_id = %transfer_id,
            total_bytes,
            "started inbound clipboard image transfer"
        );
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
    if let Err(error) = validate_transfer_size(next_size) {
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
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    if let Err(error) = tokio::io::AsyncWriteExt::write_all(&mut transfer.temp_file, &chunk).await {
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
    inbound_transfers.insert(transfer_id.clone(), transfer);
    send_file_chunk_credit(writer, &transfer_id, 1, frame_buffer).await?;
    tokio::io::AsyncWriteExt::flush(writer)
        .await
        .context("flush inbound file transfer chunk credit")?;
    Ok(())
}

pub(super) async fn handle_clipboard_image_chunk(
    state: &AppState,
    transfer_id: String,
    data: Vec<u8>,
    inbound_clipboard_transfers: &mut HashMap<String, InboundClipboardImageTransfer>,
) -> Result<()> {
    let Some(mut transfer) = inbound_clipboard_transfers.remove(&transfer_id) else {
        warn!(
            transfer_id = %transfer_id,
            "received clipboard image chunk for unknown transfer"
        );
        return Ok(());
    };

    let next_size = transfer.bytes_received.saturating_add(data.len() as u64);
    if next_size > transfer.total_bytes {
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
        discard_inbound_clipboard_image_transfer(transfer).await;
        return Ok(());
    }

    let max_image_bytes = ClipboardPolicy::default().max_image_bytes as u64;
    if next_size > max_image_bytes {
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!(
                "reason=clipboard_image_too_large transfer_id={transfer_id} size={next_size} limit={max_image_bytes}"
            ),
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
        discard_inbound_transfer(transfer).await;
        return Ok(());
    }

    if let Err(error) = tokio::io::AsyncWriteExt::flush(&mut transfer.temp_file).await {
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
    drop(transfer.temp_file);

    match state
        .store_incoming_file_from_temp(
            &transfer.peer_id,
            &transfer.file_name,
            &transfer.temp_path,
            transfer.bytes_received,
        )
        .await
    {
        Ok(path) => {
            info!(
                peer_id = %transfer.peer_id,
                transfer_id = %transfer_id,
                file_name = %transfer.file_name,
                path = %path.display(),
                "stored inbound file payload"
            );
        }
        Err(error) => {
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
        warn!(
            transfer_id = %transfer_id,
            "received clipboard image end for unknown transfer"
        );
        return Ok(());
    };

    if transfer.bytes_received != transfer.total_bytes {
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
        discard_inbound_clipboard_image_transfer(transfer).await;
        return Ok(());
    }

    let payload = ClipboardPayload::Image(transfer.data);
    let actual_hash = payload_hash_hex(&payload);
    if actual_hash != transfer.hash_hex {
        record_transport_transfer_rejected(
            state,
            &transfer.peer_id,
            format!(
                "reason=hash_mismatch transfer_id={transfer_id} expected={} actual={actual_hash}",
                transfer.hash_hex
            ),
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
    info!(
        peer_id = %peer_id,
        transfer_id = %transfer_id,
        total_bytes,
        "completed inbound clipboard image transfer"
    );
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
