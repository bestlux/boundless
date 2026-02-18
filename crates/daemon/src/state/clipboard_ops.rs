use super::*;

impl AppState {
    pub async fn queue_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::ClipboardText { text });
        Ok(())
    }

    pub async fn queue_clipboard_image(&self, peer_id: &str, image_bmp: Vec<u8>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        validate_bmp_payload(&image_bmp).context("invalid clipboard BMP payload")?;

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::ClipboardImage { image_bmp });
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
        let connected_peer_ids = self.connected_peer_ids().await;
        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(false),
        };

        {
            let mut sync = self.clipboard_sync.write().await;
            if let Some(suppress_hash) = sync.suppress_echo_hash.as_deref() {
                if suppress_hash == hash.as_str() {
                    sync.suppress_echo_hash = None;
                    sync.last_observed_hash = Some(hash);
                    return Ok(false);
                }
                // Clipboard moved on before the echo value was observed; drop stale token.
                sync.suppress_echo_hash = None;
            }

            if connected_peer_ids.is_empty() {
                // Do not cache disconnected observations. On next peer connect we should
                // still broadcast current clipboard contents.
                sync.last_observed_hash = None;
                return Ok(false);
            }

            if sync.last_observed_hash.as_deref() == Some(hash.as_str()) {
                return Ok(false);
            }
            sync.last_observed_hash = Some(hash);
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        for peer_id in &connected_peer_ids {
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

        Ok(true)
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

        let mut sync = self.clipboard_sync.write().await;
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
            });
        Ok(())
    }

    pub async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload> {
        self.clipboard_sync.write().await.pending_remote.pop_front()
    }

    pub async fn requeue_remote_clipboard_payload_front(
        &self,
        item: PendingRemoteClipboardPayload,
    ) {
        let mut sync = self.clipboard_sync.write().await;
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_back();
        }
        sync.pending_remote.push_front(item);
    }

    pub async fn mark_remote_clipboard_applied(&self, hash: &str) {
        let mut sync = self.clipboard_sync.write().await;
        sync.suppress_echo_hash = Some(hash.to_string());
        sync.last_observed_hash = Some(hash.to_string());
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
        validate_transfer_size(metadata.len())?;
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(anyhow::Error::from)?;

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::File { file_name, bytes });
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
            let mut sequences = self.input_sequence_by_peer.write().await;
            let entry = sequences.entry(peer_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::InputFrame {
                sequence,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events,
            });
        Ok(())
    }

    pub async fn drain_outgoing(&self, peer_id: &str) -> Vec<OutboundPayload> {
        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .remove(peer_id)
            .map(|queue| queue.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
    }

    pub async fn requeue_outgoing_front(&self, peer_id: &str, payloads: Vec<OutboundPayload>) {
        if payloads.is_empty() {
            return;
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        let queue = queue_map.entry(peer_id.to_string()).or_default();
        for payload in payloads.into_iter().rev() {
            queue.push_front(payload);
        }
    }

    pub async fn record_transport_event(&self, event: TransportEventRecord) {
        let mut events = self.transport_events.write().await;
        events.push_back(event);
        while events.len() > MAX_TRANSPORT_EVENTS {
            events.pop_front();
        }
    }

    pub async fn transport_events(&self) -> Vec<TransportEventRecord> {
        self.transport_events.read().await.iter().cloned().collect()
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
        })
        .await;
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
        })
        .await;
    }

    pub async fn record_incoming_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
            size_bytes: size_bytes as u64,
        })
        .await;
    }

    pub async fn record_outgoing_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
            size_bytes: size_bytes as u64,
        })
        .await;
    }

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
        })
        .await;

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
        })
        .await;
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
        })
        .await;
    }
}
