use super::*;

#[derive(Debug)]
#[allow(dead_code)]
struct OutgoingInputQueueReport {
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

    async fn queue_outgoing_bulk_payload(&self, peer_id: &str, payload: OutboundPayload) {
        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            queue_map
                .entry(peer_id.to_string())
                .or_default()
                .push_back(payload);
        }
        self.notify_outgoing_flush_signal();
    }

    async fn queue_outgoing_input_payload(
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
    }

    pub async fn queue_local_clipboard_image_for_connected_peers(
        &self,
        image_bmp: Vec<u8>,
    ) -> Result<bool> {
        self.queue_local_clipboard_payload_for_connected_peers(ClipboardPayload::Image(image_bmp))
            .await
    }

    async fn queue_local_clipboard_payload_for_connected_peers(
        &self,
        payload: ClipboardPayload,
    ) -> Result<bool> {
        let initially_connected_peer_ids = self.connected_peer_ids().await;
        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(false),
        };

        {
            let mut sync = self.clipboard.sync.write().await;
            if let Some(suppress_hash) = sync.suppress_echo_hash.as_deref() {
                if suppress_hash == hash.as_str() {
                    sync.suppress_echo_hash = None;
                    sync.last_observed_hash = Some(hash);
                    return Ok(false);
                }
                // Clipboard moved on before the echo value was observed; drop stale token.
                sync.suppress_echo_hash = None;
            }

            if sync.last_observed_hash.as_deref() == Some(hash.as_str()) {
                return Ok(false);
            }
        }

        self.prune_stale_outgoing_clipboard_payloads().await;
        self.store_latest_clipboard_replay(payload.clone(), hash, HashSet::new())
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
            return Ok(false);
        }

        for peer_id in &initially_connected_peer_ids {
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
        }

        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            for peer_id in &initially_connected_peer_ids {
                let outbound = match &payload {
                    ClipboardPayload::Text(text) => {
                        OutboundPayload::ClipboardText { text: text.clone() }
                    }
                    ClipboardPayload::Image(bytes) => OutboundPayload::ClipboardImage {
                        image_bmp: bytes.clone(),
                    },
                };

                queue_map
                    .entry(peer_id.clone())
                    .or_default()
                    .push_back(outbound);
            }
        }
        self.notify_outgoing_flush_signal();

        Ok(!initially_connected_peer_ids.is_empty() || scheduled_late_replay)
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

    pub async fn queue_file_from_path(&self, peer_id: &str, file_path: &Path) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let file_name = file_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .ok_or_else(|| anyhow::anyhow!("invalid file path"))?;

        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(anyhow::Error::from)?;
        if !metadata.is_file() {
            anyhow::bail!("file path must reference a regular file");
        }
        tokio::fs::File::open(file_path)
            .await
            .map_err(anyhow::Error::from)?;
        let total_bytes = metadata.len();
        validate_transfer_size(total_bytes)?;

        let transfer_id = uuid::Uuid::new_v4().to_string();
        let source_path = file_path.to_path_buf();
        {
            let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
            let queue = queue_map.entry(peer_id.to_string()).or_default();
            queue.push_back(OutboundPayload::FileStart {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                total_bytes,
            });
            let mut offset_bytes = 0u64;
            while offset_bytes < total_bytes {
                let remaining = (total_bytes - offset_bytes) as usize;
                let length_bytes = remaining.min(FILE_TRANSFER_CHUNK_BYTES);
                queue.push_back(OutboundPayload::FileChunk {
                    transfer_id: transfer_id.clone(),
                    source_path: source_path.clone(),
                    offset_bytes,
                    length_bytes,
                });
                offset_bytes = offset_bytes.saturating_add(length_bytes as u64);
            }
            queue.push_back(OutboundPayload::FileEnd {
                transfer_id,
                file_name,
                total_bytes,
            });
        }
        self.notify_outgoing_flush_signal();
        Ok(())
    }

    pub async fn queue_input_move(&self, peer_id: &str, dx: i32, dy: i32) -> Result<()> {
        self.queue_input_events(peer_id, vec![InputEvent::MouseMove { dx, dy }])
            .await
    }

    pub async fn queue_input_key(
        &self,
        peer_id: &str,
        scan_code: u16,
        key_state: KeyState,
    ) -> Result<()> {
        self.queue_input_events(
            peer_id,
            vec![InputEvent::Key {
                scan_code,
                state: key_state,
            }],
        )
        .await
    }

    pub async fn queue_input_events(&self, peer_id: &str, events: Vec<InputEvent>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        if events.is_empty() {
            anyhow::bail!("input frame must include at least one event");
        }
        if events.len() > MAX_EVENTS_PER_FRAME {
            anyhow::bail!(
                "input frame event count exceeds limit: {} > {}",
                events.len(),
                MAX_EVENTS_PER_FRAME
            );
        }

        let sequence = {
            let mut sequences = self.input.sequence_by_peer.write().await;
            let entry = sequences.entry(peer_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };

        self.queue_outgoing_input_payload(
            peer_id,
            OutboundPayload::InputFrame {
                sequence,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events,
            },
        )
        .await;
        Ok(())
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
        let preview = text.chars().take(80).collect::<String>();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: preview,
            size_bytes: text.len() as u64,
        });
    }

    pub async fn record_outgoing_clipboard_text(&self, peer_id: &str, text: &str) {
        let preview = text.chars().take(80).collect::<String>();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: preview,
            size_bytes: text.len() as u64,
        });
    }

    pub async fn record_incoming_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
            size_bytes: size_bytes as u64,
        });
    }

    pub async fn record_outgoing_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
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
        validate_transfer_size(bytes.len() as u64)?;
        let sanitized_name = sanitize_incoming_file_name(file_name)?;

        let peer_dir = self.inbox_root.join(peer_id);
        tokio::fs::create_dir_all(&peer_dir).await?;

        let final_path = resolve_conflict_path(&peer_dir, &sanitized_name);
        if !final_path.starts_with(&peer_dir) {
            anyhow::bail!("incoming file path escaped inbox root");
        }
        tokio::fs::write(&final_path, bytes).await?;

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: final_path.display().to_string(),
            size_bytes: tokio::fs::metadata(&final_path).await?.len(),
        });

        Ok(final_path)
    }

    pub async fn store_incoming_file_from_temp(
        &self,
        peer_id: &str,
        file_name: &str,
        temp_path: &Path,
        size_bytes: u64,
    ) -> Result<PathBuf> {
        validate_transfer_size(size_bytes)?;
        let sanitized_name = sanitize_incoming_file_name(file_name)?;

        let peer_dir = self.inbox_root.join(peer_id);
        tokio::fs::create_dir_all(&peer_dir).await?;

        let final_path = resolve_conflict_path(&peer_dir, &sanitized_name);
        if !final_path.starts_with(&peer_dir) {
            anyhow::bail!("incoming file path escaped inbox root");
        }

        match tokio::fs::rename(temp_path, &final_path).await {
            Ok(()) => {}
            Err(_) => {
                tokio::fs::copy(temp_path, &final_path).await?;
                let _ = tokio::fs::remove_file(temp_path).await;
            }
        }

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: final_path.display().to_string(),
            size_bytes,
        });

        Ok(final_path)
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
