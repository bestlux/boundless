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
        AntiIdleSetRequest, Empty, FileTransferSetRequest, HotkeyTriggerRequest, LayoutSetRequest,
        NearbyPairingDecisionRequest, NearbyRequestCodeStartRequest, NearbySubmitCodeRequest,
        SendFileRequest,
    };
    use serde::Deserialize;
    use std::{
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
        discovered_peers: Vec<UiDiscoveredPeer>,
        paired_peers: Vec<UiPairedPeer>,
        pending_requests: Vec<UiPendingRequest>,
        anti_idle_config: UiAntiIdleConfig,
        anti_idle_status: UiAntiIdleStatus,
        file_transfer_config: UiFileTransferConfig,
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

    #[derive(Debug, Clone, Deserialize)]
    struct UiDiscoveredPeer {
        machine_id: String,
        display_name: String,
        endpoint: String,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiPairedPeer {
        peer_id: String,
        display_name: String,
        address: String,
        connected: bool,
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
                    discovered_peers: snapshot
                        .discovered_peers
                        .into_iter()
                        .map(|peer| UiDiscoveredPeer {
                            machine_id: peer.machine_id,
                            display_name: peer.display_name,
                            endpoint: peer.endpoint,
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
                })?;
            }
            Ok(())
        })
    }

    fn pair_nearby_request_code_blocking(
        endpoint: &str,
        host: String,
        port: u16,
    ) -> Result<NearbyRequestCodeStart> {
        block_on_result(pair_nearby_request_code(endpoint, host, port))
    }

    fn pair_nearby_submit_code_blocking(
        endpoint: &str,
        request_id: String,
        code: String,
        verification_nonce: String,
        host: String,
        port: u16,
        alias: Option<String>,
    ) -> Result<String> {
        block_on_result(pair_nearby_submit_code(
            endpoint,
            request_id,
            code,
            verification_nonce,
            host,
            port,
            alias,
        ))
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

    async fn pair_nearby_request_code(
        endpoint: &str,
        host: String,
        port: u16,
    ) -> Result<NearbyRequestCodeStart> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .request_nearby_pairing_code(NearbyRequestCodeStartRequest {
                host,
                port: u32::from(port),
                alias: String::new(),
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
        request_id: String,
        code: String,
        verification_nonce: String,
        host: String,
        port: u16,
        alias: Option<String>,
    ) -> Result<String> {
        let mut client = connect_control_plane(endpoint).await?;
        let response = client
            .submit_nearby_pairing_code(NearbySubmitCodeRequest {
                host,
                port: u32::from(port),
                request_id: request_id.clone(),
                code,
                verification_nonce,
                alias: alias.unwrap_or_default(),
            })
            .await?
            .into_inner();

        if !response.ok {
            bail!("nearby pairing failed: {}", response.message);
        }
        if response.request_id != request_id {
            bail!("nearby pairing request id mismatch");
        }

        Ok(response.peer_machine_id)
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
            return format!(
                "{message}\n\nThe remote pairing service stalled.\nRetry pairing. If this repeats, restart the target daemon and tray."
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
        fn resolve_discovered_peer_supports_index_and_prefix() {
            let peers = vec![
                UiDiscoveredPeer {
                    machine_id: "machine-alpha-1234".to_string(),
                    display_name: "Office Desktop".to_string(),
                    endpoint: "10.10.0.10:15100".to_string(),
                },
                UiDiscoveredPeer {
                    machine_id: "machine-bravo-5678".to_string(),
                    display_name: "Living Room".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
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
                },
                UiDiscoveredPeer {
                    machine_id: "machine-beta-5678".to_string(),
                    display_name: "Office Laptop".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
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
