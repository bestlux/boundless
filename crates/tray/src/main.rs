#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("boundlesstray is currently supported on Windows only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_app::run()
}

#[cfg(windows)]
mod windows_app {
    use anyhow::{Context, Result, bail};
    use app_services::desktop::{
        CANONICAL_LOCAL_LAYOUT_TOKEN, LayoutPeerToken, host_and_pairing_port_from_endpoint,
        is_local_layout_token as shared_is_local_layout_token, parse_pairing_port,
        resolve_boundlessd_candidates, serialize_layout_matrix, spawn_boundlessd_process,
        terminate_boundlessd_processes, validate_layout_matrix_spec,
    };
    use clap::Parser;
    use control_plane_client::{
        channel, connect_control_plane, default_endpoint, has_access_denied_io_error,
        is_named_pipe_endpoint,
    };
    use eframe::icon_data;
    use image::ImageFormat;
    use ipc_api::boundless::v1::{
        AntiIdleSetRequest, Empty, FeatureSetRequest, FileTransferActionRequest,
        FileTransferSetRequest, HotkeySetRequest, HotkeyTriggerRequest, InputHandoffSetRequest,
        LayoutSetRequest, NearbyPairingDecisionRequest, NearbyRequestCodeStartRequest,
        NearbySubmitCodeRequest, RemovePeerRequest, SafeResetRequest, SendFileRequest,
    };
    use serde::Deserialize;
    use std::{
        collections::BTreeMap,
        future::Future,
        process::Command as ProcessCommand,
        time::{Duration, Instant},
    };
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{Menu, MenuItem, PredefinedMenuItem},
    };

    const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon.png");
    const TRAY_ICON_BYTES: &[u8] = include_bytes!("../assets/tray-icon-20.png");
    const BOUNDLESS_SERVICE_NAME: &str = "BoundlessService";
    const ACTION_DASHBOARD: &str = "dashboard";
    const ACTION_QUIT: &str = "quit";
    #[derive(Debug, Parser)]
    #[command(
        name = "boundlesstray",
        version,
        about = "Boundless tray control surface"
    )]
    struct Cli {
        #[arg(
            long,
            env = "BOUNDLESS_API_ENDPOINT",
            default_value_t = default_endpoint()
        )]
        endpoint: String,
        #[arg(long, default_value_t = true)]
        start_daemon: bool,
    }

    #[derive(Debug)]
    struct AppContext {
        endpoint: String,
        start_daemon: bool,
        daemon_candidates: Vec<String>,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiSnapshot {
        generated_at: String,
        daemon_online: bool,
        machine_id: String,
        layout_matrix: String,
        features: BTreeMap<String, bool>,
        hotkeys: BTreeMap<String, String>,
        discovered_peers: Vec<UiDiscoveredPeer>,
        paired_peers: Vec<UiPairedPeer>,
        pending_requests: Vec<UiPendingRequest>,
        anti_idle_config: UiAntiIdleConfig,
        anti_idle_status: UiAntiIdleStatus,
        file_transfer_config: UiFileTransferConfig,
        file_transfers: Vec<UiFileTransfer>,
        input_handoff_config: UiInputHandoffConfig,
        input_runtime: UiInputRuntime,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiAntiIdleConfig {
        enabled: bool,
        recent_activity_window_secs: u32,
        allow_on_battery: bool,
        keep_display_on: bool,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiAntiIdleStatus {
        supported: bool,
        enabled: bool,
        active: bool,
        display_required: bool,
        reason: String,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiFileTransferConfig {
        receive_dir: String,
        organize_by_peer: bool,
        auto_accept_trusted_peers: bool,
        max_file_bytes: u64,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiFileTransfer {
        transfer_id: String,
        previous_transfer_id: String,
        direction: String,
        peer_id: String,
        file_name: String,
        state: String,
        transferred_bytes: u64,
        total_bytes: u64,
        failure_reason: String,
        source_path: String,
        final_path: String,
        queued_at: String,
        updated_at: String,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiInputHandoffConfig {
        block_screen_corners: bool,
        corner_block_px: u32,
        relative_mouse: bool,
        hide_cursor_at_edge: bool,
        draw_cursor_marker: bool,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiInputRuntime {
        owner_peer_id: String,
        configured_capture_target_peer_id: String,
        active_capture_target_peer_id: String,
        lock_active: bool,
        lock_supported: bool,
        capture_backend_mode: String,
        pending_inject_frames: u32,
        pending_inject_high_water: u32,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiDiscoveredPeer {
        machine_id: String,
        display_name: String,
        endpoint: String,
        #[serde(default)]
        endpoint_candidates: Vec<String>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiPairedPeer {
        peer_id: String,
        display_name: String,
        address: String,
        connected: bool,
        health_state: String,
        health_reason: String,
        trust_state: String,
        trusted_since: String,
        trust_fingerprint: String,
        device_identity: String,
    }

    #[allow(dead_code)]
    #[derive(Debug, Clone, Deserialize)]
    struct UiPendingRequest {
        request_id: String,
        requester_machine_id: String,
        requester_display_name: String,
        created_at: String,
        #[serde(default)]
        verification_code: String,
        #[serde(default)]
        verification_expires_at: String,
        #[serde(default)]
        requires_verification_code: bool,
    }

    enum NearbyRequestCodeStart {
        CodeRequired {
            request_id: String,
            verification_nonce: String,
            expires_at: String,
        },
        Unsupported {
            reason: String,
        },
    }

    #[derive(Debug, Clone)]
    struct GuidedPairingFlow {
        dialog_title: String,
        host: String,
        pairing_port: u16,
        default_alias: String,
        orientation_selector_fallback: String,
        endpoint_candidates: Vec<String>,
    }

    #[derive(Debug, Clone)]
    struct PairingChallengeState {
        request_id: String,
        verification_nonce: String,
        expires_at: String,
    }

    #[derive(Debug, Clone)]
    struct GuidedPairingResult {
        peer_machine_id: String,
        orientation_selector: String,
        message: String,
    }

    #[derive(Debug, Clone)]
    struct PairingSubmitResult {
        peer_machine_id: String,
        message: String,
    }

    struct NearbySubmitCode {
        request_id: String,
        code: String,
        verification_nonce: String,
        host: String,
        port: u16,
        alias: Option<String>,
        endpoint_candidates: Vec<String>,
    }

    include!("dashboard.rs");

    #[cfg(test)]
    #[allow(dead_code)]
    mod dashboard_test_support {
        include!("dashboard_test_support.rs");
    }

    #[cfg(test)]
    mod dashboard_pairing_target_selection_tests {
        include!("dashboard_pairing_target_selection_tests.rs");
    }

    #[cfg(test)]
    mod dashboard_pairing_transition_tests {
        include!("dashboard_pairing_transition_tests.rs");
    }

    #[cfg(test)]
    mod dashboard_transfer_center_tests {
        include!("dashboard_transfer_center_tests.rs");
    }

    #[cfg(test)]
    fn filter_connectable_discovered_peers(
        discovered_peers: Vec<UiDiscoveredPeer>,
        local_machine_id: &str,
        paired_peers: &[UiPairedPeer],
    ) -> Vec<UiDiscoveredPeer> {
        let local_machine_id = local_machine_id.to_ascii_lowercase();
        let paired_peer_ids = paired_peers
            .iter()
            .map(|peer| peer.peer_id.to_ascii_lowercase())
            .collect::<std::collections::HashSet<_>>();
        let mut discovered_peers = discovered_peers
            .into_iter()
            .filter(|peer| {
                let machine_id = peer.machine_id.to_ascii_lowercase();
                machine_id != local_machine_id && !paired_peer_ids.contains(&machine_id)
            })
            .collect::<Vec<_>>();
        discovered_peers.sort_by(|a, b| {
            a.display_name
                .to_ascii_lowercase()
                .cmp(&b.display_name.to_ascii_lowercase())
                .then_with(|| a.machine_id.cmp(&b.machine_id))
        });
        discovered_peers
    }

    fn watch_ui_snapshots_blocking<F>(endpoint: &str, mut on_snapshot: F) -> Result<()>
    where
        F: FnMut(UiSnapshot) -> Result<()>,
    {
        block_on_result(async move {
            let mut client = connect_control_plane(endpoint).await?;
            let mut stream = client.watch_ui(Empty {}).await?.into_inner();
            while let Some(snapshot) = stream.message().await? {
                on_snapshot(UiSnapshot {
                    generated_at: snapshot.generated_at,
                    daemon_online: snapshot.daemon_online,
                    machine_id: snapshot.machine_id,
                    layout_matrix: snapshot.layout_matrix,
                    features: snapshot.features.into_iter().collect(),
                    hotkeys: snapshot.hotkeys.into_iter().collect(),
                    discovered_peers: snapshot
                        .discovered_peers
                        .into_iter()
                        .map(|peer| UiDiscoveredPeer {
                            machine_id: peer.machine_id,
                            display_name: peer.display_name,
                            endpoint: peer.endpoint,
                            endpoint_candidates: peer.endpoint_candidates,
                        })
                        .collect(),
                    paired_peers: snapshot
                        .paired_peers
                        .into_iter()
                        .map(|peer| UiPairedPeer {
                            peer_id: peer.peer_id,
                            display_name: peer.display_name,
                            address: peer.address,
                            connected: peer.connected,
                            health_state: peer.health_state,
                            health_reason: peer.health_reason,
                            trust_state: peer.trust_state,
                            trusted_since: peer.trusted_since,
                            trust_fingerprint: peer.trust_fingerprint,
                            device_identity: peer.device_identity,
                        })
                        .collect(),
                    pending_requests: snapshot
                        .pending_requests
                        .into_iter()
                        .map(|request| UiPendingRequest {
                            request_id: request.request_id,
                            requester_machine_id: request.requester_machine_id,
                            requester_display_name: request.requester_display_name,
                            created_at: request.created_at,
                            verification_code: request.verification_code,
                            verification_expires_at: request.verification_expires_at,
                            requires_verification_code: request.requires_verification_code,
                        })
                        .collect(),
                    anti_idle_config: snapshot
                        .anti_idle_config
                        .map(|config| UiAntiIdleConfig {
                            enabled: config.enabled,
                            recent_activity_window_secs: config.recent_activity_window_secs,
                            allow_on_battery: config.allow_on_battery,
                            keep_display_on: config.keep_display_on,
                        })
                        .unwrap_or_default(),
                    anti_idle_status: snapshot
                        .anti_idle_status
                        .map(|status| UiAntiIdleStatus {
                            supported: status.supported,
                            enabled: status.enabled,
                            active: status.active,
                            display_required: status.display_required,
                            reason: status.reason,
                        })
                        .unwrap_or_default(),
                    file_transfer_config: snapshot
                        .file_transfer_config
                        .map(|config| UiFileTransferConfig {
                            receive_dir: config.receive_dir,
                            organize_by_peer: config.organize_by_peer,
                            auto_accept_trusted_peers: config.auto_accept_trusted_peers,
                            max_file_bytes: config.max_file_bytes,
                        })
                        .unwrap_or_default(),
                    file_transfers: snapshot
                        .file_transfers
                        .into_iter()
                        .map(|transfer| UiFileTransfer {
                            transfer_id: transfer.transfer_id,
                            previous_transfer_id: transfer.previous_transfer_id,
                            direction: transfer.direction,
                            peer_id: transfer.peer_id,
                            file_name: transfer.file_name,
                            state: transfer.state,
                            transferred_bytes: transfer.transferred_bytes,
                            total_bytes: transfer.total_bytes,
                            failure_reason: transfer.failure_reason,
                            source_path: transfer.source_path,
                            final_path: transfer.final_path,
                            queued_at: transfer.queued_at,
                            updated_at: transfer.updated_at,
                        })
                        .collect(),
                    input_handoff_config: snapshot
                        .input_handoff_config
                        .map(|config| UiInputHandoffConfig {
                            block_screen_corners: config.block_screen_corners,
                            corner_block_px: config.corner_block_px,
                            relative_mouse: config.relative_mouse,
                            hide_cursor_at_edge: config.hide_cursor_at_edge,
                            draw_cursor_marker: config.draw_cursor_marker,
                        })
                        .unwrap_or_default(),
                    input_runtime: snapshot
                        .input_runtime
                        .map(|runtime| UiInputRuntime {
                            owner_peer_id: runtime.owner_peer_id,
                            configured_capture_target_peer_id: runtime
                                .configured_capture_target_peer_id,
                            active_capture_target_peer_id: runtime.active_capture_target_peer_id,
                            lock_active: runtime.lock_active,
                            lock_supported: runtime.lock_supported,
                            capture_backend_mode: runtime.capture_backend_mode,
                            pending_inject_frames: runtime.pending_inject_frames,
                            pending_inject_high_water: runtime.pending_inject_high_water,
                        })
                        .unwrap_or_default(),
                })?;
            }
            Ok(())
        })
    }

    fn pair_nearby_request_code_blocking(
        endpoint: &str,
        host: String,
        port: u16,
        endpoint_candidates: Vec<String>,
    ) -> Result<NearbyRequestCodeStart> {
        block_on_result(pair_nearby_request_code(
            endpoint,
            host,
            port,
            endpoint_candidates,
        ))
    }

    fn pair_nearby_submit_code_blocking(
        endpoint: &str,
        request: NearbySubmitCode,
    ) -> Result<PairingSubmitResult> {
        block_on_result(pair_nearby_submit_code(endpoint, request))
    }

    fn approve_nearby_pairing_request_blocking(endpoint: &str, request_id: &str) -> Result<String> {
        block_on_result(approve_nearby_pairing_request(
            endpoint,
            request_id.to_string(),
        ))
    }

    fn reject_nearby_pairing_request_blocking(endpoint: &str, request_id: &str) -> Result<String> {
        block_on_result(reject_nearby_pairing_request(
            endpoint,
            request_id.to_string(),
        ))
    }

    fn trigger_hotkey_action_blocking(endpoint: &str, action: &str) -> Result<String> {
        block_on_result(trigger_hotkey_action(endpoint, action.to_string()))
    }

    fn remove_peer_blocking(endpoint: &str, peer_id: String) -> Result<String> {
        block_on_result(remove_peer(endpoint, peer_id))
    }

    fn layout_set_blocking(endpoint: &str, matrix_spec: String) -> Result<String> {
        block_on_result(layout_set(endpoint, matrix_spec))
    }

    fn set_anti_idle_config_blocking(
        endpoint: &str,
        enabled: bool,
        recent_activity_window_secs: u32,
        allow_on_battery: bool,
        keep_display_on: bool,
    ) -> Result<String> {
        block_on_result(set_anti_idle_config(
            endpoint,
            enabled,
            recent_activity_window_secs,
            allow_on_battery,
            keep_display_on,
        ))
    }

    fn set_file_transfer_config_blocking(
        endpoint: &str,
        receive_dir: String,
        organize_by_peer: bool,
        auto_accept_trusted_peers: bool,
        max_file_bytes: u64,
    ) -> Result<String> {
        block_on_result(set_file_transfer_config(
            endpoint,
            receive_dir,
            organize_by_peer,
            auto_accept_trusted_peers,
            max_file_bytes,
        ))
    }

    fn cancel_file_transfer_blocking(endpoint: &str, transfer_id: String) -> Result<String> {
        block_on_result(cancel_file_transfer(endpoint, transfer_id))
    }

    fn retry_file_transfer_blocking(endpoint: &str, transfer_id: String) -> Result<String> {
        block_on_result(retry_file_transfer(endpoint, transfer_id))
    }

    fn clear_completed_file_transfers_blocking(endpoint: &str) -> Result<String> {
        block_on_result(clear_completed_file_transfers(endpoint))
    }

    fn set_feature_blocking(endpoint: &str, name: String, enabled: bool) -> Result<String> {
        block_on_result(set_feature(endpoint, name, enabled))
    }

    fn set_input_handoff_config_blocking(
        endpoint: &str,
        block_screen_corners: bool,
        corner_block_px: u32,
        relative_mouse: bool,
        hide_cursor_at_edge: bool,
        draw_cursor_marker: bool,
    ) -> Result<String> {
        block_on_result(set_input_handoff_config(
            endpoint,
            block_screen_corners,
            corner_block_px,
            relative_mouse,
            hide_cursor_at_edge,
            draw_cursor_marker,
        ))
    }

    fn set_hotkey_blocking(endpoint: &str, action: String, combo: String) -> Result<String> {
        block_on_result(set_hotkey(endpoint, action, combo))
    }

    fn safe_reset_blocking(
        endpoint: &str,
        network_only: bool,
        all: bool,
        confirm: String,
    ) -> Result<String> {
        block_on_result(safe_reset(endpoint, network_only, all, confirm))
    }

    fn ensure_daemon_available_blocking(ctx: &AppContext) -> Result<Option<String>> {
        block_on_result(ensure_daemon_available(
            &ctx.endpoint,
            ctx.start_daemon,
            &ctx.daemon_candidates,
        ))
    }

    fn block_on_result<F, T>(future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create tokio runtime for tray async flow")?;
        runtime.block_on(future)
    }

    async fn trigger_hotkey_action(endpoint: &str, action: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .trigger_hotkey_action(HotkeyTriggerRequest { action })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn layout_set(endpoint: &str, matrix_spec: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .layout_set(LayoutSetRequest { matrix_spec })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn set_anti_idle_config(
        endpoint: &str,
        enabled: bool,
        recent_activity_window_secs: u32,
        allow_on_battery: bool,
        keep_display_on: bool,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .set_anti_idle_config(AntiIdleSetRequest {
                enabled,
                recent_activity_window_secs,
                allow_on_battery,
                keep_display_on,
            })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn set_file_transfer_config(
        endpoint: &str,
        receive_dir: String,
        organize_by_peer: bool,
        auto_accept_trusted_peers: bool,
        max_file_bytes: u64,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .set_file_transfer_config(FileTransferSetRequest {
                receive_dir,
                organize_by_peer,
                auto_accept_trusted_peers,
                max_file_bytes,
            })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn cancel_file_transfer(endpoint: &str, transfer_id: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .cancel_file_transfer(FileTransferActionRequest { transfer_id })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn retry_file_transfer(endpoint: &str, transfer_id: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .retry_file_transfer(FileTransferActionRequest { transfer_id })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn clear_completed_file_transfers(endpoint: &str) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .clear_completed_file_transfers(Empty {})
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn set_feature(endpoint: &str, name: String, enabled: bool) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .set_feature(FeatureSetRequest { name, enabled })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn set_input_handoff_config(
        endpoint: &str,
        block_screen_corners: bool,
        corner_block_px: u32,
        relative_mouse: bool,
        hide_cursor_at_edge: bool,
        draw_cursor_marker: bool,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .set_input_handoff_config(InputHandoffSetRequest {
                block_screen_corners,
                corner_block_px,
                relative_mouse,
                hide_cursor_at_edge,
                draw_cursor_marker,
            })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn set_hotkey(endpoint: &str, action: String, combo: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .set_hotkey(HotkeySetRequest { action, combo })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    async fn safe_reset(
        endpoint: &str,
        network_only: bool,
        all: bool,
        confirm: String,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .safe_reset(SafeResetRequest {
                network_only,
                all,
                confirm,
            })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    fn send_files_to_peer_blocking(
        endpoint: &str,
        peer_id: String,
        paths: Vec<String>,
    ) -> Result<String> {
        block_on_result(send_files_to_peer(endpoint, peer_id, paths))
    }

    async fn send_files_to_peer(
        endpoint: &str,
        peer_id: String,
        paths: Vec<String>,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let total = paths.len();
        for path in paths {
            let response = client
                .send_file(SendFileRequest {
                    peer_id: peer_id.clone(),
                    file_path: path,
                })
                .await?
                .into_inner();
            if !response.ok {
                bail!(response.message);
            }
        }
        Ok(format!(
            "Queued {total} file{} for transfer",
            if total == 1 { "" } else { "s" }
        ))
    }

    async fn ensure_daemon_available(
        endpoint: &str,
        start_daemon: bool,
        daemon_candidates: &[String],
    ) -> Result<Option<String>> {
        let initial_error = match channel(endpoint).await {
            Ok(_) => return Ok(None),
            Err(error) => error,
        };

        if !start_daemon {
            bail!("daemon is not reachable at {endpoint}; run boundlessd or pass --start-daemon");
        }

        if is_named_pipe_endpoint(endpoint) {
            match query_boundless_service_state() {
                BoundlessServiceState::Running => {
                    bail!(
                        "{BOUNDLESS_SERVICE_NAME} is running but the tray cannot reach the service pipe at {endpoint}: {initial_error}. Do not start a separate per-user boundlessd.exe. Restart {BOUNDLESS_SERVICE_NAME}, repair the install, or verify the MSI allowed-user SID."
                    );
                }
                BoundlessServiceState::Installed { state } => {
                    bail!(
                        "{BOUNDLESS_SERVICE_NAME} is installed but not running (state={state}) and the tray cannot reach {endpoint}: {initial_error}. Start {BOUNDLESS_SERVICE_NAME} or repair the install; do not start a separate per-user boundlessd.exe."
                    );
                }
                BoundlessServiceState::Missing => {}
                BoundlessServiceState::QueryFailed(error) => {
                    bail!(
                        "could not determine whether {BOUNDLESS_SERVICE_NAME} is installed before starting a local daemon for {endpoint}: {error}. Start or repair {BOUNDLESS_SERVICE_NAME}, or remove the service for dev-mode tray startup."
                    );
                }
            }
        }

        if is_named_pipe_endpoint(endpoint) && has_access_denied_io_error(&initial_error) {
            let launched = recover_stale_named_pipe_owner(endpoint, daemon_candidates).await?;
            return Ok(Some(format!(
                "{launched} (after clearing stale boundlessd.exe named-pipe owner)"
            )));
        }

        let launched = spawn_daemon_process(daemon_candidates)?;
        wait_for_daemon_ready(endpoint, launched, "start attempt").await
    }

    async fn recover_stale_named_pipe_owner(
        endpoint: &str,
        daemon_candidates: &[String],
    ) -> Result<String> {
        let terminated = terminate_boundlessd_processes()?;
        tokio::time::sleep(Duration::from_millis(400)).await;

        if channel(endpoint).await.is_ok() {
            return Ok("existing daemon became reachable after stale-process cleanup".to_string());
        }

        let launched = spawn_daemon_process(daemon_candidates)?;
        let context = if terminated {
            "stale-daemon recovery"
        } else {
            "named-pipe recovery"
        };
        match wait_for_daemon_ready(endpoint, launched.clone(), context).await? {
            Some(path) => Ok(path),
            None => Ok("existing daemon became reachable during named-pipe recovery".to_string()),
        }
    }

    async fn wait_for_daemon_ready(
        endpoint: &str,
        launched: String,
        context: &str,
    ) -> Result<Option<String>> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match channel(endpoint).await {
                Ok(_) => return Ok(Some(launched)),
                Err(error) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "daemon did not become reachable at {endpoint} after {context}: {error}"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    fn spawn_daemon_process(candidates: &[String]) -> Result<String> {
        spawn_boundlessd_process(candidates)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BoundlessServiceState {
        Missing,
        Running,
        Installed { state: String },
        QueryFailed(String),
    }

    fn query_boundless_service_state() -> BoundlessServiceState {
        match ProcessCommand::new("sc.exe")
            .args(["query", BOUNDLESS_SERVICE_NAME])
            .output()
        {
            Ok(output) => parse_boundless_service_state(
                output.status.success(),
                &String::from_utf8_lossy(&output.stdout),
                &String::from_utf8_lossy(&output.stderr),
            ),
            Err(error) => BoundlessServiceState::QueryFailed(error.to_string()),
        }
    }

    fn parse_boundless_service_state(
        success: bool,
        stdout: &str,
        stderr: &str,
    ) -> BoundlessServiceState {
        let combined = format!("{stdout}\n{stderr}");
        let lowered = combined.to_ascii_lowercase();
        if lowered.contains("failed 1060") || lowered.contains("does not exist") {
            return BoundlessServiceState::Missing;
        }

        for line in stdout.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with("STATE") {
                continue;
            }
            if trimmed.contains("RUNNING") {
                return BoundlessServiceState::Running;
            }
            let state = trimmed
                .split_once(':')
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_string());
            return BoundlessServiceState::Installed { state };
        }

        if success {
            BoundlessServiceState::Installed {
                state: "unknown".to_string(),
            }
        } else {
            BoundlessServiceState::QueryFailed(combined.trim().to_string())
        }
    }

    async fn pair_nearby_request_code(
        endpoint: &str,
        host: String,
        port: u16,
        endpoint_candidates: Vec<String>,
    ) -> Result<NearbyRequestCodeStart> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .request_nearby_pairing_code(NearbyRequestCodeStartRequest {
                host,
                port: u32::from(port),
                alias: String::new(),
                endpoint_candidates,
            })
            .await?
            .into_inner();

        if response.code_required {
            return Ok(NearbyRequestCodeStart::CodeRequired {
                request_id: response.request_id,
                verification_nonce: response.verification_nonce,
                expires_at: response.verification_expires_at,
            });
        }

        if response.unsupported {
            return Ok(NearbyRequestCodeStart::Unsupported {
                reason: response.message,
            });
        }

        bail!("nearby pairing request failed: {}", response.message);
    }

    async fn pair_nearby_submit_code(
        endpoint: &str,
        request: NearbySubmitCode,
    ) -> Result<PairingSubmitResult> {
        let expected_request_id = request.request_id.clone();
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .submit_nearby_pairing_code(NearbySubmitCodeRequest {
                host: request.host,
                port: u32::from(request.port),
                request_id: request.request_id.clone(),
                code: request.code,
                verification_nonce: request.verification_nonce,
                alias: request.alias.unwrap_or_default(),
                endpoint_candidates: request.endpoint_candidates,
            })
            .await?
            .into_inner();

        if !response.ok {
            bail!("nearby pairing failed: {}", response.message);
        }
        if response.request_id != expected_request_id {
            bail!("nearby pairing request id mismatch");
        }

        Ok(PairingSubmitResult {
            peer_machine_id: response.peer_machine_id,
            message: response.message,
        })
    }

    async fn approve_nearby_pairing_request(endpoint: &str, request_id: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .approve_nearby_pairing_request(NearbyPairingDecisionRequest {
                request_id,
                alias: String::new(),
            })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn reject_nearby_pairing_request(endpoint: &str, request_id: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .reject_nearby_pairing_request(NearbyPairingDecisionRequest {
                request_id,
                alias: String::new(),
            })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn remove_peer(endpoint: &str, peer_id: String) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .remove_peer(RemovePeerRequest { peer_id })
            .await?
            .into_inner();
        if !response.ok {
            bail!(response.message);
        }
        Ok(response.message)
    }

    #[cfg(test)]
    fn resolve_discovered_peer<'a>(
        peers: &'a [UiDiscoveredPeer],
        selector: &str,
    ) -> Result<&'a UiDiscoveredPeer> {
        if let Ok(index) = selector.parse::<usize>() {
            if index == 0 {
                bail!("setup selector index must start at 1");
            }
            return peers
                .get(index - 1)
                .ok_or_else(|| anyhow::anyhow!("no discovered peer at index {index}"));
        }

        let normalized = selector.trim();
        if normalized.is_empty() {
            bail!("setup selector must not be empty");
        }
        let selector_lower = normalized.to_ascii_lowercase();
        let matches = peers
            .iter()
            .filter(|peer| {
                peer.machine_id.eq_ignore_ascii_case(normalized)
                    || peer
                        .machine_id
                        .to_ascii_lowercase()
                        .starts_with(&selector_lower)
                    || peer.display_name.eq_ignore_ascii_case(normalized)
                    || peer
                        .display_name
                        .to_ascii_lowercase()
                        .starts_with(&selector_lower)
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!("no discovered peer matching `{selector}`");
        }
        if matches.len() > 1 {
            bail!("multiple discovered peers match `{selector}`; use full machine_id or index");
        }
        Ok(matches[0])
    }

    #[cfg(test)]
    fn is_local_layout_token(token: &str, machine_id: &str) -> bool {
        shared_is_local_layout_token(token, machine_id, None)
    }

    fn make_tray_icon() -> Result<Icon> {
        let image = image::load_from_memory_with_format(TRAY_ICON_BYTES, ImageFormat::Png)
            .context("decode tray icon asset")?
            .into_rgba8();
        let (width, height) = image.dimensions();
        Icon::from_rgba(image.into_raw(), width, height).context("create tray icon image")
    }

    fn make_window_icon() -> Result<egui::IconData> {
        icon_data::from_png_bytes(APP_ICON_BYTES).context("decode window icon asset")
    }

    fn short_token(value: &str) -> &str {
        value.get(..8).unwrap_or(value)
    }

    fn empty_as_none(value: &str) -> &str {
        if value.is_empty() { "none" } else { value }
    }

    fn format_error_for_dialog(error: &anyhow::Error) -> String {
        let message = error.to_string();
        let lowered = message.to_ascii_lowercase();

        if lowered.contains("attempts_remaining=") {
            if let Some(attempts_remaining) = extract_attempts_remaining(&message) {
                return format!(
                    "{message}\n\nCode confirmation failed.\nDouble-check the 6-digit code and retry.\nAttempts remaining: {attempts_remaining}."
                );
            }
            return format!(
                "{message}\n\nCode confirmation failed.\nDouble-check the 6-digit code and retry."
            );
        }

        if lowered.contains("temporarily locked") {
            return format!(
                "{message}\n\nToo many invalid attempts were submitted.\nWait for lockout to expire, then start a new pairing request."
            );
        }

        if lowered.contains("verification nonce is invalid")
            || lowered.contains("verification code and nonce are invalid")
        {
            return format!(
                "{message}\n\nThis pairing request is stale or mismatched.\nStart a new request and enter the fresh code from the target machine."
            );
        }

        if lowered.contains("pairing request rejected") {
            return format!(
                "{message}\n\nThe target rejected the request.\nStart a new pairing request from the tray and confirm on the target machine."
            );
        }

        if lowered.contains("timed out waiting for nearby pairing approval") {
            return format!(
                "{message}\n\nThe target did not approve in time.\nStart a new pairing request and approve it on the target before timeout."
            );
        }

        if lowered.contains("nearby code request rate limited") {
            return format!(
                "{message}\n\nCode requests are briefly rate-limited.\nWait a few seconds and retry."
            );
        }

        if lowered.contains("nearby pairing endpoint closed without a response") {
            return format!(
                "{message}\n\nThe remote pairing service did not respond.\nVerify both trays are updated and retry."
            );
        }
        if lowered.contains("read nearby pairing response timed out")
            || lowered.contains("connect nearby pairing endpoint")
            || lowered.contains("send nearby pairing request timed out")
        {
            let remote_port = extract_pairing_error_remote_port(&message)
                .map(|port| format!(" TCP {port}"))
                .unwrap_or_else(|| " the nearby pairing TCP port".to_string());
            return format!(
                "{message}\n\nThe remote pairing service was discovered, but{remote_port} was not reachable or did not respond.\nVerify both machines are on a trusted Private network. If a firewall rule is needed, make it a manual, admin-approved Private-profile rule scoped to %ProgramFiles%\\Boundless\\boundless-service.exe. Transport also needs TCP 15100 after trust is established."
            );
        }

        message
    }

    fn should_offer_new_request_retry(error: &anyhow::Error) -> bool {
        let lowered = error.to_string().to_ascii_lowercase();
        lowered.contains("pairing request rejected")
            || lowered.contains("verification code expired")
            || lowered.contains("timed out waiting for nearby pairing approval")
            || lowered.contains("nearby pairing request not found")
            || lowered.contains("nearby pairing endpoint closed without a response")
            || lowered.contains("read nearby pairing response timed out")
            || lowered.contains("connect nearby pairing endpoint")
            || lowered.contains("send nearby pairing request timed out")
    }

    fn extract_attempts_remaining(message: &str) -> Option<u8> {
        const MARKER: &str = "attempts_remaining=";
        let marker_index = message.find(MARKER)?;
        let start = marker_index + MARKER.len();
        let digits = message[start..]
            .chars()
            .take_while(|char| char.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        digits.parse::<u8>().ok()
    }

    fn extract_pairing_error_remote_port(message: &str) -> Option<u16> {
        let endpoint_marker = "nearby pairing endpoint ";
        let endpoint_start = message.find(endpoint_marker)? + endpoint_marker.len();
        let endpoint = message[endpoint_start..].split_whitespace().next()?;
        let (_, raw_port) = endpoint.rsplit_once(':')?;
        raw_port.parse::<u16>().ok().filter(|port| *port != 0)
    }

    #[cfg(test)]
    mod tests {
        use super::dashboard_layout::compute_visible_bounds;
        use super::*;

        #[test]
        fn extract_attempts_remaining_reads_numeric_suffix() {
            let message = "verification code is invalid; attempts_remaining=4";
            assert_eq!(extract_attempts_remaining(message), Some(4));
        }

        #[test]
        fn extract_attempts_remaining_ignores_missing_marker() {
            assert_eq!(extract_attempts_remaining("no attempts here"), None);
        }

        #[test]
        fn extract_pairing_error_remote_port_reads_endpoint_port() {
            let message = "connect nearby pairing endpoint 10.10.0.187:15200 timed out after 4s";
            assert_eq!(extract_pairing_error_remote_port(message), Some(15200));
        }

        #[test]
        fn format_error_for_dialog_names_firewall_and_pairing_port() {
            let error = anyhow::anyhow!(
                "connect nearby pairing endpoint 10.10.0.187:15200 timed out after 4s; likely network/firewall reachability issue for remote TCP 15200"
            );
            let formatted = format_error_for_dialog(&error);

            assert!(formatted.contains("TCP 15200"));
            assert!(formatted.contains("Private network"));
            assert!(formatted.contains("admin-approved"));
            assert!(formatted.contains("%ProgramFiles%\\Boundless\\boundless-service.exe"));
            assert!(formatted.contains("TCP 15100"));
        }

        #[test]
        fn resolve_discovered_peer_supports_index_and_prefix() {
            let peers = vec![
                UiDiscoveredPeer {
                    machine_id: "machine-alpha-1234".to_string(),
                    display_name: "Office Desktop".to_string(),
                    endpoint: "10.10.0.10:15100".to_string(),
                    endpoint_candidates: vec!["10.10.0.10:15100".to_string()],
                },
                UiDiscoveredPeer {
                    machine_id: "machine-bravo-5678".to_string(),
                    display_name: "Living Room".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
                    endpoint_candidates: vec!["10.10.0.11:15100".to_string()],
                },
            ];

            let by_index = resolve_discovered_peer(&peers, "1").expect("peer by index");
            assert_eq!(by_index.machine_id, "machine-alpha-1234");

            let by_prefix = resolve_discovered_peer(&peers, "living").expect("peer by prefix");
            assert_eq!(by_prefix.machine_id, "machine-bravo-5678");
        }

        #[test]
        fn resolve_discovered_peer_rejects_ambiguous_matches() {
            let peers = vec![
                UiDiscoveredPeer {
                    machine_id: "machine-alpha-1234".to_string(),
                    display_name: "Office".to_string(),
                    endpoint: "10.10.0.10:15100".to_string(),
                    endpoint_candidates: vec!["10.10.0.10:15100".to_string()],
                },
                UiDiscoveredPeer {
                    machine_id: "machine-beta-5678".to_string(),
                    display_name: "Office Laptop".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
                    endpoint_candidates: vec!["10.10.0.11:15100".to_string()],
                },
            ];

            let error =
                resolve_discovered_peer(&peers, "office").expect_err("must reject ambiguous");
            assert!(
                error
                    .to_string()
                    .contains("multiple discovered peers match"),
                "ambiguous selector should be rejected"
            );
        }

        #[test]
        fn parse_pairing_port_validates_range() {
            assert_eq!(parse_pairing_port("15200").expect("valid port"), 15200);
            assert!(
                parse_pairing_port("0").is_err(),
                "port zero must be rejected"
            );
            assert!(
                parse_pairing_port("not-a-number").is_err(),
                "non-numeric input must be rejected"
            );
        }

        #[test]
        fn should_offer_new_request_retry_matches_rejected_and_timeout() {
            let rejected =
                anyhow::anyhow!("verification code is invalid; pairing request rejected");
            assert!(
                should_offer_new_request_retry(&rejected),
                "rejected requests should offer retry"
            );

            let timeout =
                anyhow::anyhow!("timed out waiting for nearby pairing approval request_id=abc");
            assert!(
                should_offer_new_request_retry(&timeout),
                "timeout should offer retry"
            );
        }

        #[test]
        fn should_offer_new_request_retry_ignores_lockout() {
            let lockout = anyhow::anyhow!(
                "verification temporarily locked after repeated invalid attempts; retry later"
            );
            assert!(
                !should_offer_new_request_retry(&lockout),
                "lockout should not offer immediate retry"
            );
        }

        #[test]
        fn should_offer_new_request_retry_matches_transport_stall_signals() {
            let endpoint_closed =
                anyhow::anyhow!("nearby pairing endpoint closed without a response");
            assert!(
                should_offer_new_request_retry(&endpoint_closed),
                "closed endpoint should offer retry"
            );

            let response_timeout =
                anyhow::anyhow!("read nearby pairing response timed out after 20s");
            assert!(
                should_offer_new_request_retry(&response_timeout),
                "response timeout should offer retry"
            );
        }

        #[test]
        fn service_state_parser_detects_running_stopped_and_missing() {
            let running = r#"
SERVICE_NAME: BoundlessService
        TYPE               : 10  WIN32_OWN_PROCESS
        STATE              : 4  RUNNING
"#;
            assert_eq!(
                parse_boundless_service_state(true, running, ""),
                BoundlessServiceState::Running
            );

            let stopped = r#"
SERVICE_NAME: BoundlessService
        TYPE               : 10  WIN32_OWN_PROCESS
        STATE              : 1  STOPPED
"#;
            assert_eq!(
                parse_boundless_service_state(true, stopped, ""),
                BoundlessServiceState::Installed {
                    state: "1  STOPPED".to_string()
                }
            );

            let missing = "[SC] EnumQueryServicesStatus:OpenService FAILED 1060:\r\nThe specified service does not exist as an installed service.";
            assert_eq!(
                parse_boundless_service_state(false, "", missing),
                BoundlessServiceState::Missing
            );

            assert_eq!(
                parse_boundless_service_state(false, "", "unexpected sc.exe failure"),
                BoundlessServiceState::QueryFailed("unexpected sc.exe failure".to_string())
            );
        }

        #[test]
        fn layout_local_token_recognizes_canonical_aliases_and_machine_id() {
            let machine_id = "local-machine-id";
            assert!(is_local_layout_token("self", machine_id));
            assert!(is_local_layout_token("local", machine_id));
            assert!(is_local_layout_token("this", machine_id));
            assert!(is_local_layout_token("me", machine_id));
            assert!(is_local_layout_token("LOCAL-MACHINE-ID", machine_id));
        }

        #[test]
        fn layout_local_token_rejects_legacy_this_pc() {
            assert!(!is_local_layout_token("THIS-PC", "local-machine-id"));
        }

        #[test]
        fn layout_serialization_uses_canonical_self_token() {
            let mut grid = std::collections::HashMap::<(i32, i32), String>::new();
            grid.insert((1, 1), "local-machine-id".to_string());
            grid.insert((2, 1), "peer-right".to_string());

            let matrix = serialize_layout_matrix(&grid, "local-machine-id");
            assert_eq!(matrix, "self,peer-right");
        }

        #[test]
        fn layout_apply_validation_requires_exactly_one_local_cell() {
            let mut grid = std::collections::HashMap::<(i32, i32), String>::new();
            grid.insert((0, 0), "peer-a".to_string());
            assert!(
                app_services::desktop::validate_layout_before_apply(&grid, "local-machine-id")
                    .is_err(),
                "layout with zero local cells must fail apply validation"
            );

            grid.insert((1, 0), "local-machine-id".to_string());
            assert!(
                app_services::desktop::validate_layout_before_apply(&grid, "local-machine-id")
                    .is_ok(),
                "layout with one local cell should pass apply validation"
            );

            grid.insert((2, 0), "LOCAL-MACHINE-ID".to_string());
            assert!(
                app_services::desktop::validate_layout_before_apply(&grid, "local-machine-id")
                    .is_err(),
                "layout with multiple local cells must fail apply validation"
            );
        }

        #[test]
        fn layout_visible_bounds_include_drag_origin_for_edge_drags() {
            let mut grid = std::collections::HashMap::<(i32, i32), String>::new();
            grid.insert((3, 3), "local-machine-id".to_string());

            let bounds = compute_visible_bounds(&grid, Some((4, 3)));

            assert_eq!(
                bounds,
                (2, 2, 5, 4),
                "dragging an edge device should keep its original edge cell visible"
            );
        }

        #[test]
        fn layout_visible_bounds_center_on_drag_origin_when_grid_is_temporarily_empty() {
            let grid = std::collections::HashMap::<(i32, i32), String>::new();

            let bounds = compute_visible_bounds(&grid, Some((6, 0)));

            assert_eq!(
                bounds,
                (4, 0, 6, 2),
                "dragging the only visible device should still leave a drop target near its origin"
            );
        }

        #[test]
        fn tray_icon_asset_decodes() {
            make_tray_icon().expect("tray icon asset should decode");
        }

        #[test]
        fn window_icon_asset_decodes() {
            make_window_icon().expect("window icon asset should decode");
        }
    }
}
