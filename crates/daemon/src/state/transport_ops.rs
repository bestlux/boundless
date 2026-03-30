use super::*;

impl AppState {
    pub async fn request_peer_reconnect(&self, peer_id: &str) -> u64 {
        let generation = self.transport.request_peer_reconnect(peer_id).await;
        self.notify_peer_reconcile_wake("peer_reconnect_requested");
        generation
    }

    pub async fn peer_reconnect_generation(&self, peer_id: &str) -> u64 {
        self.transport.peer_reconnect_generation(peer_id).await
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

    pub async fn register_pending_transport_session(
        &self,
        abort_handle: tokio::task::AbortHandle,
    ) -> u64 {
        self.transport
            .register_pending_transport_session(abort_handle)
            .await
    }

    pub async fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: tokio::task::AbortHandle,
    ) -> u64 {
        self.transport
            .register_transport_session_for_peer(peer_id, abort_handle)
            .await
    }

    pub async fn bind_pending_transport_session_to_peer(
        &self,
        session_id: u64,
        peer_id: &str,
    ) -> bool {
        self.transport
            .bind_pending_transport_session_to_peer(session_id, peer_id)
            .await
    }

    pub async fn clear_transport_session_registration(&self, session_id: u64) {
        self.transport
            .clear_transport_session_registration(session_id)
            .await;
    }

    pub async fn abort_transport_sessions_for_peer(&self, peer_id: &str) -> usize {
        self.transport
            .abort_transport_sessions_for_peer(peer_id)
            .await
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
            let mut router = self.input.router.write().await;
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

            let mut capture_target = self.input.capture_target_peer_id.write().await;
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
