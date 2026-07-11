use super::*;

pub(crate) struct TransportSessionEgressGuard {
    _transition: tokio::sync::OwnedMutexGuard<()>,
}

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

    pub fn register_pending_transport_session(
        &self,
        abort_handle: tokio::task::AbortHandle,
    ) -> u64 {
        self.transport
            .register_pending_transport_session(abort_handle)
    }

    pub fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: tokio::task::AbortHandle,
    ) -> u64 {
        self.transport
            .register_transport_session_for_peer(peer_id, abort_handle)
    }

    pub fn bind_pending_transport_session_to_peer(&self, session_id: u64, peer_id: &str) -> bool {
        self.transport
            .bind_pending_transport_session_to_peer(session_id, peer_id)
    }

    pub fn allocate_transport_session_id(&self) -> u64 {
        self.transport.allocate_transport_session_id()
    }

    pub async fn claim_transport_session(
        &self,
        peer_id: &str,
        session_id: u64,
        preferred: bool,
        cancellation: Arc<RuntimeWakeSignal>,
    ) -> TransportSessionClaim {
        let _transition = self.transport_session_transition.lock().await;
        self.transport
            .claim_transport_session(peer_id, session_id, preferred, cancellation)
    }

    /// Serializes queue drain/write/flush with transport ownership changes.
    ///
    /// The caller must hold this guard until every drained payload has either
    /// been flushed or returned to the shared queue. That prevents a preferred
    /// replacement from draining N+1 while the superseded lane still owns N.
    pub(crate) async fn acquire_transport_session_egress(
        &self,
        peer_id: &str,
        session_id: u64,
    ) -> Option<TransportSessionEgressGuard> {
        let transition = self.transport_session_transition.clone().lock_owned().await;
        if !self
            .transport
            .is_active_transport_session(peer_id, session_id)
        {
            return None;
        }
        Some(TransportSessionEgressGuard {
            _transition: transition,
        })
    }

    pub fn clear_active_transport_session(&self, peer_id: &str, session_id: u64) -> bool {
        self.transport
            .clear_active_transport_session(peer_id, session_id)
    }

    pub async fn close_active_transport_session(&self, peer_id: &str, session_id: u64) -> bool {
        let _transition = self.transport_session_transition.lock().await;
        if !self
            .transport
            .clear_active_transport_session(peer_id, session_id)
        {
            return false;
        }
        let _ = self.set_peer_connected(peer_id, false).await;
        true
    }

    pub async fn mark_peer_disconnected_if_no_active_transport_session(
        &self,
        peer_id: &str,
    ) -> Result<bool> {
        let _transition = self.transport_session_transition.lock().await;
        if self.transport.has_active_transport_session(peer_id) {
            return Ok(false);
        }
        self.set_peer_connected(peer_id, false).await?;
        Ok(true)
    }

    pub fn has_active_transport_session(&self, peer_id: &str) -> bool {
        self.transport.has_active_transport_session(peer_id)
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

    pub fn begin_transport_session_shutdown(&self) {
        self.transport.begin_transport_session_shutdown();
    }

    pub async fn abort_all_transport_sessions_for_shutdown(&self) -> usize {
        self.transport.abort_all_transport_sessions().await
    }

    pub async fn mark_all_peers_disconnected(&self) -> Result<usize> {
        let disconnected_peer_ids = self
            .mutate_config_and_save(|config| {
                let mut disconnected_peer_ids = Vec::<String>::new();

                for peer in &mut config.peers {
                    if !peer.connected {
                        continue;
                    }
                    peer.connected = false;
                    peer.last_seen = Utc::now();
                    disconnected_peer_ids.push(peer.peer_id.clone());
                }

                let should_save = !disconnected_peer_ids.is_empty();
                Ok((disconnected_peer_ids, should_save))
            })
            .await?;

        if !disconnected_peer_ids.is_empty() {
            let mut authorization = self.input.control.authorization.write().await;
            let mut released_owner = false;
            for peer_id in &disconnected_peer_ids {
                released_owner = authorization.release_owner(peer_id) || released_owner;
                authorization.clear_peer_state(peer_id);
            }
            drop(authorization);
            if released_owner {
                self.notify_input_owner_transition();
            }

            for peer_id in &disconnected_peer_ids {
                self.clear_pending_inject_input_frames_for_peer(peer_id)
                    .await;
                self.clear_remote_anti_idle_peer(peer_id).await;
            }

            let mut capture_target = self.input.control.capture_target_peer_id.write().await;
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
