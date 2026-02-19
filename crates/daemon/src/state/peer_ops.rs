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
        let mut config = self.config.write().await;
        let before = config.peers.len();
        config.peers.retain(|p| p.peer_id != peer_id);
        let removed = before != config.peers.len();
        if removed {
            save_config_at(&self.config_path, &config)?;
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
            self.reconnect_generation_by_peer
                .write()
                .await
                .remove(peer_id);
            self.abort_transport_sessions_for_peer(peer_id).await;
            let mut capture_target = self.input_capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
        }
        Ok(removed)
    }

    pub async fn set_peer_connected(&self, peer_id: &str, connected: bool) -> Result<()> {
        let mut config = self.config.write().await;
        let mut changed = false;

        if let Some(peer) = config.peers.iter_mut().find(|p| p.peer_id == peer_id) {
            peer.connected = connected;
            peer.last_seen = Utc::now();
            changed = true;
        }

        if changed {
            save_config_at(&self.config_path, &config)?;
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
