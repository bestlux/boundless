use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::*;

pub(crate) struct ReservedIncomingFile {
    pub(crate) sanitized_name: String,
    pub(crate) final_path: PathBuf,
    pub(crate) temp_path: PathBuf,
    pub(crate) temp_file: tokio::fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalClipboardObservationOutcome {
    Accepted { delivered: bool },
    SuppressedEcho,
    Duplicate,
    Disabled,
}

impl LocalClipboardObservationOutcome {
    fn delivered(self) -> bool {
        matches!(self, Self::Accepted { delivered: true })
    }

    pub(super) fn supersedes_remote(self) -> bool {
        // A same-hash observation with a new broker sequence is an explicit
        // user recopy. The caller gates this with sequence idempotence, so a
        // response-loss retry cannot become a false superseding update.
        matches!(self, Self::Accepted { .. } | Self::Duplicate)
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct OutgoingInputQueueReport {
    enqueued: bool,
    depth: usize,
    dropped: Option<(u64, &'static str)>,
    coalesced: Option<(u64, u64, usize)>,
    high_water: Option<usize>,
}

fn drain_queue_up_to(
    queue: &mut VecDeque<OutboundPayload>,
    max_payloads: usize,
) -> Vec<OutboundPayload> {
    if max_payloads == 0 || queue.is_empty() {
        return Vec::new();
    }
    let drain_count = queue.len().min(max_payloads);
    queue.drain(..drain_count).collect()
}

fn outbound_payload_from_clipboard_payload(payload: &ClipboardPayload) -> OutboundPayload {
    match payload {
        ClipboardPayload::Text(text) => OutboundPayload::ClipboardText { text: text.clone() },
        ClipboardPayload::Image(image_bmp) => OutboundPayload::ClipboardImage {
            image_bmp: image_bmp.clone(),
        },
    }
}

fn clipboard_payload_from_outbound_payload(payload: &OutboundPayload) -> Option<ClipboardPayload> {
    match payload {
        OutboundPayload::ClipboardText { text } => Some(ClipboardPayload::Text(text.clone())),
        OutboundPayload::ClipboardImage { image_bmp } => {
            Some(ClipboardPayload::Image(image_bmp.clone()))
        }
        _ => None,
    }
}

fn outbound_input_move_only_delta(payload: &OutboundPayload) -> Option<(u64, i64, i32, i32)> {
    let OutboundPayload::InputFrame {
        sequence,
        timestamp_unix_ms,
        events,
    } = payload
    else {
        return None;
    };

    let mut dx = 0i32;
    let mut dy = 0i32;
    let mut saw_event = false;
    for event in events {
        let InputEvent::MouseMove {
            dx: event_dx,
            dy: event_dy,
        } = event
        else {
            return None;
        };
        dx = dx.saturating_add(*event_dx);
        dy = dy.saturating_add(*event_dy);
        saw_event = true;
    }

    saw_event.then_some((*sequence, *timestamp_unix_ms, dx, dy))
}

fn replace_outbound_input_move_only(
    payload: &mut OutboundPayload,
    sequence: u64,
    timestamp_unix_ms: i64,
    dx: i32,
    dy: i32,
) -> usize {
    let OutboundPayload::InputFrame {
        sequence: payload_sequence,
        timestamp_unix_ms: payload_timestamp,
        events,
    } = payload
    else {
        return 0;
    };

    *payload_sequence = sequence;
    *payload_timestamp = timestamp_unix_ms;
    *events = if dx == 0 && dy == 0 {
        Vec::new()
    } else {
        vec![InputEvent::MouseMove { dx, dy }]
    };
    events.len()
}

fn try_coalesce_outgoing_input_back(
    queue: &mut VecDeque<OutboundPayload>,
    newer: &OutboundPayload,
) -> Option<(u64, u64, usize)> {
    let (older_sequence, _, older_dx, older_dy) = outbound_input_move_only_delta(queue.back()?)?;
    let (newer_sequence, newer_timestamp, newer_dx, newer_dy) =
        outbound_input_move_only_delta(newer)?;
    let merged_dx = older_dx.saturating_add(newer_dx);
    let merged_dy = older_dy.saturating_add(newer_dy);
    let merged_event_count = replace_outbound_input_move_only(
        queue.back_mut()?,
        newer_sequence,
        newer_timestamp,
        merged_dx,
        merged_dy,
    );
    if merged_event_count == 0 {
        queue.pop_back();
    }
    Some((older_sequence, newer_sequence, merged_event_count))
}

fn try_coalesce_outgoing_input_front(
    queue: &mut VecDeque<OutboundPayload>,
    older: &OutboundPayload,
) -> Option<(u64, u64, usize)> {
    let (older_sequence, _, older_dx, older_dy) = outbound_input_move_only_delta(older)?;
    let (newer_sequence, newer_timestamp, newer_dx, newer_dy) =
        outbound_input_move_only_delta(queue.front()?)?;
    let merged_dx = older_dx.saturating_add(newer_dx);
    let merged_dy = older_dy.saturating_add(newer_dy);
    let merged_event_count = replace_outbound_input_move_only(
        queue.front_mut()?,
        newer_sequence,
        newer_timestamp,
        merged_dx,
        merged_dy,
    );
    if merged_event_count == 0 {
        queue.pop_front();
    }
    Some((older_sequence, newer_sequence, merged_event_count))
}

fn remove_oldest_coalescible_outgoing_input(queue: &mut VecDeque<OutboundPayload>) -> Option<u64> {
    let index = queue
        .iter()
        .position(|payload| outbound_input_move_only_delta(payload).is_some())?;
    queue.remove(index).and_then(|payload| {
        let OutboundPayload::InputFrame { sequence, .. } = payload else {
            return None;
        };
        Some(sequence)
    })
}

impl AppState {
    fn capture_obsolete_inflight_replay(
        sync: &mut ClipboardSyncState,
        replay: &ClipboardReplayState,
    ) {
        for peer_id in &replay.inflight_peer_ids {
            sync.obsolete_inflight_replay_hashes_by_peer
                .entry(peer_id.clone())
                .or_default()
                .insert(replay.hash.clone());
        }
    }

    async fn prune_stale_outgoing_clipboard_payloads(&self) {
        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        queue_map.retain(|_, queue| {
            queue.retain(|payload| {
                !matches!(
                    payload,
                    OutboundPayload::ClipboardText { .. } | OutboundPayload::ClipboardImage { .. }
                )
            });
            !queue.is_empty()
        });
    }

    async fn store_latest_clipboard_replay(
        &self,
        payload: ClipboardPayload,
        hash: String,
        source_peer_ids: HashSet<String>,
    ) {
        let mut sync = self.clipboard.sync.write().await;
        let previous = sync.pending_replay.clone();
        if let Some(previous) = previous.as_ref() {
            Self::capture_obsolete_inflight_replay(&mut sync, previous);
        }
        sync.pending_replay = Some(ClipboardReplayState {
            payload,
            hash: hash.clone(),
            source_peer_ids,
            scheduled_peer_ids: HashSet::new(),
            inflight_peer_ids: HashSet::new(),
        });
        sync.last_observed_hash = Some(hash);
    }

    async fn should_drop_obsolete_inflight_clipboard_payload(
        &self,
        peer_id: &str,
        payload: &OutboundPayload,
    ) -> bool {
        let Some(clipboard_payload) = clipboard_payload_from_outbound_payload(payload) else {
            return false;
        };
        let hash = payload_hash_hex(&clipboard_payload);

        let mut sync = self.clipboard.sync.write().await;
        let Some(obsolete_hashes) = sync
            .obsolete_inflight_replay_hashes_by_peer
            .get_mut(peer_id)
        else {
            return false;
        };
        if !obsolete_hashes.remove(hash.as_str()) {
            return false;
        }
        if obsolete_hashes.is_empty() {
            sync.obsolete_inflight_replay_hashes_by_peer.remove(peer_id);
        }
        true
    }

    pub(crate) async fn has_current_clipboard_replay_delivery_pending_for_peer(
        &self,
        peer_id: &str,
    ) -> bool {
        let replay_state = {
            let sync = self.clipboard.sync.read().await;
            sync.pending_replay.clone()
        };
        let Some(replay_state) = replay_state else {
            return false;
        };
        if replay_state.source_peer_ids.contains(peer_id)
            || replay_state.scheduled_peer_ids.contains(peer_id)
            || replay_state.inflight_peer_ids.contains(peer_id)
        {
            return true;
        }

        let queue_map = self.transport.outgoing_bulk_payloads.read().await;
        queue_map.get(peer_id).is_some_and(|queue| {
            queue.iter().any(|payload| {
                clipboard_payload_from_outbound_payload(payload)
                    .is_some_and(|payload| payload_hash_hex(&payload) == replay_state.hash)
            })
        })
    }

    pub(crate) async fn schedule_pending_clipboard_replay_for_peer(&self, peer_id: &str) -> bool {
        let mut sync = self.clipboard.sync.write().await;
        let Some(replay) = sync.pending_replay.as_mut() else {
            return false;
        };
        if replay.source_peer_ids.contains(peer_id)
            || replay.scheduled_peer_ids.contains(peer_id)
            || replay.inflight_peer_ids.contains(peer_id)
        {
            return false;
        }
        replay.scheduled_peer_ids.insert(peer_id.to_string())
    }

    pub(crate) async fn clear_pending_clipboard_replay_for_peer(&self, peer_id: &str) {
        let mut sync = self.clipboard.sync.write().await;
        let Some(replay) = sync.pending_replay.as_mut() else {
            return;
        };
        replay.scheduled_peer_ids.remove(peer_id);
        replay.inflight_peer_ids.remove(peer_id);
    }

    pub(crate) async fn clear_obsolete_inflight_clipboard_replays_for_peer(&self, peer_id: &str) {
        self.clipboard
            .sync
            .write()
            .await
            .obsolete_inflight_replay_hashes_by_peer
            .remove(peer_id);
    }

    async fn take_scheduled_clipboard_replay_for_peer(
        &self,
        peer_id: &str,
    ) -> Option<OutboundPayload> {
        let mut sync = self.clipboard.sync.write().await;
        let replay = sync.pending_replay.as_mut()?;
        if !replay.scheduled_peer_ids.remove(peer_id) {
            return None;
        }
        replay.inflight_peer_ids.insert(peer_id.to_string());
        Some(outbound_payload_from_clipboard_payload(&replay.payload))
    }

    async fn restore_replayed_clipboard_payload(
        &self,
        peer_id: &str,
        payload: &OutboundPayload,
    ) -> bool {
        let Some(clipboard_payload) = clipboard_payload_from_outbound_payload(payload) else {
            return false;
        };
        let hash = payload_hash_hex(&clipboard_payload);

        let mut sync = self.clipboard.sync.write().await;
        let Some(replay) = sync.pending_replay.as_mut() else {
            return false;
        };
        if replay.hash != hash || !replay.inflight_peer_ids.remove(peer_id) {
            return false;
        }
        replay.scheduled_peer_ids.insert(peer_id.to_string());
        true
    }

    pub(crate) async fn queue_outgoing_bulk_payload(
        &self,
        peer_id: &str,
        payload: OutboundPayload,
    ) {
        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            queue_map
                .entry(peer_id.to_string())
                .or_default()
                .push_back(payload);
        }
        self.notify_outgoing_flush_signal();
    }

    pub(crate) async fn queue_outgoing_input_payload(
        &self,
        peer_id: &str,
        payload: OutboundPayload,
    ) -> OutgoingInputQueueReport {
        let mut queue_map = self.transport.outgoing_input_payloads.write().await;
        let queue = queue_map.entry(peer_id.to_string()).or_default();

        if let Some((older_sequence, newer_sequence, merged_event_count)) =
            try_coalesce_outgoing_input_back(queue, &payload)
        {
            let depth = queue.len();
            drop(queue_map);
            let high_water = self.observe_outgoing_input_high_water(peer_id, depth);
            if let Some(depth) = high_water {
                self.record_input_queue_high_water("outgoing_input", peer_id, depth);
            }
            self.record_input_queue_coalesced(
                "outgoing_input",
                peer_id,
                older_sequence,
                newer_sequence,
                merged_event_count,
            );
            self.notify_outgoing_flush_signal();
            return OutgoingInputQueueReport {
                enqueued: true,
                depth,
                dropped: None,
                coalesced: Some((older_sequence, newer_sequence, merged_event_count)),
                high_water,
            };
        }

        let incoming_is_move_only = outbound_input_move_only_delta(&payload).is_some();
        let mut maybe_payload = Some(payload);
        let mut dropped = None;
        if queue.len() >= MAX_PENDING_OUTGOING_INPUT_FRAMES {
            dropped = remove_oldest_coalescible_outgoing_input(queue)
                .map(|sequence| (sequence, "evict_oldest_move"));

            if dropped.is_none() {
                if incoming_is_move_only {
                    dropped = maybe_payload.as_ref().and_then(|payload| {
                        let OutboundPayload::InputFrame { sequence, .. } = payload else {
                            return None;
                        };
                        Some((*sequence, "drop_new_move"))
                    });
                    maybe_payload = None;
                } else {
                    dropped = queue.pop_front().and_then(|payload| {
                        let OutboundPayload::InputFrame { sequence, .. } = payload else {
                            return None;
                        };
                        Some((sequence, "evict_oldest_fallback"))
                    });
                }
            }
        }

        let enqueued = maybe_payload.is_some();
        if let Some(payload) = maybe_payload.take() {
            queue.push_back(payload);
        }
        let depth = queue.len();
        drop(queue_map);
        let high_water = self.observe_outgoing_input_high_water(peer_id, depth);

        if let Some((sequence, reason)) = dropped {
            self.record_input_queue_overflow_drop("outgoing_input", peer_id, sequence, reason);
        }

        if let Some(depth) = high_water {
            self.record_input_queue_high_water("outgoing_input", peer_id, depth);
        }
        self.notify_outgoing_flush_signal();
        OutgoingInputQueueReport {
            enqueued,
            depth,
            dropped,
            coalesced: None,
            high_water,
        }
    }

    pub async fn queue_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        self.queue_outgoing_bulk_payload(peer_id, OutboundPayload::ClipboardText { text })
            .await;
        Ok(())
    }

    pub async fn queue_clipboard_image(&self, peer_id: &str, image_bmp: Vec<u8>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        validate_bmp_payload(&image_bmp).context("invalid clipboard BMP payload")?;

        self.queue_outgoing_bulk_payload(peer_id, OutboundPayload::ClipboardImage { image_bmp })
            .await;
        Ok(())
    }

    pub async fn queue_local_clipboard_text_for_connected_peers(
        &self,
        text: String,
    ) -> Result<bool> {
        self.queue_local_clipboard_payload_for_connected_peers(ClipboardPayload::Text(text))
            .await
            .map(LocalClipboardObservationOutcome::delivered)
    }

    pub async fn queue_local_clipboard_image_for_connected_peers(
        &self,
        image_bmp: Vec<u8>,
    ) -> Result<bool> {
        self.queue_local_clipboard_payload_for_connected_peers(ClipboardPayload::Image(image_bmp))
            .await
            .map(LocalClipboardObservationOutcome::delivered)
    }

    pub(super) async fn queue_local_clipboard_payload_for_connected_peers(
        &self,
        payload: ClipboardPayload,
    ) -> Result<LocalClipboardObservationOutcome> {
        let initially_connected_peer_ids = self.connected_peer_ids().await;
        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(LocalClipboardObservationOutcome::Disabled),
        };

        {
            let mut sync = self.clipboard.sync.write().await;
            if let Some(suppress_hash) = sync.suppress_echo_hash.as_deref() {
                if suppress_hash == hash.as_str() {
                    sync.suppress_echo_hash = None;
                    sync.last_observed_hash = Some(hash);
                    return Ok(LocalClipboardObservationOutcome::SuppressedEcho);
                }
                // Clipboard moved on before the echo value was observed; drop stale token.
                sync.suppress_echo_hash = None;
            }

            if sync.last_observed_hash.as_deref() == Some(hash.as_str()) {
                return Ok(LocalClipboardObservationOutcome::Duplicate);
            }
        }

        let initially_connected_payloads = initially_connected_peer_ids
            .iter()
            .map(|peer_id| {
                (
                    peer_id.clone(),
                    outbound_payload_from_clipboard_payload(&payload),
                )
            })
            .collect::<Vec<_>>();

        self.prune_stale_outgoing_clipboard_payloads().await;
        self.store_latest_clipboard_replay(payload, hash, HashSet::new())
            .await;

        let currently_connected_peer_ids = self.connected_peer_ids().await;
        let late_connected_peer_ids = currently_connected_peer_ids
            .iter()
            .filter(|peer_id| !initially_connected_peer_ids.contains(*peer_id))
            .cloned()
            .collect::<Vec<_>>();

        let mut scheduled_late_replay = false;
        for peer_id in &late_connected_peer_ids {
            if self
                .schedule_pending_clipboard_replay_for_peer(peer_id.as_str())
                .await
            {
                scheduled_late_replay = true;
            }
        }

        if initially_connected_peer_ids.is_empty() && !scheduled_late_replay {
            return Ok(LocalClipboardObservationOutcome::Accepted { delivered: false });
        }

        for peer_id in &initially_connected_peer_ids {
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
        }

        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            for (peer_id, outbound) in initially_connected_payloads {
                queue_map.entry(peer_id).or_default().push_back(outbound);
            }
        }
        self.notify_outgoing_flush_signal();

        Ok(LocalClipboardObservationOutcome::Accepted {
            delivered: !initially_connected_peer_ids.is_empty() || scheduled_late_replay,
        })
    }

    pub async fn enqueue_remote_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        self.enqueue_remote_clipboard_payload(peer_id, ClipboardPayload::Text(text))
            .await
    }

    pub async fn enqueue_remote_clipboard_image(
        &self,
        peer_id: &str,
        image_bmp: Vec<u8>,
    ) -> Result<()> {
        self.enqueue_remote_clipboard_payload(peer_id, ClipboardPayload::Image(image_bmp))
            .await
    }

    async fn enqueue_remote_clipboard_payload(
        &self,
        peer_id: &str,
        payload: ClipboardPayload,
    ) -> Result<()> {
        match &payload {
            ClipboardPayload::Text(text) => {
                self.record_incoming_clipboard_text(peer_id, text).await;
            }
            ClipboardPayload::Image(image_bmp) => {
                self.record_incoming_clipboard_image(peer_id, image_bmp.len())
                    .await;
            }
        }

        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(()),
        };

        let mut sync = self.clipboard.sync.write().await;
        if sync.last_observed_hash.as_deref() == Some(hash.as_str()) {
            if let Some(replay) = sync.pending_replay.as_mut()
                && replay.hash == hash
            {
                replay.source_peer_ids.insert(peer_id.to_string());
            }
            return Ok(());
        }
        if sync.pending_remote.back().map(|item| item.hash.as_str()) == Some(hash.as_str()) {
            return Ok(());
        }
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_front();
        }
        sync.pending_remote
            .push_back(PendingRemoteClipboardPayload {
                peer_id: peer_id.to_string(),
                payload,
                hash,
                retry_count: 0,
            });
        Ok(())
    }

    pub async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload> {
        self.clipboard.sync.write().await.pending_remote.pop_front()
    }

    pub(crate) async fn stage_remote_clipboard_payload_for_broker(
        &self,
    ) -> Option<PendingRemoteClipboardPayload> {
        let mut sync = self.clipboard.sync.write().await;
        if sync.broker_inflight_remote.is_none() {
            sync.broker_inflight_remote = sync.pending_remote.pop_front();
        }
        sync.broker_inflight_remote.clone()
    }

    pub async fn requeue_remote_clipboard_payload_front(
        &self,
        item: PendingRemoteClipboardPayload,
    ) {
        let mut sync = self.clipboard.sync.write().await;
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_back();
        }
        sync.pending_remote.push_front(item);
    }

    pub(crate) async fn requeue_broker_clipboard_inflight(&self) {
        let mut sync = self.clipboard.sync.write().await;
        let Some(item) = sync.broker_inflight_remote.take() else {
            return;
        };
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_back();
        }
        sync.pending_remote.push_front(item);
    }

    /// A successfully accepted local user update is newer than remote values
    /// already waiting for this broker. Drop those stale remote candidates so
    /// they cannot overwrite the local clipboard on the next exchange.
    pub(crate) async fn discard_broker_remote_clipboard_for_local_update(&self) -> usize {
        let mut sync = self.clipboard.sync.write().await;
        let mut discarded = sync.pending_remote.len();
        sync.pending_remote.clear();
        discarded += usize::from(sync.broker_inflight_remote.take().is_some());
        discarded
    }

    pub(crate) async fn report_broker_remote_clipboard_apply(
        &self,
        source_peer_id: &str,
        hash: &str,
        applied: bool,
        error_message: &str,
    ) -> bool {
        let item = {
            let mut sync = self.clipboard.sync.write().await;
            let matches = sync
                .broker_inflight_remote
                .as_ref()
                .is_some_and(|item| item.peer_id == source_peer_id && item.hash == hash);
            if matches {
                sync.broker_inflight_remote.take()
            } else {
                None
            }
        };
        let Some(mut item) = item else {
            return false;
        };

        if applied {
            self.mark_remote_clipboard_applied(&item.peer_id, &item.payload, &item.hash)
                .await;
            return true;
        }

        item.retry_count = item.retry_count.saturating_add(1);
        if item.retry_count > crate::clipboard::MAX_REMOTE_CLIPBOARD_APPLY_RETRIES {
            tracing::warn!(
                peer_id = %item.peer_id,
                retry_count = item.retry_count,
                error = error_message,
                "dropping brokered remote clipboard payload after bounded retries"
            );
            return true;
        }

        tracing::warn!(
            peer_id = %item.peer_id,
            retry_count = item.retry_count,
            error = error_message,
            "broker failed to apply remote clipboard payload; requeueing for retry"
        );
        self.requeue_remote_clipboard_payload_front(item).await;
        true
    }

    pub async fn mark_remote_clipboard_applied(
        &self,
        source_peer_id: &str,
        payload: &ClipboardPayload,
        hash: &str,
    ) {
        self.prune_stale_outgoing_clipboard_payloads().await;
        let mut sync = self.clipboard.sync.write().await;
        let previous = sync.pending_replay.clone();
        if let Some(previous) = previous.as_ref() {
            Self::capture_obsolete_inflight_replay(&mut sync, previous);
        }
        let mut source_peer_ids = HashSet::new();
        source_peer_ids.insert(source_peer_id.to_string());
        sync.suppress_echo_hash = Some(hash.to_string());
        sync.last_observed_hash = Some(hash.to_string());
        sync.pending_replay = Some(ClipboardReplayState {
            payload: payload.clone(),
            hash: hash.to_string(),
            source_peer_ids,
            scheduled_peer_ids: HashSet::new(),
            inflight_peer_ids: HashSet::new(),
        });
    }

    pub async fn queue_file_from_path(&self, peer_id: &str, file_path: &Path) -> Result<String> {
        self.queue_file_from_path_with_previous(peer_id, file_path, None)
            .await
    }

    pub(crate) async fn queue_file_from_path_with_previous(
        &self,
        peer_id: &str,
        file_path: &Path,
        previous_transfer_id: Option<String>,
    ) -> Result<String> {
        let Some(_peer) = self.get_peer(peer_id).await else {
            anyhow::bail!("unknown peer {peer_id}");
        };

        let file_name = file_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .ok_or_else(|| anyhow::anyhow!("invalid file path"))?;

        let transfer_id = uuid::Uuid::new_v4().to_string();
        let source_path = file_path.to_path_buf();
        let outgoing_projection = |total_bytes| OutgoingFileTransferProjection {
            transfer_id: transfer_id.clone(),
            previous_transfer_id: previous_transfer_id.clone(),
            peer_id: peer_id.to_string(),
            file_name: file_name.clone(),
            source_path: source_path.clone(),
            total_bytes,
        };
        let metadata = match tokio::fs::metadata(file_path).await {
            Ok(metadata) => metadata,
            Err(error) => {
                self.record_outgoing_file_transfer_failed(
                    outgoing_projection(0),
                    format!("source_unavailable: {error}"),
                )
                .await;
                return Err(anyhow::Error::from(error));
            }
        };
        if !metadata.is_file() {
            self.record_outgoing_file_transfer_failed(
                outgoing_projection(metadata.len()),
                "source_not_regular_file".to_string(),
            )
            .await;
            anyhow::bail!("file path must reference a regular file");
        }
        if let Err(error) = tokio::fs::File::open(file_path).await {
            self.record_outgoing_file_transfer_failed(
                outgoing_projection(metadata.len()),
                format!("source_open_failed: {error}"),
            )
            .await;
            return Err(anyhow::Error::from(error));
        }
        let total_bytes = metadata.len();
        let source_modified = metadata.modified().ok();
        if let Err(error) =
            validate_transfer_size_with_limit(total_bytes, self.file_transfer_max_bytes().await)
        {
            self.record_outgoing_file_transfer_failed(
                outgoing_projection(total_bytes),
                error.to_string(),
            )
            .await;
            return Err(anyhow::Error::from(error));
        }

        self.record_outgoing_file_transfer_queued(outgoing_projection(total_bytes))
            .await;
        self.outbound_file_transfers.write().await.insert(
            transfer_id.clone(),
            OutboundFileTransfer {
                peer_id: peer_id.to_string(),
                file_name: file_name.clone(),
                source_path,
                total_bytes,
                source_modified,
                offset_bytes: 0,
                source_file: None,
            },
        );
        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            let queue = queue_map.entry(peer_id.to_string()).or_default();
            queue.push_back(OutboundPayload::FileStart {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                total_bytes,
            });
            queue.push_back(OutboundPayload::FileTransferCursor {
                transfer_id: transfer_id.clone(),
            });
        }
        self.notify_outgoing_flush_signal();
        Ok(transfer_id)
    }

    pub(crate) async fn outbound_file_transfer_remaining_bytes(
        &self,
        peer_id: &str,
        transfer_id: &str,
    ) -> Option<u64> {
        let transfers = self.outbound_file_transfers.read().await;
        let transfer = transfers.get(transfer_id)?;
        if transfer.peer_id != peer_id {
            return None;
        }
        Some(transfer.total_bytes.saturating_sub(transfer.offset_bytes))
    }

    pub(crate) async fn materialize_outbound_file_chunk(
        &self,
        peer_id: &str,
        transfer_id: &str,
    ) -> Result<OutboundFileChunk> {
        let mut transfers = self.outbound_file_transfers.write().await;
        let Some(transfer) = transfers.get_mut(transfer_id) else {
            anyhow::bail!("unknown outbound file transfer {transfer_id}");
        };
        if transfer.peer_id != peer_id {
            anyhow::bail!("outbound file transfer {transfer_id} does not belong to peer {peer_id}");
        }

        let metadata = tokio::fs::metadata(&transfer.source_path)
            .await
            .with_context(|| {
                format!(
                    "inspect outbound file source {}",
                    transfer.source_path.display()
                )
            })?;
        if !metadata.is_file() {
            anyhow::bail!("outbound file source is no longer a regular file");
        }
        if metadata.len() != transfer.total_bytes
            || (transfer.source_modified.is_some()
                && metadata.modified().ok() != transfer.source_modified)
        {
            anyhow::bail!("outbound file source changed after transfer was queued");
        }

        if transfer.source_file.is_none() {
            let source_file = tokio::fs::File::open(&transfer.source_path)
                .await
                .with_context(|| {
                    format!(
                        "open outbound file source {}",
                        transfer.source_path.display()
                    )
                })?;
            transfer.source_file = Some(source_file);
        }

        let remaining = transfer.total_bytes.saturating_sub(transfer.offset_bytes);
        let length_bytes = (remaining as usize).min(FILE_TRANSFER_CHUNK_BYTES);
        let offset_bytes = transfer.offset_bytes;
        let mut data = vec![0u8; length_bytes];
        if length_bytes > 0 {
            let source_file = transfer
                .source_file
                .as_mut()
                .expect("outbound source file should be open before reading");
            source_file
                .seek(std::io::SeekFrom::Start(offset_bytes))
                .await
                .with_context(|| {
                    format!(
                        "seek outbound file source {} to offset {}",
                        transfer.source_path.display(),
                        offset_bytes
                    )
                })?;
            source_file.read_exact(&mut data).await.with_context(|| {
                format!(
                    "read outbound file source {} offset {} length {}",
                    transfer.source_path.display(),
                    offset_bytes,
                    length_bytes
                )
            })?;
        }

        let next_offset = offset_bytes.saturating_add(length_bytes as u64);
        Ok(OutboundFileChunk {
            transfer_id: transfer_id.to_string(),
            offset_bytes,
            data,
            finished: next_offset >= transfer.total_bytes,
        })
    }

    pub(crate) async fn commit_outbound_file_chunk(
        &self,
        peer_id: &str,
        transfer_id: &str,
        offset_bytes: u64,
        length_bytes: usize,
    ) -> bool {
        let mut transfers = self.outbound_file_transfers.write().await;
        let Some(transfer) = transfers.get_mut(transfer_id) else {
            return false;
        };
        if transfer.peer_id != peer_id || transfer.offset_bytes != offset_bytes {
            return false;
        }
        transfer.offset_bytes = transfer.offset_bytes.saturating_add(length_bytes as u64);
        true
    }

    pub(crate) async fn complete_outbound_file_transfer(
        &self,
        peer_id: &str,
        transfer_id: &str,
    ) -> Option<(String, u64)> {
        let mut transfers = self.outbound_file_transfers.write().await;
        if transfers
            .get(transfer_id)
            .is_none_or(|transfer| transfer.peer_id != peer_id)
        {
            return None;
        }
        let transfer = transfers.remove(transfer_id)?;
        drop(transfers);
        self.mark_file_transfer_completed(transfer_id, None).await;
        Some((transfer.file_name, transfer.total_bytes))
    }

    pub(crate) async fn fail_outbound_file_transfer(
        &self,
        peer_id: &str,
        transfer_id: &str,
        reason: &str,
    ) -> bool {
        let mut transfers = self.outbound_file_transfers.write().await;
        if transfers
            .get(transfer_id)
            .is_none_or(|transfer| transfer.peer_id != peer_id)
        {
            return false;
        }
        let transfer = transfers.remove(transfer_id);
        drop(transfers);
        let Some(transfer) = transfer else {
            return false;
        };
        self.mark_file_transfer_failed(transfer_id, reason).await;
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file_transfer_failed".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("transfer_id={transfer_id} reason={reason}"),
            size_bytes: transfer.total_bytes,
        });
        true
    }

    pub async fn drain_outgoing_input(
        &self,
        peer_id: &str,
        max_payloads: usize,
    ) -> Vec<OutboundPayload> {
        let mut queue_map = self.transport.outgoing_input_payloads.write().await;
        let mut drained = {
            let Some(queue) = queue_map.get_mut(peer_id) else {
                return Vec::new();
            };
            drain_queue_up_to(queue, max_payloads)
        };

        if queue_map.get(peer_id).is_some_and(VecDeque::is_empty) {
            queue_map.remove(peer_id);
        }

        drained.shrink_to_fit();
        drained
    }

    pub async fn drain_outgoing_bulk(
        &self,
        peer_id: &str,
        max_payloads: usize,
    ) -> Vec<OutboundPayload> {
        if max_payloads == 0 {
            return Vec::new();
        }

        let replay = self.take_scheduled_clipboard_replay_for_peer(peer_id).await;
        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        let mut drained = Vec::new();
        if let Some(payload) = replay {
            drained.push(payload);
        }

        let remaining_capacity = max_payloads.saturating_sub(drained.len());
        if remaining_capacity > 0 {
            let queued = match queue_map.get_mut(peer_id) {
                Some(queue) => drain_queue_up_to(queue, remaining_capacity),
                None => Vec::new(),
            };
            drained.extend(queued);
        }

        if queue_map.get(peer_id).is_some_and(VecDeque::is_empty) {
            queue_map.remove(peer_id);
        }

        if drained.is_empty() {
            return drained;
        }

        drained.shrink_to_fit();
        drained
    }

    pub async fn remove_queued_file_transfer(&self, peer_id: &str, transfer_id: &str) {
        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        let Some(queue) = queue_map.get_mut(peer_id) else {
            let mut transfers = self.outbound_file_transfers.write().await;
            let removed = if transfers
                .get(transfer_id)
                .is_some_and(|transfer| transfer.peer_id == peer_id)
            {
                transfers.remove(transfer_id).is_some()
            } else {
                false
            };
            drop(transfers);
            if removed {
                self.mark_file_transfer_cancelled(transfer_id, "removed_from_queue")
                    .await;
            }
            return;
        };

        queue.retain(|payload| !outbound_payload_has_transfer_id(payload, transfer_id));
        if queue.is_empty() {
            queue_map.remove(peer_id);
        }
        drop(queue_map);

        let mut transfers = self.outbound_file_transfers.write().await;
        let removed = if transfers
            .get(transfer_id)
            .is_some_and(|transfer| transfer.peer_id == peer_id)
        {
            transfers.remove(transfer_id).is_some()
        } else {
            false
        };
        drop(transfers);
        if removed {
            self.mark_file_transfer_cancelled(transfer_id, "removed_from_queue")
                .await;
        }
    }

    pub async fn cancel_outbound_file_transfer(
        &self,
        peer_id: &str,
        transfer_id: &str,
        reason: &str,
    ) -> bool {
        let mut transfers = self.outbound_file_transfers.write().await;
        if transfers
            .get(transfer_id)
            .is_none_or(|transfer| transfer.peer_id != peer_id)
        {
            return false;
        }
        let removed = transfers.remove(transfer_id);
        drop(transfers);
        let Some(transfer) = removed else {
            return false;
        };

        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        if let Some(queue) = queue_map.get_mut(peer_id) {
            queue.retain(|payload| !outbound_payload_has_transfer_id(payload, transfer_id));
            if queue.is_empty() {
                queue_map.remove(peer_id);
            }
        }
        drop(queue_map);

        self.mark_file_transfer_cancelled(transfer_id, reason).await;
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file_transfer_cancelled".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("transfer_id={transfer_id} reason={reason}"),
            size_bytes: transfer.total_bytes,
        });
        true
    }

    #[cfg(test)]
    pub async fn outgoing_bulk_queue_len(&self, peer_id: &str) -> usize {
        self.transport
            .outgoing_bulk_payloads
            .read()
            .await
            .get(peer_id)
            .map(VecDeque::len)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub async fn outbound_file_transfer_count(&self) -> usize {
        self.outbound_file_transfers.read().await.len()
    }

    #[cfg(test)]
    pub async fn drain_outgoing(&self, peer_id: &str) -> Vec<OutboundPayload> {
        let input = self.drain_outgoing_input(peer_id, usize::MAX).await;
        let bulk = self.drain_outgoing_bulk(peer_id, usize::MAX).await;
        let mut payloads = Vec::with_capacity(input.len() + bulk.len());
        payloads.extend(input);
        payloads.extend(bulk);
        payloads
    }

    pub async fn requeue_outgoing_front(&self, peer_id: &str, payloads: Vec<OutboundPayload>) {
        if payloads.is_empty() {
            return;
        }

        let mut split = OutgoingPeerQueues::default();
        for payload in payloads {
            split.push(payload);
        }

        let mut restored_replay = false;
        let mut requeued_bulk = VecDeque::new();
        while let Some(payload) = split.bulk.pop_front() {
            if self
                .restore_replayed_clipboard_payload(peer_id, &payload)
                .await
            {
                restored_replay = true;
                continue;
            }
            if self
                .should_drop_obsolete_inflight_clipboard_payload(peer_id, &payload)
                .await
            {
                continue;
            }
            requeued_bulk.push_back(payload);
        }

        let has_input = !split.input.is_empty();
        if has_input {
            let mut coalesced = Vec::<(u64, u64, usize)>::new();
            let mut dropped = Vec::<(u64, &'static str)>::new();
            let mut queue_map = self.transport.outgoing_input_payloads.write().await;
            let queue = queue_map.entry(peer_id.to_string()).or_default();
            for payload in split.input.into_iter().rev() {
                if let Some(result) = try_coalesce_outgoing_input_front(queue, &payload) {
                    coalesced.push(result);
                    continue;
                }

                let incoming_is_move_only = outbound_input_move_only_delta(&payload).is_some();
                if queue.len() >= MAX_PENDING_OUTGOING_INPUT_FRAMES {
                    if let Some(sequence) = remove_oldest_coalescible_outgoing_input(queue) {
                        dropped.push((sequence, "evict_oldest_move"));
                    } else if incoming_is_move_only {
                        if let Some((sequence, _, _, _)) = outbound_input_move_only_delta(&payload)
                        {
                            dropped.push((sequence, "drop_new_move"));
                        }
                        continue;
                    } else if let Some(sequence) = queue.pop_back().and_then(|payload| {
                        let OutboundPayload::InputFrame { sequence, .. } = payload else {
                            return None;
                        };
                        Some(sequence)
                    }) {
                        dropped.push((sequence, "evict_newest_fallback"));
                    }
                }

                queue.push_front(payload);
            }
            let depth = queue.len();
            drop(queue_map);

            for (older_sequence, newer_sequence, merged_event_count) in coalesced {
                self.record_input_queue_coalesced(
                    "outgoing_input",
                    peer_id,
                    older_sequence,
                    newer_sequence,
                    merged_event_count,
                );
            }
            for (sequence, reason) in dropped {
                self.record_input_queue_overflow_drop("outgoing_input", peer_id, sequence, reason);
            }
            if let Some(depth) = self.observe_outgoing_input_high_water(peer_id, depth) {
                self.record_input_queue_high_water("outgoing_input", peer_id, depth);
            }
        }

        let has_bulk = !requeued_bulk.is_empty();
        if has_bulk {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            let queue = queue_map.entry(peer_id.to_string()).or_default();
            for payload in requeued_bulk.into_iter().rev() {
                queue.push_front(payload);
            }
        }

        if restored_replay || has_input || has_bulk {
            self.notify_outgoing_flush_signal();
        }
    }

    fn observe_outgoing_input_high_water(&self, peer_id: &str, depth: usize) -> Option<usize> {
        self.transport
            .observe_outgoing_input_high_water(peer_id, depth)
    }

    pub fn record_transport_event(&self, event: TransportEventRecord) {
        self.transport.record_transport_event(event);
    }

    pub async fn transport_events(&self) -> Vec<TransportEventRecord> {
        self.transport.transport_events_snapshot().await
    }

    pub async fn record_incoming_clipboard_text(&self, peer_id: &str, text: &str) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: "payload_type=text disposition=received".to_string(),
            size_bytes: text.len() as u64,
        });
    }

    pub async fn record_outgoing_clipboard_text(&self, peer_id: &str, text: &str) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: "payload_type=text disposition=sent".to_string(),
            size_bytes: text.len() as u64,
        });
    }

    pub async fn record_incoming_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: "payload_type=bmp disposition=received".to_string(),
            size_bytes: size_bytes as u64,
        });
    }

    pub async fn record_outgoing_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: "payload_type=bmp disposition=sent".to_string(),
            size_bytes: size_bytes as u64,
        });
    }

    #[cfg(test)]
    pub async fn store_incoming_file(
        &self,
        peer_id: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<PathBuf> {
        let mut reserved = self
            .reserve_incoming_file(peer_id, file_name, bytes.len() as u64)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut reserved.temp_file, &bytes).await?;
        reserved.temp_file.sync_all().await?;
        drop(reserved.temp_file);
        if let Err(error) =
            complete_reserved_incoming_file(&reserved.temp_path, &reserved.final_path).await
        {
            let _ = tokio::fs::remove_file(&reserved.temp_path).await;
            return Err(error);
        }

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: reserved.sanitized_name,
            size_bytes: tokio::fs::metadata(&reserved.final_path).await?.len(),
        });

        Ok(reserved.final_path)
    }

    pub(crate) async fn reserve_incoming_file(
        &self,
        peer_id: &str,
        file_name: &str,
        size_bytes: u64,
    ) -> Result<ReservedIncomingFile> {
        validate_transfer_size_with_limit(size_bytes, self.file_transfer_max_bytes().await)?;
        let sanitized_name = sanitize_incoming_file_name(file_name)?;

        let peer_dir = self.receive_dir_for_peer(peer_id).await;
        tokio::fs::create_dir_all(&peer_dir).await?;

        reserve_incoming_file_path(&peer_dir, sanitized_name).await
    }

    pub async fn store_incoming_file_from_temp(
        &self,
        peer_id: &str,
        file_name: &str,
        temp_path: &Path,
        size_bytes: u64,
    ) -> Result<PathBuf> {
        let reserved = self
            .reserve_incoming_file(peer_id, file_name, size_bytes)
            .await?;
        let sanitized_name = reserved.sanitized_name;
        let final_path = reserved.final_path;
        let part_path = reserved.temp_path;
        drop(reserved.temp_file);

        match tokio::fs::rename(temp_path, &part_path).await {
            Ok(()) => {
                sync_file_at(&part_path).await?;
            }
            Err(_) => {
                if let Err(error) = copy_file_to_reserved_part(temp_path, &part_path).await {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    return Err(error);
                }
                let _ = tokio::fs::remove_file(temp_path).await;
            }
        }

        if let Err(error) = complete_reserved_incoming_file(&part_path, &final_path).await {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(error);
        }

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: sanitized_name,
            size_bytes,
        });

        Ok(final_path)
    }

    pub(crate) async fn complete_incoming_file(
        &self,
        peer_id: &str,
        sanitized_name: String,
        temp_path: &Path,
        final_path: &Path,
        size_bytes: u64,
    ) -> Result<PathBuf> {
        complete_reserved_incoming_file(temp_path, final_path).await?;

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: sanitized_name,
            size_bytes,
        });

        Ok(final_path.to_path_buf())
    }

    async fn receive_dir_for_peer(&self, peer_id: &str) -> PathBuf {
        let file_transfer = self.config.read().await.file_transfer.clone();
        let receive_dir = PathBuf::from(file_transfer.receive_dir);
        if file_transfer.organize_by_peer {
            receive_dir.join(filesystem_safe_peer_dir_name(peer_id))
        } else {
            receive_dir
        }
    }

    pub async fn record_outgoing_file(&self, peer_id: &str, file_name: &str, size_bytes: u64) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: file_name.to_string(),
            size_bytes,
        });
    }

    pub async fn record_outgoing_input_frame(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        capture_timestamp_unix_ms: i64,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} capture_to_send_ms={} captured_at_unix_ms={capture_timestamp_unix_ms}",
                elapsed_ms(capture_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }
}

fn outbound_payload_has_transfer_id(payload: &OutboundPayload, expected_transfer_id: &str) -> bool {
    match payload {
        OutboundPayload::FileStart { transfer_id, .. }
        | OutboundPayload::FileChunk { transfer_id, .. }
        | OutboundPayload::FileTransferCursor { transfer_id }
        | OutboundPayload::FileEnd { transfer_id, .. } => transfer_id == expected_transfer_id,
        _ => false,
    }
}

fn filesystem_safe_peer_dir_name(peer_id: &str) -> String {
    let digest = Sha256::digest(peer_id.as_bytes());
    format!("peer-{}", bytes_to_hex(&digest[..16]))
}

async fn reserve_incoming_file_path(
    peer_dir: &Path,
    sanitized_name: String,
) -> Result<ReservedIncomingFile> {
    for suffix in 0..=9_999u32 {
        let final_path = peer_dir.join(conflict_file_name(&sanitized_name, suffix));
        if !final_path.starts_with(peer_dir) {
            anyhow::bail!("incoming file path escaped inbox root");
        }
        if tokio::fs::try_exists(&final_path).await? {
            continue;
        }

        let temp_path = incoming_part_path(&final_path)?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
        {
            Ok(temp_file) => {
                return Ok(ReservedIncomingFile {
                    sanitized_name,
                    final_path,
                    temp_path,
                    temp_file,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("reserve inbound file part path"),
        }
    }

    let fallback_name = format!(
        "{} ({})",
        file_stem_or_default(&sanitized_name),
        uuid::Uuid::new_v4()
    );
    let final_path = peer_dir.join(fallback_name);
    let temp_path = incoming_part_path(&final_path)?;
    let temp_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
        .context("reserve fallback inbound file part path")?;

    Ok(ReservedIncomingFile {
        sanitized_name,
        final_path,
        temp_path,
        temp_file,
    })
}

fn conflict_file_name(file_name: &str, suffix: u32) -> String {
    if suffix == 0 {
        return file_name.to_string();
    }

    let stem = file_stem_or_default(file_name);
    let extension = Path::new(file_name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_string());
    let mut candidate = format!("{stem} ({suffix})");
    if let Some(extension) = extension {
        candidate.push('.');
        candidate.push_str(&extension);
    }
    candidate
}

fn file_stem_or_default(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "file".to_string())
}

fn incoming_part_path(final_path: &Path) -> Result<PathBuf> {
    let final_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("incoming final path must have UTF-8 file name")?;
    Ok(final_path.with_file_name(format!(".{final_name}.boundless.part")))
}

async fn copy_file_to_reserved_part(source_path: &Path, part_path: &Path) -> Result<()> {
    let mut source = tokio::fs::File::open(source_path)
        .await
        .with_context(|| format!("open inbound temp source {}", source_path.display()))?;
    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(part_path)
        .await
        .with_context(|| format!("open inbound reserved part {}", part_path.display()))?;
    tokio::io::copy(&mut source, &mut target)
        .await
        .with_context(|| format!("copy inbound temp payload to {}", part_path.display()))?;
    target
        .sync_all()
        .await
        .with_context(|| format!("sync inbound reserved part {}", part_path.display()))
}

async fn sync_file_at(path: &Path) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .await
        .with_context(|| format!("open inbound part for sync {}", path.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("sync inbound part {}", path.display()))
}

async fn complete_reserved_incoming_file(temp_path: &Path, final_path: &Path) -> Result<()> {
    if tokio::fs::try_exists(final_path).await? {
        anyhow::bail!(
            "incoming file destination already exists: {}",
            final_path.display()
        );
    }
    tokio::fs::rename(temp_path, final_path)
        .await
        .with_context(|| format!("finalize inbound file {}", final_path.display()))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
