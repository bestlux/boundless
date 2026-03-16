use super::*;

impl AppState {
    pub async fn request_peer_reconnect(&self, peer_id: &str) -> u64 {
        let mut generations = self.reconnect_generation_by_peer.write().await;
        let entry = generations.entry(peer_id.to_string()).or_insert(0);
        *entry += 1;
        let generation = *entry;
        drop(generations);
        self.notify_peer_reconcile_wake("peer_reconnect_requested");
        generation
    }

    pub async fn peer_reconnect_generation(&self, peer_id: &str) -> u64 {
        *self
            .reconnect_generation_by_peer
            .read()
            .await
            .get(peer_id)
            .unwrap_or(&0)
    }

    pub async fn request_peer_reconnect_and_reset(&self, peer_id: &str) -> Result<(u64, usize)> {
        let generation = self.request_peer_reconnect(peer_id).await;
        let aborted_sessions = self.abort_transport_sessions_for_peer(peer_id).await;
        self.set_peer_connected(peer_id, false).await?;
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "peer_reconnect_requested".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("generation={generation} aborted_sessions={aborted_sessions}"),
            size_bytes: 0,
        });
        Ok((generation, aborted_sessions))
    }

    pub async fn request_all_peers_reconnect_and_reset(&self) -> Result<(usize, usize)> {
        let peer_ids = self
            .list_peers()
            .await
            .into_iter()
            .map(|peer| peer.peer_id)
            .collect::<Vec<_>>();

        for peer_id in &peer_ids {
            self.request_peer_reconnect(peer_id).await;
        }

        let aborted_sessions = self.abort_transport_sessions_for_peers(&peer_ids).await;
        let disconnected_peers = self.mark_all_peers_disconnected().await?;

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "peers_reconnect_requested".to_string(),
            peer_id: "all".to_string(),
            detail: format!(
                "requested_peers={} disconnected_peers={} aborted_sessions={aborted_sessions}",
                peer_ids.len(),
                disconnected_peers
            ),
            size_bytes: 0,
        });
        self.notify_peer_reconcile_wake("all_peers_reconnect_requested");

        Ok((disconnected_peers, aborted_sessions))
    }

    fn next_transport_session_id(&self) -> u64 {
        self.next_transport_session_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn register_pending_transport_session(&self, abort_handle: AbortHandle) -> u64 {
        let session_id = self.next_transport_session_id();
        self.pending_transport_session_abort_handles
            .write()
            .await
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: AbortHandle,
    ) -> u64 {
        let session_id = self.next_transport_session_id();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn bind_pending_transport_session_to_peer(
        &self,
        session_id: u64,
        peer_id: &str,
    ) -> bool {
        let abort_handle = self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id);
        let Some(abort_handle) = abort_handle else {
            return false;
        };

        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        true
    }

    pub async fn clear_transport_session_registration(&self, session_id: u64) {
        if self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id)
            .is_some()
        {
            return;
        }

        let mut by_peer = self.transport_session_abort_handles_by_peer.write().await;
        let mut empty_peers = Vec::<String>::new();
        for (peer_id, sessions) in by_peer.iter_mut() {
            if sessions.remove(&session_id).is_some() && sessions.is_empty() {
                empty_peers.push(peer_id.clone());
            }
        }
        for peer_id in empty_peers {
            by_peer.remove(&peer_id);
        }
    }

    pub async fn abort_transport_sessions_for_peer(&self, peer_id: &str) -> usize {
        let sessions = self
            .transport_session_abort_handles_by_peer
            .write()
            .await
            .remove(peer_id)
            .unwrap_or_default();
        let aborted = sessions.len();
        for handle in sessions.into_values() {
            handle.abort();
        }
        aborted
    }

    pub async fn abort_transport_sessions_for_peers(&self, peer_ids: &[String]) -> usize {
        let mut total = 0usize;
        for peer_id in peer_ids {
            total += self.abort_transport_sessions_for_peer(peer_id).await;
        }
        total
    }

    pub async fn mark_all_peers_disconnected(&self) -> Result<usize> {
        let mut config = self.config.write().await;
        let mut disconnected_peer_ids = Vec::<String>::new();

        for peer in &mut config.peers {
            if !peer.connected {
                continue;
            }
            peer.connected = false;
            peer.last_seen = Utc::now();
            disconnected_peer_ids.push(peer.peer_id.clone());
        }

        if !disconnected_peer_ids.is_empty() {
            save_config_at(&self.config_path, &config)?;
        }
        drop(config);

        if !disconnected_peer_ids.is_empty() {
            let mut router = self.input_router.write().await;
            let mut released_owner = false;
            for peer_id in &disconnected_peer_ids {
                released_owner = router.release_owner(peer_id) || released_owner;
                router.clear_peer_state(peer_id);
            }
            drop(router);
            if released_owner {
                self.note_input_owner_transition().await;
            }

            for peer_id in &disconnected_peer_ids {
                self.clear_pending_inject_input_frames_for_peer(peer_id)
                    .await;
            }

            let mut capture_target = self.input_capture_target_peer_id.write().await;
            if capture_target
                .as_deref()
                .is_some_and(|peer_id| disconnected_peer_ids.iter().any(|id| id == peer_id))
            {
                *capture_target = None;
            }
            drop(capture_target);
            self.notify_input_capture_wake("all_peers_disconnected");
            self.notify_peer_reconcile_wake("all_peers_disconnected");
        }

        Ok(disconnected_peer_ids.len())
    }
}
