use super::*;
use app_services::desktop::{LayoutPeerToken, canonicalize_layout_matrix_spec};

impl AppState {
    pub async fn update_bind(&self, bind: String) -> Result<()> {
        validate_bind_address(&bind)?;

        let mut config = self.config.write().await;
        config.api_bind = bind;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_api_transport(&self, api_transport: ApiTransport) -> Result<()> {
        let mut config = self.config.write().await;
        config.api_transport = api_transport;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_api_pipe_name(&self, pipe_name: String) -> Result<()> {
        validate_pipe_name(&pipe_name)?;

        let mut config = self.config.write().await;
        config.api_pipe_name = pipe_name;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_network_port(&self, port: u16) -> Result<()> {
        let mut config = self.config.write().await;
        config.network_port = port;
        save_config_at(&self.config_path, &config)
    }

    pub async fn file_transfer_config(&self) -> FileTransferConfig {
        self.config.read().await.file_transfer.clone()
    }

    pub async fn file_transfer_max_bytes(&self) -> u64 {
        self.config.read().await.file_transfer.max_file_bytes
    }

    pub async fn file_transfer_auto_accept_trusted_peers(&self) -> bool {
        self.config
            .read()
            .await
            .file_transfer
            .auto_accept_trusted_peers
    }

    pub async fn update_file_transfer_config(
        &self,
        file_transfer: FileTransferConfig,
    ) -> Result<()> {
        if file_transfer.receive_dir.trim().is_empty() {
            anyhow::bail!("file transfer receive directory must not be empty");
        }
        if file_transfer.max_file_bytes == 0 {
            anyhow::bail!("file transfer max_file_bytes must be greater than zero");
        }

        let receive_dir = PathBuf::from(&file_transfer.receive_dir);
        tokio::fs::create_dir_all(&receive_dir).await?;

        let mut config = self.config.write().await;
        config.file_transfer = file_transfer;
        save_config_at(&self.config_path, &config)
    }

    pub async fn input_handoff_config(&self) -> InputHandoffConfig {
        self.config.read().await.input_handoff.clone()
    }

    pub async fn update_input_handoff_config(
        &self,
        input_handoff: InputHandoffConfig,
    ) -> Result<()> {
        if input_handoff.corner_block_px > 256 {
            anyhow::bail!("input_handoff.corner_block_px must be <= 256");
        }

        let mut config = self.config.write().await;
        config.input_handoff = input_handoff;
        save_config_at(&self.config_path, &config)?;
        drop(config);
        self.notify_input_capture_wake("input_handoff_config_changed");
        Ok(())
    }

    pub async fn set_discovered_endpoint(
        &self,
        machine_id: &str,
        display_name: &str,
        endpoint: SocketAddr,
    ) -> Option<DiscoveredPeerEndpoint> {
        let previous = self.discovery.endpoints.write().await.insert(
            machine_id.to_string(),
            DiscoveredPeerEndpoint {
                display_name: display_name.to_string(),
                endpoint,
            },
        );
        self.notify_peer_reconcile_wake("discovered_endpoint");
        previous
    }

    pub async fn clear_discovered_endpoint(
        &self,
        machine_id: &str,
    ) -> Option<DiscoveredPeerEndpoint> {
        let removed = self.discovery.endpoints.write().await.remove(machine_id);
        if removed.is_some() {
            self.notify_peer_reconcile_wake("discovered_endpoint_removed");
        }
        removed
    }

    pub async fn discovered_endpoints(&self) -> Vec<(String, DiscoveredPeerEndpoint)> {
        let mut entries = self
            .discovery
            .endpoints
            .read()
            .await
            .iter()
            .map(|(machine_id, record)| (machine_id.clone(), record.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    pub async fn discovered_endpoint(&self, machine_id: &str) -> Option<SocketAddr> {
        self.discovery
            .endpoints
            .read()
            .await
            .get(machine_id)
            .map(|record| record.endpoint)
    }

    pub async fn set_mdns_active(&self, active: bool) {
        *self.discovery.mdns_active.write().await = active;
    }

    pub async fn mdns_active(&self) -> bool {
        *self.discovery.mdns_active.read().await
    }

    pub async fn layout(&self) -> String {
        self.config.read().await.layout_matrix.clone()
    }

    pub async fn set_layout(&self, matrix: String) -> Result<()> {
        let mut config = self.config.write().await;
        let peers = config
            .peers
            .iter()
            .map(|peer| LayoutPeerToken {
                peer_id: peer.peer_id.clone(),
                display_name: peer.display_name.clone(),
            })
            .collect::<Vec<_>>();
        let canonical_matrix = canonicalize_layout_matrix_spec(
            &matrix,
            &config.machine_id,
            Some(config.device_name.as_str()),
            &peers,
        )?;
        config.layout_matrix = canonical_matrix;
        save_config_at(&self.config_path, &config)?;
        drop(config);
        self.invalidate_cached_layout_matrix().await;
        self.notify_input_capture_wake("layout_changed");
        Ok(())
    }

    pub async fn edge_switch_policy(&self) -> (EasyMouseMode, bool, InputHandoffConfig) {
        let config = self.config.read().await;
        let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        let easy_mouse_enabled = config.features.get("easy_mouse").copied().unwrap_or(true);
        let wrap_mouse = config.features.get("wrap_mouse").copied().unwrap_or(true);
        let input_handoff = config.input_handoff.clone();

        let mode = if share_input_enabled && easy_mouse_enabled {
            EasyMouseMode::Enable
        } else {
            EasyMouseMode::Disable
        };

        (mode, wrap_mouse, input_handoff)
    }

    pub async fn capture_handoff_target_for_direction(
        &self,
        current_target: Option<&str>,
        direction: SwitchDirection,
    ) -> Option<CaptureHandoffTarget> {
        let config = self.config.read().await;
        let matrix = self
            .cached_layout_matrix_for_spec(&config.layout_matrix)
            .await;
        resolve_capture_handoff_target_with_fallback_from_matrix(
            &config,
            current_target,
            direction,
            matrix.as_ref(),
        )
    }

    pub async fn apply_switch_all_capture_target(&self) -> Option<String> {
        let next = self.next_switch_all_capture_target().await;
        match next.as_deref() {
            Some(peer_id) => {
                let _ = self.set_input_capture_target(Some(peer_id)).await;
            }
            None => {
                self.clear_input_capture_target().await;
            }
        }
        next
    }

    pub async fn next_switch_all_capture_target(&self) -> Option<String> {
        let order = {
            let config = self.config.read().await;
            let matrix = self
                .cached_layout_matrix_for_spec(&config.layout_matrix)
                .await;
            resolve_switch_all_target_order_from_matrix(&config, matrix.as_ref())
        };
        let current_target = self
            .input
            .control
            .capture_target_peer_id
            .read()
            .await
            .clone();
        if order.is_empty() {
            return None;
        }

        if let Some(current) = current_target
            && let Some(index) = order.iter().position(|peer_id| peer_id == &current)
        {
            return Some(order[(index + 1) % order.len()].clone());
        }

        Some(order[0].clone())
    }

    pub async fn set_feature(&self, name: String, enabled: bool) -> Result<()> {
        let mut config = self.config.write().await;
        config.features.insert(name.clone(), enabled);
        save_config_at(&self.config_path, &config)?;

        if name == "share_input" {
            self.input.control.router.write().await.set_enabled(enabled);
            self.notify_input_inject_wake("share_input_toggled");
            self.notify_input_capture_wake("share_input_toggled");
        } else if name == "share_clipboard" && !enabled {
            self.clipboard.clear().await;
        } else if name == "easy_mouse" || name == "wrap_mouse" {
            self.notify_input_capture_wake("input_policy_toggled");
        }

        Ok(())
    }

    pub async fn feature_map(&self) -> std::collections::BTreeMap<String, bool> {
        self.config.read().await.features.clone()
    }

    #[cfg(windows)]
    pub async fn hotkey_map(&self) -> std::collections::BTreeMap<String, String> {
        self.config.read().await.hotkeys.clone()
    }

    pub async fn set_hotkey(&self, action: String, combo: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.hotkeys.insert(action, combo);
        save_config_at(&self.config_path, &config)
    }
}
