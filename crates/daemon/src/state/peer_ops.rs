use super::*;

impl AppState {
    pub async fn join_peer(
        &self,
        code: String,
        host: String,
        alias: Option<String>,
    ) -> Result<String> {
        let now = Utc::now();
        self.consume_pairing_code(&code).await?;

        let mut config = self.config.write().await;
        let normalized_address = normalize_peer_address(&host, config.network_port)?;
        let peer_id = uuid::Uuid::new_v4().to_string();

        let peer = PeerConfig {
            peer_id: peer_id.clone(),
            display_name: alias.unwrap_or_else(|| format!("peer-{}", &peer_id[..8])),
            address: normalized_address,
            connected: false,
            last_seen: now,
        };

        config.peers.push(peer);
        save_config_at(&self.config_path, &config)?;
        Ok(peer_id)
    }

    pub async fn list_peers(&self) -> Vec<PeerConfig> {
        self.config.read().await.peers.clone()
    }

    pub async fn get_peer(&self, peer_id: &str) -> Option<PeerConfig> {
        self.config
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.peer_id == peer_id)
            .cloned()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> Result<bool> {
        let removed = {
            let mut config = self.config.write().await;
            let before = config.peers.len();
            config.peers.retain(|p| p.peer_id != peer_id);
            let removed = before != config.peers.len();
            if removed {
                save_config_at(&self.config_path, &config)?;
            }
            removed
        };
        if removed {
            let mut router = self.input_router.write().await;
            let released_owner = router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            drop(router);
            if released_owner {
                self.note_input_owner_transition().await;
            }
            self.input_sequence_by_peer.write().await.remove(peer_id);
            self.discovered_endpoints.write().await.remove(peer_id);
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
            self.reconnect_generation_by_peer
                .write()
                .await
                .remove(peer_id);
            self.abort_transport_sessions_for_peer(peer_id).await;
            let mut capture_target = self.input_capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
            let _ = remove_trust_record(&self.security_paths, peer_id)?;
        }
        Ok(removed)
    }

    pub async fn set_peer_connected(&self, peer_id: &str, connected: bool) -> Result<()> {
        let (peer_found, transitioned_to_connected) = {
            let mut config = self.config.write().await;
            let mut peer_found = false;
            let mut transitioned_to_connected = false;

            if let Some(peer_index) = config.peers.iter().position(|p| p.peer_id == peer_id) {
                let previous_connected = config.peers[peer_index].connected;
                let previous_last_seen = config.peers[peer_index].last_seen;
                transitioned_to_connected = !previous_connected && connected;
                config.peers[peer_index].connected = connected;
                config.peers[peer_index].last_seen = Utc::now();
                peer_found = true;

                if let Err(error) = save_config_at(&self.config_path, &config) {
                    config.peers[peer_index].connected = previous_connected;
                    config.peers[peer_index].last_seen = previous_last_seen;
                    return Err(error);
                }
            }

            (peer_found, transitioned_to_connected)
        };

        if !peer_found {
            return Ok(());
        }

        if !connected {
            let mut router = self.input_router.write().await;
            let released_owner = router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            drop(router);
            if released_owner {
                self.note_input_owner_transition().await;
            }
            let mut capture_target = self.input_capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
        } else if transitioned_to_connected
            && !self
                .has_current_clipboard_replay_queued_for_peer(peer_id)
                .await
            && self
                .schedule_pending_clipboard_replay_for_peer(peer_id)
                .await
        {
            self.notify_outgoing_flush_signal();
        }

        Ok(())
    }

    pub async fn touch_peer(&self, peer_id: &str) -> Result<()> {
        let mut config = self.config.write().await;
        if let Some(peer) = config.peers.iter_mut().find(|p| p.peer_id == peer_id) {
            peer.last_seen = Utc::now();
        }
        Ok(())
    }
}
