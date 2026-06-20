use super::*;

impl AppState {
    pub async fn join_peer(
        &self,
        code: String,
        host: String,
        alias: Option<String>,
    ) -> Result<String> {
        let now = Utc::now();
        self.ensure_trust_rotation_not_pending()?;
        self.consume_pairing_code(&code).await?;

        let peer_id = self
            .mutate_config_and_save(|config| {
                self.ensure_trust_rotation_not_pending()?;
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
                Ok((peer_id, true))
            })
            .await?;
        self.notify_peer_reconcile_wake("peer_joined");
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
        let removed = self
            .mutate_config_and_save(|config| {
                let before = config.peers.len();
                config.peers.retain(|p| p.peer_id != peer_id);
                let removed = before != config.peers.len();
                Ok((removed, removed))
            })
            .await?;
        if removed {
            let mut router = self.input.control.router.write().await;
            let released_owner = router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            drop(router);
            if released_owner {
                self.note_input_owner_transition().await;
            }
            self.input
                .control
                .sequence_by_peer
                .write()
                .await
                .remove(peer_id);
            self.discovery.endpoints.write().await.remove(peer_id);
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
            self.clear_obsolete_inflight_clipboard_replays_for_peer(peer_id)
                .await;
            self.clear_remote_anti_idle_peer(peer_id).await;
            self.transport
                .reconnect_generation_by_peer
                .write()
                .await
                .remove(peer_id);
            self.abort_transport_sessions_for_peer(peer_id).await;
            let mut capture_target = self.input.control.capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
            let _ = remove_trust_record(&self.security_paths, peer_id)?;
            self.notify_input_capture_wake("peer_removed");
            self.notify_peer_reconcile_wake("peer_removed");
        }
        Ok(removed)
    }

    pub async fn rotate_trust(&self) -> Result<String> {
        self.trust_rotation_pending_restart
            .store(true, Ordering::Release);
        let (machine_id, device_name) = {
            let config = self.config.read().await;
            (config.machine_id.clone(), config.device_name.clone())
        };
        let advertised_host = std::env::var("BOUNDLESS_ADVERTISE_HOST").ok();
        let identity = rotate_device_identity(
            &self.security_paths,
            &machine_id,
            &device_name,
            advertised_host.as_deref(),
        )?;
        upsert_trust_record(
            &self.security_paths,
            TrustRecord {
                machine_id: machine_id.clone(),
                ca_cert_pem: identity.ca_cert_pem,
                added_at: Utc::now(),
            },
        )?;
        self.mutate_config_and_save(|config| {
            config.peers.clear();
            config.layout_matrix = "self".to_string();
            config.updated_at = Utc::now();
            Ok(((), true))
        })
        .await?;

        let aborted_sessions = self.transport.clear().await;
        self.clipboard.clear().await;
        self.discovery.clear().await;
        self.pairing.clear().await;
        self.input
            .reset(
                self.config
                    .read()
                    .await
                    .features
                    .get("share_input")
                    .copied()
                    .unwrap_or(true),
            )
            .await;
        self.invalidate_cached_layout_matrix().await;
        self.notify_input_capture_wake("trust_rotated");
        self.notify_peer_reconcile_wake("trust_rotated");

        Ok(format!(
            "trust_rotated=true peers_cleared=true aborted_sessions={aborted_sessions} restart_required=true"
        ))
    }

    pub async fn set_peer_connected(&self, peer_id: &str, connected: bool) -> Result<()> {
        let (peer_found, transitioned_to_connected) = self
            .mutate_config_and_save(|config| {
                let mut peer_found = false;
                let mut transitioned_to_connected = false;
                let mut should_save = false;

                if let Some(peer_index) = config.peers.iter().position(|p| p.peer_id == peer_id) {
                    let previous_connected = config.peers[peer_index].connected;
                    transitioned_to_connected = !previous_connected && connected;
                    peer_found = true;

                    if previous_connected || connected {
                        config.peers[peer_index].connected = connected;
                        config.peers[peer_index].last_seen = Utc::now();
                        should_save = true;
                    }
                }

                Ok(((peer_found, transitioned_to_connected), should_save))
            })
            .await?;

        if !peer_found {
            return Ok(());
        }

        if !connected {
            let mut router = self.input.control.router.write().await;
            let released_owner = router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            drop(router);
            if released_owner {
                self.note_input_owner_transition().await;
            }
            let mut capture_target = self.input.control.capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
            self.clear_pending_clipboard_replay_for_peer(peer_id).await;
            self.clear_obsolete_inflight_clipboard_replays_for_peer(peer_id)
                .await;
            self.clear_remote_anti_idle_peer(peer_id).await;
            self.notify_input_capture_wake("peer_disconnected");
        } else if transitioned_to_connected
            && !self
                .has_current_clipboard_replay_delivery_pending_for_peer(peer_id)
                .await
            && self
                .schedule_pending_clipboard_replay_for_peer(peer_id)
                .await
        {
            self.notify_outgoing_flush_signal();
        }

        self.notify_peer_reconcile_wake(if connected {
            "peer_connected"
        } else {
            "peer_disconnected"
        });

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
