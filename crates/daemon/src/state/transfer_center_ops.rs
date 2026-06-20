use super::*;

impl AppState {
    pub(crate) async fn file_transfer_records(&self) -> Vec<FileTransferRecord> {
        self.file_transfer_records
            .read()
            .await
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) async fn record_outgoing_file_transfer_queued(
        &self,
        transfer: OutgoingFileTransferProjection,
    ) {
        let now = Utc::now();
        self.push_file_transfer_record(FileTransferRecord {
            transfer_id: transfer.transfer_id,
            previous_transfer_id: transfer.previous_transfer_id,
            direction: FileTransferDirection::Outgoing,
            peer_id: transfer.peer_id,
            file_name: transfer.file_name,
            state: FileTransferState::Queued,
            transferred_bytes: 0,
            total_bytes: transfer.total_bytes,
            failure_reason: None,
            source_path: Some(transfer.source_path),
            final_path: None,
            queued_at: now,
            updated_at: now,
        })
        .await;
    }

    pub(crate) async fn record_outgoing_file_transfer_failed(
        &self,
        transfer: OutgoingFileTransferProjection,
        reason: String,
    ) {
        let now = Utc::now();
        self.push_file_transfer_record(FileTransferRecord {
            transfer_id: transfer.transfer_id,
            previous_transfer_id: transfer.previous_transfer_id,
            direction: FileTransferDirection::Outgoing,
            peer_id: transfer.peer_id,
            file_name: transfer.file_name,
            state: FileTransferState::Failed,
            transferred_bytes: 0,
            total_bytes: transfer.total_bytes,
            failure_reason: Some(reason),
            source_path: Some(transfer.source_path),
            final_path: None,
            queued_at: now,
            updated_at: now,
        })
        .await;
    }

    pub(crate) async fn record_incoming_file_transfer_started(
        &self,
        transfer_id: String,
        peer_id: String,
        file_name: String,
        total_bytes: u64,
        final_path: PathBuf,
    ) {
        let now = Utc::now();
        self.push_file_transfer_record(FileTransferRecord {
            transfer_id,
            previous_transfer_id: None,
            direction: FileTransferDirection::Incoming,
            peer_id,
            file_name,
            state: FileTransferState::Active,
            transferred_bytes: 0,
            total_bytes,
            failure_reason: None,
            source_path: None,
            final_path: Some(final_path),
            queued_at: now,
            updated_at: now,
        })
        .await;
    }

    pub(crate) async fn record_incoming_file_transfer_failed(
        &self,
        transfer_id: String,
        peer_id: String,
        file_name: String,
        total_bytes: u64,
        reason: String,
    ) {
        let now = Utc::now();
        self.push_file_transfer_record(FileTransferRecord {
            transfer_id,
            previous_transfer_id: None,
            direction: FileTransferDirection::Incoming,
            peer_id,
            file_name,
            state: FileTransferState::Failed,
            transferred_bytes: 0,
            total_bytes,
            failure_reason: Some(reason),
            source_path: None,
            final_path: None,
            queued_at: now,
            updated_at: now,
        })
        .await;
    }

    pub(crate) async fn mark_file_transfer_active(&self, transfer_id: &str) {
        self.update_file_transfer_record(transfer_id, |record, now| {
            if !record.state.is_terminal() {
                record.state = FileTransferState::Active;
                record.updated_at = now;
            }
        })
        .await;
    }

    pub(crate) async fn mark_file_transfer_progress(
        &self,
        transfer_id: &str,
        transferred_bytes: u64,
    ) {
        self.update_file_transfer_record(transfer_id, |record, now| {
            if record.state.is_terminal() {
                return;
            }
            record.state = FileTransferState::Active;
            record.transferred_bytes = record
                .transferred_bytes
                .max(transferred_bytes)
                .min(record.total_bytes);
            record.updated_at = now;
        })
        .await;
    }

    pub(crate) async fn mark_file_transfer_completed(
        &self,
        transfer_id: &str,
        final_path: Option<PathBuf>,
    ) {
        self.update_file_transfer_record(transfer_id, |record, now| {
            record.state = FileTransferState::Completed;
            record.transferred_bytes = record.total_bytes;
            record.failure_reason = None;
            if final_path.is_some() {
                record.final_path = final_path.clone();
            }
            record.updated_at = now;
        })
        .await;
    }

    pub(crate) async fn mark_file_transfer_failed(&self, transfer_id: &str, reason: &str) {
        self.update_file_transfer_record(transfer_id, |record, now| {
            record.state = FileTransferState::Failed;
            record.failure_reason = Some(reason.to_string());
            record.updated_at = now;
        })
        .await;
    }

    pub(crate) async fn mark_file_transfer_cancelled(&self, transfer_id: &str, reason: &str) {
        self.update_file_transfer_record(transfer_id, |record, now| {
            record.state = FileTransferState::Cancelled;
            record.failure_reason = Some(reason.to_string());
            record.updated_at = now;
        })
        .await;
    }

    pub async fn retry_file_transfer_from_beginning(&self, transfer_id: &str) -> Result<String> {
        let record = self
            .file_transfer_records
            .read()
            .await
            .iter()
            .find(|record| record.transfer_id == transfer_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("transfer {transfer_id} not found"))?;

        if !matches!(
            record.state,
            FileTransferState::Failed | FileTransferState::Cancelled
        ) {
            anyhow::bail!("transfer {transfer_id} is not failed or cancelled");
        }
        if record.direction != FileTransferDirection::Outgoing {
            anyhow::bail!("only outgoing transfers can be retried from this machine");
        }
        let source_path = record
            .source_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("transfer {transfer_id} has no retry source path"))?;

        self.queue_file_from_path_with_previous(
            &record.peer_id,
            source_path,
            Some(record.transfer_id.clone()),
        )
        .await
    }

    pub async fn cancel_file_transfer_by_id(&self, transfer_id: &str, reason: &str) -> bool {
        let record = self
            .file_transfer_records
            .read()
            .await
            .iter()
            .find(|record| record.transfer_id == transfer_id)
            .cloned();
        let Some(record) = record else {
            return false;
        };
        if record.direction != FileTransferDirection::Outgoing
            || !matches!(
                record.state,
                FileTransferState::Queued | FileTransferState::Active
            )
        {
            return false;
        }
        self.cancel_outbound_file_transfer(&record.peer_id, transfer_id, reason)
            .await
    }

    pub async fn clear_completed_file_transfers(&self) -> usize {
        let mut records = self.file_transfer_records.write().await;
        let before = records.len();
        records.retain(|record| record.state != FileTransferState::Completed);
        before.saturating_sub(records.len())
    }

    pub(crate) async fn reject_outbound_file_transfer(
        &self,
        peer_id: &str,
        transfer_id: &str,
        reason: &str,
    ) -> bool {
        let removed = self
            .remove_outbound_file_transfer_state(peer_id, transfer_id)
            .await;
        self.remove_outgoing_bulk_payloads_for_transfer(peer_id, transfer_id)
            .await;
        self.mark_file_transfer_failed(transfer_id, reason).await;
        removed
    }

    pub(crate) async fn fail_outbound_file_transfers_for_peer(&self, peer_id: &str, reason: &str) {
        let removed = {
            let mut transfers = self.outbound_file_transfers.write().await;
            let transfer_ids = transfers
                .iter()
                .filter(|(_, transfer)| transfer.peer_id == peer_id)
                .map(|(transfer_id, _)| transfer_id.clone())
                .collect::<Vec<_>>();
            transfer_ids
                .into_iter()
                .filter_map(|transfer_id| {
                    transfers
                        .remove(&transfer_id)
                        .map(|transfer| (transfer_id, transfer.total_bytes))
                })
                .collect::<Vec<_>>()
        };

        if removed.is_empty() {
            return;
        }

        let transfer_ids = removed
            .iter()
            .map(|(transfer_id, _)| transfer_id.clone())
            .collect::<HashSet<_>>();
        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        if let Some(queue) = queue_map.get_mut(peer_id) {
            queue.retain(|payload| !outbound_payload_matches_any(payload, &transfer_ids));
            if queue.is_empty() {
                queue_map.remove(peer_id);
            }
        }
        drop(queue_map);

        for (transfer_id, total_bytes) in removed {
            self.mark_file_transfer_failed(&transfer_id, reason).await;
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "outgoing".to_string(),
                kind: "file_transfer_failed".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!("transfer_id={transfer_id} reason={reason}"),
                size_bytes: total_bytes,
            });
        }
    }

    async fn push_file_transfer_record(&self, record: FileTransferRecord) {
        let mut records = self.file_transfer_records.write().await;
        if let Some(existing) = records
            .iter_mut()
            .find(|existing| existing.transfer_id == record.transfer_id)
        {
            *existing = record;
        } else {
            records.push_back(record);
        }
        prune_file_transfer_records(&mut records);
    }

    async fn update_file_transfer_record(
        &self,
        transfer_id: &str,
        update: impl FnOnce(&mut FileTransferRecord, chrono::DateTime<Utc>),
    ) {
        let mut records = self.file_transfer_records.write().await;
        if let Some(record) = records
            .iter_mut()
            .find(|record| record.transfer_id == transfer_id)
        {
            update(record, Utc::now());
        }
    }

    async fn remove_outbound_file_transfer_state(&self, peer_id: &str, transfer_id: &str) -> bool {
        let mut transfers = self.outbound_file_transfers.write().await;
        if transfers
            .get(transfer_id)
            .is_some_and(|transfer| transfer.peer_id == peer_id)
        {
            transfers.remove(transfer_id);
            true
        } else {
            false
        }
    }

    async fn remove_outgoing_bulk_payloads_for_transfer(&self, peer_id: &str, transfer_id: &str) {
        let mut queue_map = self.transport.outgoing_bulk_payloads.write().await;
        if let Some(queue) = queue_map.get_mut(peer_id) {
            queue.retain(|payload| !outbound_payload_matches(payload, transfer_id));
            if queue.is_empty() {
                queue_map.remove(peer_id);
            }
        }
    }
}

fn prune_file_transfer_records(records: &mut VecDeque<FileTransferRecord>) {
    while records.len() > MAX_FILE_TRANSFER_RECORDS {
        let remove_index = records
            .iter()
            .position(|record| record.state.is_terminal())
            .unwrap_or(0);
        records.remove(remove_index);
    }
}

fn outbound_payload_matches_any(payload: &OutboundPayload, transfer_ids: &HashSet<String>) -> bool {
    match payload {
        OutboundPayload::FileStart { transfer_id, .. }
        | OutboundPayload::FileChunk { transfer_id, .. }
        | OutboundPayload::FileTransferCursor { transfer_id }
        | OutboundPayload::FileEnd { transfer_id, .. } => transfer_ids.contains(transfer_id),
        _ => false,
    }
}

fn outbound_payload_matches(payload: &OutboundPayload, expected_transfer_id: &str) -> bool {
    match payload {
        OutboundPayload::FileStart { transfer_id, .. }
        | OutboundPayload::FileChunk { transfer_id, .. }
        | OutboundPayload::FileTransferCursor { transfer_id }
        | OutboundPayload::FileEnd { transfer_id, .. } => transfer_id == expected_transfer_id,
        _ => false,
    }
}
