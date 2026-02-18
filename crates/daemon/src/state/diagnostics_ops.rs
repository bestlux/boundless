use super::*;

impl AppState {
    pub async fn safe_reset(&self, network_only: bool, all: bool) -> Result<()> {
        let mut config = self.config.write().await;

        if all {
            let machine_id = config.machine_id.clone();
            let device_name = config.device_name.clone();
            *config = RuntimeConfig::default();
            config.machine_id = machine_id;
            config.device_name = device_name;
        } else if network_only {
            config.peers.clear();
        }

        self.outgoing_payloads.write().await.clear();
        self.transport_events.write().await.clear();
        *self.clipboard_sync.write().await = ClipboardSyncState::default();
        self.discovered_endpoints.write().await.clear();
        *self.input_router.write().await =
            InputRouter::new(config.features.get("share_input").copied().unwrap_or(true));
        self.input_sequence_by_peer.write().await.clear();
        self.pending_inject_input_frames.write().await.clear();
        *self.input_capture_target_peer_id.write().await = None;
        *self.input_lock_active.write().await = false;
        self.reconnect_generation_by_peer.write().await.clear();
        self.pairing_codes.write().await.clear();
        self.pending_nearby_pairing_requests.write().await.clear();
        self.nearby_pairing_decisions.write().await.clear();
        self.pending_transport_session_abort_handles
            .write()
            .await
            .clear();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .clear();

        save_config_at(&self.config_path, &config)
    }

    pub async fn diagnostics_dump(&self, output_path: Option<String>) -> Result<String> {
        let target = output_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("Boundless")
                    .join("diagnostics")
            });

        tokio::fs::create_dir_all(&target).await?;

        let file_path = target.join(format!("dump-{}.txt", Utc::now().format("%Y%m%d-%H%M%S")));

        let snapshot = self.snapshot().await;
        let trust_count = self
            .trusted_records()
            .await
            .map(|items| items.len())
            .unwrap_or(0);
        let event_count = self.transport_events.read().await.len();
        let input_owner = self
            .input_owner()
            .await
            .unwrap_or_else(|| "none".to_string());
        let input_capture_target = self
            .input_capture_target()
            .await
            .unwrap_or_else(|| "none".to_string());

        let report = format!(
            "Boundless Diagnostics\nMachine: {}\nFingerprint: {}\nPeers: {}\nTrusted CAs: {}\nTransport Events: {}\nInput Owner: {}\nInput Capture Target: {}\nAPI: {}\nTransport Port: {}\nProtocol: {}\n",
            snapshot.machine_id,
            self.fingerprint(),
            snapshot.peers.len(),
            trust_count,
            event_count,
            input_owner,
            input_capture_target,
            snapshot.api_bind,
            snapshot.network_port,
            snapshot.protocol_version
        );

        tokio::fs::write(&file_path, report).await?;
        Ok(file_path.display().to_string())
    }

    pub(crate) async fn connected_peer_ids(&self) -> Vec<String> {
        self.config
            .read()
            .await
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .map(|peer| peer.peer_id.clone())
            .collect()
    }

    pub(crate) async fn validated_clipboard_payload_hash(
        &self,
        payload: &ClipboardPayload,
    ) -> Result<Option<String>> {
        if let ClipboardPayload::Image(image_bmp) = payload {
            validate_bmp_payload(image_bmp).context("invalid clipboard BMP payload")?;
        }

        let policy = ClipboardPolicy {
            enabled: self
                .config
                .read()
                .await
                .features
                .get("share_clipboard")
                .copied()
                .unwrap_or(true),
            ..ClipboardPolicy::default()
        };

        match validate_payload(policy, payload) {
            Ok(()) => Ok(Some(payload_hash_hex(payload))),
            Err(ClipboardPolicyError::Disabled) => Ok(None),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }
}
