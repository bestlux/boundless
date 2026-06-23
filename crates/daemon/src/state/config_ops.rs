use std::sync::OnceLock;

use super::*;
use app_services::desktop::{
    CANONICAL_LOCAL_LAYOUT_TOKEN, LayoutPeerToken, canonicalize_layout_matrix_spec,
    parse_layout_matrix,
};
use tokio::sync::Mutex;

static CONFIG_SAVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn config_save_lock() -> &'static Mutex<()> {
    CONFIG_SAVE_LOCK.get_or_init(|| Mutex::new(()))
}

fn preserve_in_memory_peer_touches(
    base: &RuntimeConfig,
    current: &RuntimeConfig,
    candidate: &mut RuntimeConfig,
) {
    for current_peer in &current.peers {
        let Some(base_peer) = base
            .peers
            .iter()
            .find(|peer| peer.peer_id == current_peer.peer_id)
        else {
            continue;
        };

        if current_peer.display_name != base_peer.display_name
            || current_peer.address != base_peer.address
            || current_peer.connected != base_peer.connected
            || current_peer.last_seen <= base_peer.last_seen
        {
            continue;
        }

        if let Some(candidate_peer) = candidate
            .peers
            .iter_mut()
            .find(|peer| peer.peer_id == current_peer.peer_id)
            && current_peer.last_seen > candidate_peer.last_seen
        {
            candidate_peer.last_seen = current_peer.last_seen;
        }
    }
}

impl AppState {
    pub(super) async fn mutate_config_and_save<F, T>(&self, mutate: F) -> Result<T>
    where
        F: FnOnce(&mut RuntimeConfig) -> Result<(T, bool)>,
        T: Send,
    {
        let _save_guard = config_save_lock().lock().await;

        let base = self.config.read().await.clone();
        let mut candidate = base.clone();
        let (result, should_save) = mutate(&mut candidate)?;

        if should_save {
            self.save_config_snapshot(candidate.clone()).await?;
            let mut config = self.config.write().await;
            preserve_in_memory_peer_touches(&base, &config, &mut candidate);
            *config = candidate;
        }

        Ok(result)
    }

    async fn save_config_snapshot(&self, snapshot: RuntimeConfig) -> Result<()> {
        let path = self.config_path.as_ref().clone();
        tokio::task::spawn_blocking(move || save_config_at(&path, &snapshot))
            .await
            .context("join config save task")?
    }

    pub async fn update_bind(&self, bind: String) -> Result<()> {
        validate_bind_address(&bind)?;

        self.mutate_config_and_save(|config| {
            config.api_bind = bind;
            Ok(((), true))
        })
        .await
    }

    pub async fn update_api_transport(&self, api_transport: ApiTransport) -> Result<()> {
        self.mutate_config_and_save(|config| {
            config.api_transport = api_transport;
            Ok(((), true))
        })
        .await
    }

    pub async fn update_api_pipe_name(&self, pipe_name: String) -> Result<()> {
        validate_pipe_name(&pipe_name)?;

        self.mutate_config_and_save(|config| {
            config.api_pipe_name = pipe_name;
            Ok(((), true))
        })
        .await
    }

    pub async fn update_network_port(&self, port: u16) -> Result<()> {
        self.mutate_config_and_save(|config| {
            config.network_port = port;
            Ok(((), true))
        })
        .await
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

        self.mutate_config_and_save(|config| {
            config.file_transfer = file_transfer;
            Ok(((), true))
        })
        .await
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

        self.mutate_config_and_save(|config| {
            config.input_handoff = input_handoff;
            Ok(((), true))
        })
        .await?;
        self.notify_input_capture_wake("input_handoff_config_changed");
        Ok(())
    }

    pub async fn set_discovered_endpoint(
        &self,
        machine_id: &str,
        display_name: &str,
        endpoint: SocketAddr,
    ) -> Option<DiscoveredPeerEndpoint> {
        self.set_discovered_endpoints(machine_id, display_name, vec![endpoint])
            .await
    }

    pub async fn set_discovered_endpoints(
        &self,
        machine_id: &str,
        display_name: &str,
        mut endpoint_candidates: Vec<SocketAddr>,
    ) -> Option<DiscoveredPeerEndpoint> {
        endpoint_candidates.dedup();
        let endpoint = endpoint_candidates
            .first()
            .copied()
            .expect("discovered endpoint candidates must not be empty");
        let previous = self.discovery.endpoints.write().await.insert(
            machine_id.to_string(),
            DiscoveredPeerEndpoint {
                display_name: display_name.to_string(),
                endpoint,
                endpoint_candidates,
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

    pub async fn discovered_endpoint_candidates(&self, machine_id: &str) -> Vec<SocketAddr> {
        self.discovery
            .endpoints
            .read()
            .await
            .get(machine_id)
            .map(|record| record.endpoint_candidates.clone())
            .unwrap_or_default()
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
        self.set_layout_canonical(matrix).await?;
        Ok(())
    }

    pub async fn set_layout_and_queue_sync(&self, matrix: String) -> Result<usize> {
        let canonical_matrix = self.set_layout_canonical(matrix).await?;
        Ok(self
            .queue_layout_matrix_for_connected_peers(canonical_matrix, None)
            .await)
    }

    async fn set_layout_canonical(&self, matrix: String) -> Result<String> {
        self.mutate_config_and_save(|config| {
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
            config.layout_matrix = canonical_matrix.clone();
            Ok((canonical_matrix, true))
        })
        .await?;
        self.invalidate_cached_layout_matrix().await;
        self.notify_input_capture_wake("layout_changed");
        Ok(self.config.read().await.layout_matrix.clone())
    }

    pub async fn apply_remote_layout_matrix(
        &self,
        source_peer_id: &str,
        remote_machine_id: &str,
        matrix: String,
    ) -> Result<()> {
        let mirrored = self
            .mirror_remote_layout_matrix(source_peer_id, remote_machine_id, &matrix)
            .await?;
        self.set_layout_canonical(mirrored).await?;
        Ok(())
    }

    async fn mirror_remote_layout_matrix(
        &self,
        source_peer_id: &str,
        remote_machine_id: &str,
        matrix: &str,
    ) -> Result<String> {
        let config = self.config.read().await;
        let local_machine_id = config.machine_id.clone();
        let local_display_name = config.device_name.clone();
        let peers = config
            .peers
            .iter()
            .map(|peer| LayoutPeerToken {
                peer_id: peer.peer_id.clone(),
                display_name: peer.display_name.clone(),
            })
            .collect::<Vec<_>>();
        drop(config);

        let mirrored = parse_layout_matrix(matrix)
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|token| {
                        let token = token.trim();
                        if token.is_empty() {
                            String::new()
                        } else if token.eq_ignore_ascii_case(CANONICAL_LOCAL_LAYOUT_TOKEN)
                            || token.eq_ignore_ascii_case(remote_machine_id)
                        {
                            source_peer_id.to_string()
                        } else if token.eq_ignore_ascii_case(&local_machine_id)
                            || token.eq_ignore_ascii_case(&local_display_name)
                        {
                            CANONICAL_LOCAL_LAYOUT_TOKEN.to_string()
                        } else {
                            token.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(";");

        canonicalize_layout_matrix_spec(
            &mirrored,
            &local_machine_id,
            Some(local_display_name.as_str()),
            &peers,
        )
    }

    pub(crate) async fn queue_layout_matrix_for_connected_peers(
        &self,
        matrix_spec: String,
        except_peer_id: Option<&str>,
    ) -> usize {
        let peer_ids = self
            .connected_peer_ids()
            .await
            .into_iter()
            .filter(|peer_id| except_peer_id != Some(peer_id.as_str()))
            .collect::<Vec<_>>();

        for peer_id in &peer_ids {
            self.queue_outgoing_bulk_payload(
                peer_id,
                OutboundPayload::LayoutMatrix {
                    matrix_spec: matrix_spec.clone(),
                },
            )
            .await;
        }

        peer_ids.len()
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
        match name.as_str() {
            "share_clipboard" | "transfer_file" | "share_input" | "easy_mouse" | "wrap_mouse" => {}
            "same_subnet_only" | "validate_remote_ip" => {
                anyhow::bail!(
                    "{name} is visible in the tray but unsupported until network policy enforcement lands"
                );
            }
            _ => anyhow::bail!("unknown feature '{name}'"),
        }
        let feature_name = name.clone();
        self.mutate_config_and_save(|config| {
            config.features.insert(feature_name, enabled);
            Ok(((), true))
        })
        .await?;

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
        crate::hotkeys::validate_hotkey_binding(&action, &combo)?;
        let new_combo = crate::hotkeys::canonical_hotkey_combo(&combo)?;
        self.mutate_config_and_save(|config| {
            for (existing_action, existing_combo) in &config.hotkeys {
                if existing_action != &action
                    && new_combo.is_some()
                    && crate::hotkeys::canonical_hotkey_combo(existing_combo)? == new_combo
                {
                    anyhow::bail!(
                        "hotkey combo already assigned to {existing_action}; choose a unique combo"
                    );
                }
            }
            config.hotkeys.insert(action, combo);
            Ok(((), true))
        })
        .await
    }
}
