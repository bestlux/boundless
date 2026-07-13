use super::input_ops::active_input_capture_target_from_config;
use super::*;

impl AppState {
    pub(crate) async fn control_plane_snapshot_bundle(&self) -> ControlPlaneSnapshotBundle {
        let config = self.snapshot().await;
        let peers = config.peers.clone();
        let layout_matrix = config.layout_matrix.clone();
        let features = config.features.clone();
        let anti_idle_config = config.anti_idle.clone();
        let input_handoff_config = config.input_handoff.clone();

        let (
            discovered_endpoints,
            pending_requests,
            trusted_records,
            transport_events,
            input_owner_peer_id,
            input_capture_target_peer_id,
            input_lock_runtime,
            mdns_active,
            anti_idle_runtime,
            input_capture_backend_mode,
            clipboard_backend_mode,
            pending_inject_stats,
            file_transfers,
        ) = tokio::join!(
            self.discovered_endpoints(),
            self.list_pending_nearby_pairing_requests(),
            self.trusted_records(),
            self.transport_events(),
            self.input_owner(),
            self.input_capture_target(),
            self.input_lock_runtime(),
            self.mdns_active(),
            self.async_anti_idle_runtime_state(),
            self.input_capture_backend_mode(),
            async { self.clipboard_backend_mode() },
            self.pending_inject_frame_stats(),
            self.file_transfer_records(),
        );

        let (input_locked, input_lock_supported) = input_lock_runtime;
        let (pending_inject_frames, pending_inject_high_water) = pending_inject_stats;
        let elevated_injector_status = self.input_broker.elevated_injector_status();
        let active_input_capture_target_peer_id =
            if input_capture_backend_mode == "service_session_unsupported" {
                None
            } else {
                input_capture_target_peer_id
                    .as_deref()
                    .and_then(|target| active_input_capture_target_from_config(&config, target))
            };

        ControlPlaneSnapshotBundle {
            config,
            peers,
            layout_matrix,
            features,
            discovered_endpoints,
            pending_requests,
            trusted_records: trusted_records.unwrap_or_default(),
            transport_events,
            input_owner_peer_id,
            input_capture_target_peer_id,
            active_input_capture_target_peer_id,
            input_locked,
            input_lock_supported,
            mdns_active,
            anti_idle_config,
            anti_idle_runtime,
            input_handoff_config,
            input_capture_backend_mode,
            clipboard_backend_mode: clipboard_backend_mode.to_string(),
            pending_inject_frames,
            pending_inject_high_water,
            elevated_injector_state: elevated_injector_status.state,
            elevated_injector_reason: elevated_injector_status.reason,
            elevated_injector_signature_trust: elevated_injector_status.signature_trust,
            file_transfers,
        }
    }

    async fn async_anti_idle_runtime_state(&self) -> AntiIdleRuntimeState {
        self.anti_idle_runtime_state().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state_with_target_peer() -> (AppState, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-snapshot-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        (state, peer_id, root)
    }

    #[tokio::test]
    async fn bundle_uses_captured_config_for_active_capture_target() {
        let (state, peer_id, root) = state_with_target_peer().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");
        state
            .set_feature("share_input".to_string(), false)
            .await
            .expect("disable input share");

        let bundle = state.control_plane_snapshot_bundle().await;

        assert_eq!(
            bundle.input_capture_target_peer_id.as_deref(),
            Some(peer_id.as_str())
        );
        assert_eq!(
            bundle.features.get("share_input"),
            Some(&false),
            "bundle should expose the captured feature state"
        );
        assert!(
            bundle.active_input_capture_target_peer_id.is_none(),
            "active capture target must be derived from the same captured config"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bundle_keeps_active_capture_target_for_connected_peer() {
        let (state, peer_id, root) = state_with_target_peer().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");

        let bundle = state.control_plane_snapshot_bundle().await;

        assert_eq!(
            bundle.input_capture_target_peer_id.as_deref(),
            Some(peer_id.as_str())
        );
        assert_eq!(
            bundle.active_input_capture_target_peer_id.as_deref(),
            Some(peer_id.as_str())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bundle_clears_active_capture_target_when_service_session_input_is_unsupported() {
        let (state, peer_id, root) = state_with_target_peer().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");
        state
            .set_input_capture_backend_mode("service_session_unsupported")
            .await;

        let bundle = state.control_plane_snapshot_bundle().await;

        assert_eq!(
            bundle.input_capture_target_peer_id.as_deref(),
            Some(peer_id.as_str()),
            "configured target should remain visible"
        );
        assert!(
            bundle.active_input_capture_target_peer_id.is_none(),
            "unsupported service-session runtime must not look capture-ready"
        );
        assert_eq!(
            bundle.input_capture_backend_mode,
            "service_session_unsupported"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
