use std::{path::Path, sync::Arc};

use anyhow::Result;
use app_services::{
    ControlPlaneApp, SharedControlPlaneApp,
    commands::{
        DiagnosticsDumpCommand, DiagnosticsDumpReply, FeatureSetCommand, HotkeySetCommand,
        HotkeyTriggerCommand, ImportTrustBundleCommand, InputCaptureTargetCommand,
        InputCaptureTargetReply, InputOwnerCommand, InputOwnerReply, LayoutReply, LayoutSetCommand,
        NearbyJoinStartCommand, NearbyJoinStatusCommand, NearbyPairingDecisionCommand,
        NearbyRequestCodeCommand, NearbySubmitCodeCommand, OperationReply, PairJoinCommand,
        PairJoinReply, PairingCodeReply, PairingCodeRequest, RemovePeerCommand, SafeResetCommand,
        SendClipboardImageCommand, SendClipboardTextCommand, SendFileCommand, SendInputKeyCommand,
        SendInputMoveCommand, SetAntiIdleConfigCommand,
    },
    queries::{
        AntiIdleConfigSnapshot, AntiIdleStatusSnapshot, ConsoleSnapshot, NearbyJoinStatusSnapshot,
        NearbyPairingCompletionSnapshot, NearbyRequestCodeStartSnapshot, StatusSnapshot,
        TransportEventSnapshot, TrustBundleSnapshot, UiDiscoveredPeer, UiPairedPeer,
        UiPendingRequest, UiSnapshot,
    },
};
use async_trait::async_trait;
use core_security::TrustBundle;

use crate::{config::ApiTransport, pairing_wire, state::AppState};

#[derive(Clone)]
pub struct DaemonControlPlaneApp {
    state: AppState,
}

impl DaemonControlPlaneApp {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

pub fn shared_control_plane_app(state: AppState) -> SharedControlPlaneApp {
    Arc::new(DaemonControlPlaneApp::new(state))
}

#[async_trait]
impl ControlPlaneApp for DaemonControlPlaneApp {
    async fn status_snapshot(&self) -> Result<StatusSnapshot> {
        build_status_snapshot(&self.state).await
    }

    async fn ui_snapshot(&self) -> Result<UiSnapshot> {
        build_ui_snapshot(&self.state).await
    }

    async fn console_snapshot(&self) -> Result<ConsoleSnapshot> {
        build_console_snapshot(&self.state).await
    }

    async fn create_pairing_code(&self, request: PairingCodeRequest) -> Result<PairingCodeReply> {
        let (code, expires_at) = self
            .state
            .create_pairing_code(request.ttl_seconds.max(1))
            .await;
        Ok(PairingCodeReply {
            code,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    async fn join_with_pairing_code(&self, command: PairJoinCommand) -> Result<PairJoinReply> {
        let peer_id = self
            .state
            .join_peer(command.code, command.host, command.alias)
            .await?;

        Ok(PairJoinReply {
            accepted: true,
            peer_id,
            message: "paired".to_string(),
        })
    }

    async fn set_layout(&self, command: LayoutSetCommand) -> Result<OperationReply> {
        self.state.set_layout(command.matrix_spec).await?;
        Ok(OperationReply {
            ok: true,
            message: "Layout updated".to_string(),
        })
    }

    async fn list_peers(&self) -> Result<Vec<UiPairedPeer>> {
        let bundle = self.state.control_plane_snapshot_bundle().await;
        Ok(build_paired_peers(&bundle))
    }

    async fn remove_peer(&self, command: RemovePeerCommand) -> Result<OperationReply> {
        let removed = self.state.remove_peer(&command.peer_id).await?;
        Ok(OperationReply {
            ok: removed,
            message: if removed {
                format!("Removed peer {}", command.peer_id)
            } else {
                format!("Peer {} not found", command.peer_id)
            },
        })
    }

    async fn layout(&self) -> Result<LayoutReply> {
        Ok(LayoutReply {
            matrix_spec: self.state.layout().await,
        })
    }

    async fn features(&self) -> Result<std::collections::BTreeMap<String, bool>> {
        Ok(self.state.feature_map().await)
    }

    async fn set_feature(&self, command: FeatureSetCommand) -> Result<OperationReply> {
        self.state
            .set_feature(command.name.clone(), command.enabled)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!("{}={}", command.name, command.enabled),
        })
    }

    async fn anti_idle_config(&self) -> Result<AntiIdleConfigSnapshot> {
        Ok(build_anti_idle_config_snapshot(
            self.state.anti_idle_config().await,
        ))
    }

    async fn anti_idle_status(&self) -> Result<AntiIdleStatusSnapshot> {
        Ok(build_anti_idle_status_snapshot(
            self.state.anti_idle_runtime_state().await,
        ))
    }

    async fn set_anti_idle_config(
        &self,
        command: SetAntiIdleConfigCommand,
    ) -> Result<OperationReply> {
        self.state
            .set_anti_idle_config_values(
                command.enabled,
                command.recent_activity_window_secs,
                command.allow_on_battery,
                command.keep_display_on,
            )
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!(
                "anti_idle enabled={} recent_activity_window_secs={} allow_on_battery={} keep_display_on={}",
                command.enabled,
                command.recent_activity_window_secs,
                command.allow_on_battery,
                command.keep_display_on
            ),
        })
    }

    async fn set_hotkey(&self, command: HotkeySetCommand) -> Result<OperationReply> {
        self.state
            .set_hotkey(command.action.clone(), command.combo.clone())
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!("hotkey {}={}", command.action, command.combo),
        })
    }

    async fn trigger_hotkey_action(&self, command: HotkeyTriggerCommand) -> Result<OperationReply> {
        let action_name =
            crate::hotkeys::trigger_action_for_diagnostics(&self.state, &command.action).await?;
        Ok(OperationReply {
            ok: true,
            message: format!("hotkey action {action_name} triggered"),
        })
    }

    async fn export_trust_bundle(&self) -> Result<TrustBundleSnapshot> {
        let bundle = self.state.export_trust_bundle().await?;
        Ok(TrustBundleSnapshot {
            machine_id: bundle.machine_id,
            display_name: bundle.display_name,
            network_address: bundle.network_address,
            ca_cert_pem: bundle.ca_cert_pem,
        })
    }

    async fn import_trust_bundle(
        &self,
        command: ImportTrustBundleCommand,
    ) -> Result<OperationReply> {
        self.state
            .import_trust_bundle(
                TrustBundle {
                    machine_id: command.machine_id,
                    display_name: command.display_name,
                    network_address: command.network_address,
                    ca_cert_pem: command.ca_cert_pem,
                },
                command.alias,
            )
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "trust bundle imported".to_string(),
        })
    }

    async fn dump_diagnostics(
        &self,
        command: DiagnosticsDumpCommand,
    ) -> Result<DiagnosticsDumpReply> {
        let bundle_path = self.state.diagnostics_dump(command.output_path).await?;
        Ok(DiagnosticsDumpReply { bundle_path })
    }

    async fn safe_reset(&self, command: SafeResetCommand) -> Result<OperationReply> {
        self.state
            .safe_reset(command.network_only, command.all)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "reset complete".to_string(),
        })
    }

    async fn send_clipboard_text(
        &self,
        command: SendClipboardTextCommand,
    ) -> Result<OperationReply> {
        self.state
            .queue_clipboard_text(&command.peer_id, command.text)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "clipboard payload queued".to_string(),
        })
    }

    async fn send_clipboard_image(
        &self,
        command: SendClipboardImageCommand,
    ) -> Result<OperationReply> {
        self.state
            .queue_clipboard_image(&command.peer_id, command.image_bmp)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "clipboard image payload queued".to_string(),
        })
    }

    async fn send_file(&self, command: SendFileCommand) -> Result<OperationReply> {
        self.state
            .queue_file_from_path(&command.peer_id, Path::new(&command.file_path))
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "file payload queued".to_string(),
        })
    }

    async fn send_input_move(&self, command: SendInputMoveCommand) -> Result<OperationReply> {
        self.state
            .queue_input_move(&command.peer_id, command.dx, command.dy)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "input move frame queued".to_string(),
        })
    }

    async fn send_input_key(&self, command: SendInputKeyCommand) -> Result<OperationReply> {
        let key_state = if command.key_down {
            core_input::KeyState::Down
        } else {
            core_input::KeyState::Up
        };
        self.state
            .queue_input_key(&command.peer_id, command.scan_code, key_state)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "input key frame queued".to_string(),
        })
    }

    async fn transport_events(&self) -> Result<Vec<TransportEventSnapshot>> {
        Ok(self
            .state
            .transport_events()
            .await
            .into_iter()
            .map(|event| TransportEventSnapshot {
                timestamp: event.timestamp.to_rfc3339(),
                direction: event.direction,
                kind: event.kind,
                peer_id: event.peer_id,
                detail: event.detail,
                size_bytes: event.size_bytes,
            })
            .collect())
    }

    async fn input_owner(&self) -> Result<InputOwnerReply> {
        Ok(InputOwnerReply {
            ok: true,
            owner_peer_id: self.state.input_owner().await.unwrap_or_default(),
            message: "input owner fetched".to_string(),
        })
    }

    async fn claim_input_owner(&self, command: InputOwnerCommand) -> Result<InputOwnerReply> {
        let acquired = self
            .state
            .claim_input_owner(&command.peer_id, command.force)
            .await?;
        let owner = self.state.input_owner().await.unwrap_or_default();
        Ok(InputOwnerReply {
            ok: acquired,
            owner_peer_id: owner.clone(),
            message: if acquired {
                format!("input owner set to {owner}")
            } else {
                format!("input owner remains {owner}")
            },
        })
    }

    async fn release_input_owner(&self, command: InputOwnerCommand) -> Result<InputOwnerReply> {
        let released = self.state.release_input_owner(&command.peer_id).await;
        Ok(InputOwnerReply {
            ok: released,
            owner_peer_id: self.state.input_owner().await.unwrap_or_default(),
            message: if released {
                "input owner released".to_string()
            } else {
                "peer did not hold input owner".to_string()
            },
        })
    }

    async fn input_capture_target(&self) -> Result<InputCaptureTargetReply> {
        Ok(InputCaptureTargetReply {
            ok: true,
            peer_id: self.state.input_capture_target().await.unwrap_or_default(),
            message: "input capture target fetched".to_string(),
        })
    }

    async fn set_input_capture_target(
        &self,
        command: InputCaptureTargetCommand,
    ) -> Result<InputCaptureTargetReply> {
        let target = self
            .state
            .set_input_capture_target(Some(&command.peer_id))
            .await?;
        let peer_id = target.unwrap_or_default();
        Ok(InputCaptureTargetReply {
            ok: true,
            peer_id: peer_id.clone(),
            message: if peer_id.is_empty() {
                "input capture target cleared".to_string()
            } else {
                format!("input capture target set to {peer_id}")
            },
        })
    }

    async fn clear_input_capture_target(&self) -> Result<InputCaptureTargetReply> {
        self.state.clear_input_capture_target().await;
        Ok(InputCaptureTargetReply {
            ok: true,
            peer_id: String::new(),
            message: "input capture target cleared".to_string(),
        })
    }

    async fn request_nearby_pairing_code(
        &self,
        command: NearbyRequestCodeCommand,
    ) -> Result<NearbyRequestCodeStartSnapshot> {
        let result = pairing_wire::request_nearby_pairing_code(
            &self.state,
            &command.host,
            command.port,
            command.alias,
        )
        .await?;

        Ok(match result {
            pairing_wire::NearbyRequestCodeStart::CodeRequired {
                request_id,
                verification_nonce,
                expires_at,
            } => NearbyRequestCodeStartSnapshot {
                code_required: true,
                request_id,
                verification_nonce,
                verification_expires_at: expires_at,
                unsupported: false,
                message: "enter code shown on target machine".to_string(),
            },
            pairing_wire::NearbyRequestCodeStart::Unsupported { reason } => {
                NearbyRequestCodeStartSnapshot {
                    code_required: false,
                    request_id: String::new(),
                    verification_nonce: String::new(),
                    verification_expires_at: String::new(),
                    unsupported: true,
                    message: reason,
                }
            }
        })
    }

    async fn submit_nearby_pairing_code(
        &self,
        command: NearbySubmitCodeCommand,
    ) -> Result<NearbyPairingCompletionSnapshot> {
        let request_id = command.request_id.clone();
        let peer_machine_id = pairing_wire::submit_nearby_pairing_code(
            &self.state,
            &command.host,
            command.port,
            command.request_id,
            command.code,
            command.verification_nonce,
            command.alias,
        )
        .await?;

        Ok(NearbyPairingCompletionSnapshot {
            ok: true,
            message: "nearby pairing complete".to_string(),
            request_id,
            peer_machine_id,
        })
    }

    async fn start_nearby_pairing_join(
        &self,
        command: NearbyJoinStartCommand,
    ) -> Result<NearbyJoinStatusSnapshot> {
        let result = pairing_wire::start_nearby_pairing_join(
            &self.state,
            &command.host,
            command.port,
            command.code,
            command.alias,
        )
        .await?;
        Ok(map_nearby_join_status(result))
    }

    async fn check_nearby_pairing_join(
        &self,
        command: NearbyJoinStatusCommand,
    ) -> Result<NearbyJoinStatusSnapshot> {
        let result = pairing_wire::check_nearby_pairing_join(
            &self.state,
            &command.host,
            command.port,
            command.request_id,
            command.alias,
        )
        .await?;
        Ok(map_nearby_join_status(result))
    }

    async fn approve_nearby_pairing_request(
        &self,
        command: NearbyPairingDecisionCommand,
    ) -> Result<OperationReply> {
        self.state
            .approve_nearby_pairing_request(&command.request_id, command.alias)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: "nearby pairing request approved".to_string(),
        })
    }

    async fn reject_nearby_pairing_request(
        &self,
        command: NearbyPairingDecisionCommand,
    ) -> Result<OperationReply> {
        let rejected = self
            .state
            .reject_nearby_pairing_request(&command.request_id)
            .await;
        Ok(OperationReply {
            ok: rejected,
            message: if rejected {
                "nearby pairing request rejected".to_string()
            } else {
                "nearby pairing request not found".to_string()
            },
        })
    }
}

async fn build_status_snapshot(state: &AppState) -> Result<StatusSnapshot> {
    let bundle = state.control_plane_snapshot_bundle().await;
    Ok(build_status_snapshot_from_bundle(bundle))
}

fn build_status_snapshot_from_bundle(
    bundle: crate::state::ControlPlaneSnapshotBundle,
) -> StatusSnapshot {
    let effective_api_transport = bundle.config.api_transport.effective();
    let api_pipe_name = if matches!(effective_api_transport, ApiTransport::NamedPipe) {
        bundle.config.api_pipe_name
    } else {
        String::new()
    };

    StatusSnapshot {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        machine_id: bundle.config.machine_id,
        peer_count: bundle.peers.len() as u32,
        protocol_version: bundle.config.protocol_version,
        api_bind: bundle.config.api_bind,
        api_transport: effective_api_transport.as_str().to_string(),
        api_pipe_name,
        input_locked: bundle.input_locked,
        input_lock_supported: bundle.input_lock_supported,
        capture_target_peer_id: bundle.active_input_capture_target_peer_id,
        anti_idle_supported: bundle.anti_idle_runtime.supported,
        anti_idle_enabled: bundle.anti_idle_runtime.enabled,
        anti_idle_active: bundle.anti_idle_runtime.active,
        anti_idle_display_required: bundle.anti_idle_runtime.display_required,
    }
}

async fn build_ui_snapshot(state: &AppState) -> Result<UiSnapshot> {
    let bundle = state.control_plane_snapshot_bundle().await;
    let paired_peers = build_paired_peers(&bundle);
    let discovered_peers = build_discovered_peers(&bundle, &paired_peers);
    let pending_requests = build_pending_requests(&bundle);

    Ok(UiSnapshot {
        generated_at: chrono::Utc::now().to_rfc3339(),
        daemon_online: true,
        machine_id: bundle.config.machine_id,
        layout_matrix: bundle.layout_matrix,
        discovered_peers,
        paired_peers,
        pending_requests,
        anti_idle_config: build_anti_idle_config_snapshot(bundle.anti_idle_config),
        anti_idle_status: build_anti_idle_status_snapshot(bundle.anti_idle_runtime),
    })
}

async fn build_console_snapshot(state: &AppState) -> Result<ConsoleSnapshot> {
    let bundle = state.control_plane_snapshot_bundle().await;
    Ok(build_console_snapshot_from_bundle(bundle))
}

fn build_console_snapshot_from_bundle(
    bundle: crate::state::ControlPlaneSnapshotBundle,
) -> ConsoleSnapshot {
    let status = build_status_snapshot_from_bundle(bundle.clone());
    let paired_peers = build_paired_peers(&bundle);
    let discovered_peers = build_discovered_peers(&bundle, &paired_peers);
    let pending_requests = build_pending_requests(&bundle);
    let features = bundle.features.clone();

    ConsoleSnapshot {
        status,
        layout_matrix: bundle.layout_matrix,
        peers: paired_peers.clone(),
        features,
        discovered_peers,
        pending_requests,
        transport_events: bundle
            .transport_events
            .into_iter()
            .map(|event| TransportEventSnapshot {
                timestamp: event.timestamp.to_rfc3339(),
                direction: event.direction,
                kind: event.kind,
                peer_id: event.peer_id,
                detail: event.detail,
                size_bytes: event.size_bytes,
            })
            .collect(),
        input_owner_peer_id: bundle.input_owner_peer_id,
        input_capture_target_peer_id: bundle.input_capture_target_peer_id,
        mdns_active: bundle.mdns_active,
        local_display_name: bundle.config.device_name,
        anti_idle_config: build_anti_idle_config_snapshot(bundle.anti_idle_config),
        anti_idle_status: build_anti_idle_status_snapshot(bundle.anti_idle_runtime),
    }
}

fn build_anti_idle_config_snapshot(
    config: crate::config::AntiIdleConfig,
) -> AntiIdleConfigSnapshot {
    AntiIdleConfigSnapshot {
        enabled: config.enabled,
        recent_activity_window_secs: config.recent_activity_window_secs,
        allow_on_battery: config.allow_on_battery,
        keep_display_on: config.keep_display_on,
    }
}

fn build_anti_idle_status_snapshot(
    runtime: crate::state::AntiIdleRuntimeState,
) -> AntiIdleStatusSnapshot {
    AntiIdleStatusSnapshot {
        supported: runtime.supported,
        enabled: runtime.enabled,
        active: runtime.active,
        display_required: runtime.display_required,
        reason: runtime.reason.as_str().to_string(),
    }
}

fn build_paired_peers(bundle: &crate::state::ControlPlaneSnapshotBundle) -> Vec<UiPairedPeer> {
    bundle
        .peers
        .clone()
        .into_iter()
        .map(|peer| UiPairedPeer {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            address: peer.address,
            connected: peer.connected,
        })
        .collect()
}

fn build_discovered_peers(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
    paired_peers: &[UiPairedPeer],
) -> Vec<UiDiscoveredPeer> {
    let local_machine_id = bundle.config.machine_id.clone();
    let paired_peer_ids = paired_peers
        .iter()
        .map(|peer| peer.peer_id.clone())
        .collect::<Vec<_>>();

    let mut discovered_peers = bundle
        .discovered_endpoints
        .clone()
        .into_iter()
        .filter(|(machine_id, _)| {
            machine_id != &local_machine_id
                && !paired_peer_ids.iter().any(|peer_id| peer_id == machine_id)
        })
        .map(|(machine_id, peer)| UiDiscoveredPeer {
            machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint.to_string(),
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

fn build_pending_requests(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
) -> Vec<UiPendingRequest> {
    let mut pending_requests = bundle
        .pending_requests
        .clone()
        .into_iter()
        .map(|request| {
            let requires_verification_code = request.verification_code.is_some();
            UiPendingRequest {
                request_id: request.request_id,
                requester_machine_id: request.requester_machine_id,
                requester_display_name: request.requester_display_name,
                created_at: request.created_at.to_rfc3339(),
                verification_code: request.verification_code.unwrap_or_default(),
                verification_expires_at: request
                    .verification_expires_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_default(),
                requires_verification_code,
            }
        })
        .collect::<Vec<_>>();
    pending_requests.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    pending_requests
}

fn map_nearby_join_status(result: pairing_wire::NearbyJoinStatus) -> NearbyJoinStatusSnapshot {
    NearbyJoinStatusSnapshot {
        request_id: result.request_id,
        status: result.status.as_str().to_string(),
        message: result.message,
        peer_machine_id: result.peer_machine_id,
    }
}
