use chrono::Utc;
use tracing::{info, warn};

use crate::state::{AppState, TransportEventRecord};

use super::codec::input_event_from_wire;
use super::*;

pub(super) async fn handle_clipboard_text_message(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    machine_id: String,
    text: String,
) {
    if machine_id != authenticated_peer_id {
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            "dropping clipboard payload with mismatched machine_id"
        );
        return;
    }

    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        record_transport_frame_rejected(
            state,
            authenticated_peer_id,
            format!(
                "reason=clipboard_text_too_large size={} limit={}",
                text.len(),
                MAX_CLIPBOARD_TEXT_BYTES
            ),
            text.len() as u64,
        )
        .await;
        return;
    }

    if let Some(peer_id) = remote_peer_id {
        if let Err(error) = state
            .enqueue_remote_clipboard_text(peer_id, text.clone())
            .await
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

pub(super) async fn handle_clipboard_image_message(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    machine_id: String,
    data: Vec<u8>,
) {
    if machine_id != authenticated_peer_id {
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            "dropping clipboard image payload with mismatched machine_id"
        );
        return;
    }

    if let Some(peer_id) = remote_peer_id {
        enqueue_clipboard_image_payload(state, peer_id, data, "received clipboard image payload")
            .await;
    }
}

pub(super) async fn enqueue_clipboard_image_payload(
    state: &AppState,
    peer_id: &str,
    data: Vec<u8>,
    success_message: &str,
) {
    let size_bytes = data.len();
    if let Err(error) = state.enqueue_remote_clipboard_image(peer_id, data).await {
        warn!(
            peer_id = %peer_id,
            error = ?error,
            "failed to enqueue incoming clipboard image payload"
        );
    } else {
        info!(
            peer_id = %peer_id,
            size_bytes,
            message = success_message,
            "clipboard image payload enqueued"
        );
    }
}

pub(super) async fn handle_input_frame_message(
    state: &AppState,
    authenticated_peer_id: &str,
    remote_peer_id: Option<&str>,
    machine_id: String,
    sequence: u64,
    timestamp_unix_ms: i64,
    events: Vec<WireInputEvent>,
) {
    if machine_id != authenticated_peer_id {
        warn!(
            claimed_machine_id = %machine_id,
            authenticated_machine_id = %authenticated_peer_id,
            "dropping input frame with mismatched machine_id"
        );
        return;
    }

    if let Some(peer_id) = remote_peer_id {
        let frame = InputFrame {
            source_peer_id: peer_id.to_string(),
            sequence,
            timestamp_unix_ms,
            events: events.into_iter().map(input_event_from_wire).collect(),
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
