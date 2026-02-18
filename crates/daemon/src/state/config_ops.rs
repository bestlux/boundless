use super::*;

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

    pub async fn set_discovered_endpoint(
        &self,
        machine_id: &str,
        display_name: &str,
        endpoint: SocketAddr,
    ) -> Option<DiscoveredPeerEndpoint> {
        self.discovered_endpoints.write().await.insert(
            machine_id.to_string(),
            DiscoveredPeerEndpoint {
                display_name: display_name.to_string(),
                endpoint,
            },
        )
    }

    pub async fn clear_discovered_endpoint(
        &self,
        machine_id: &str,
    ) -> Option<DiscoveredPeerEndpoint> {
        self.discovered_endpoints.write().await.remove(machine_id)
    }

    pub async fn discovered_endpoints(&self) -> Vec<(String, DiscoveredPeerEndpoint)> {
        let mut entries = self
            .discovered_endpoints
            .read()
            .await
            .iter()
            .map(|(machine_id, record)| (machine_id.clone(), record.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    pub async fn discovered_endpoint(&self, machine_id: &str) -> Option<SocketAddr> {
        self.discovered_endpoints
            .read()
            .await
            .get(machine_id)
            .map(|record| record.endpoint)
    }

    pub async fn set_mdns_active(&self, active: bool) {
        *self.mdns_active.write().await = active;
    }

    pub async fn mdns_active(&self) -> bool {
        *self.mdns_active.read().await
    }

    pub async fn layout(&self) -> String {
        self.config.read().await.layout_matrix.clone()
    }

    pub async fn set_layout(&self, matrix: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.layout_matrix = matrix;
        save_config_at(&self.config_path, &config)
    }

    pub async fn edge_switch_policy(&self) -> (EasyMouseMode, bool) {
        let config = self.config.read().await;
        let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        let easy_mouse_enabled = config.features.get("easy_mouse").copied().unwrap_or(true);
        let wrap_mouse = config.features.get("wrap_mouse").copied().unwrap_or(true);

        let mode = if share_input_enabled && easy_mouse_enabled {
            EasyMouseMode::Enable
        } else {
            EasyMouseMode::Disable
        };

        (mode, wrap_mouse)
    }

    pub async fn capture_handoff_target_for_direction(
        &self,
        current_target: Option<&str>,
        direction: SwitchDirection,
    ) -> Option<CaptureHandoffTarget> {
        let config = self.config.read().await;
        resolve_capture_handoff_target_with_fallback(&config, current_target, direction)
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
            resolve_switch_all_target_order(&config)
        };
        let current_target = self.input_capture_target_peer_id.read().await.clone();
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
            self.input_router.write().await.set_enabled(enabled);
        } else if name == "share_clipboard" && !enabled {
            *self.clipboard_sync.write().await = ClipboardSyncState::default();
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
