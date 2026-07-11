use std::{path::Path, sync::Arc};

use anyhow::Result;
use app_services::{
    ControlPlaneApp, SharedControlPlaneApp,
    commands::{
        ClipboardBrokerExchangeCommand, DiagnosticsDumpCommand, DiagnosticsDumpReply,
        FeatureSetCommand, FileTransferActionCommand, HotkeySetCommand, HotkeyTriggerCommand,
        ImportTrustBundleCommand, InputBrokerAttachCommand, InputBrokerDetachCommand,
        InputBrokerExchangeCommand, InputCaptureTargetCommand, InputCaptureTargetReply,
        InputOwnerCommand, InputOwnerReply, LayoutReply, LayoutSetCommand, NearbyJoinStartCommand,
        NearbyJoinStatusCommand, NearbyPairingDecisionCommand, NearbyRequestCodeCommand,
        NearbySubmitCodeCommand, OperationReply, PairJoinCommand, PairJoinReply, PairingCodeReply,
        PairingCodeRequest, RemovePeerCommand, RotateTrustCommand, SafeResetCommand,
        SendClipboardImageCommand, SendClipboardTextCommand, SendFileCommand, SendInputKeyCommand,
        SendInputMoveCommand, SetAntiIdleConfigCommand, SetFileTransferConfigCommand,
        SetInputHandoffConfigCommand,
    },
    diagnostics::{
        DiagnosticExportOptions, ServiceDiagnosticSnapshot, build_online_bundle,
        write_diagnostic_bundle,
    },
    queries::{
        AntiIdleConfigSnapshot, AntiIdleStatusSnapshot, ClipboardBrokerExchangeSnapshot,
        ClipboardBrokerLocalPayloadDispositionSnapshot, ClipboardRuntimeSnapshot, ConsoleSnapshot,
        FileTransferConfigSnapshot, FileTransferSnapshot, InputBrokerAttachSnapshot,
        InputBrokerExchangeSnapshot, InputBrokerInjectFrameSnapshot, InputHandoffConfigSnapshot,
        InputRuntimeSnapshot, NearbyJoinStatusSnapshot, NearbyPairingCompletionSnapshot,
        NearbyRequestCodeStartSnapshot, StatusSnapshot, TransportEventSnapshot,
        TrustBundleSnapshot, UiDiscoveredPeer, UiPairedPeer, UiPendingRequest, UiSnapshot,
    },
};
use async_trait::async_trait;
use core_clipboard::sanitize_clipboard_event_output_detail;
use core_security::{TrustBundle, fingerprint};
use serde_json::Value;

use crate::{
    config::{ApiTransport, FileTransferConfig, InputHandoffConfig},
    pairing_wire,
    runtime_tasks::{RuntimeTaskSnapshot, task_health_json},
    state::AppState,
};

#[cfg(windows)]
use app_services::diagnostics::{
    extract_service_executable_path, service_binary_manifest_version, service_version_parity,
};

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
        let synced_peers = self
            .state
            .set_layout_and_queue_sync(command.matrix_spec)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!("Layout updated; synced_peers={synced_peers}"),
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

    async fn file_transfer_config(&self) -> Result<FileTransferConfigSnapshot> {
        Ok(build_file_transfer_config_snapshot(
            self.state.file_transfer_config().await,
        ))
    }

    async fn set_file_transfer_config(
        &self,
        command: SetFileTransferConfigCommand,
    ) -> Result<OperationReply> {
        let current = self.state.file_transfer_config().await;
        let max_file_bytes = if command.max_file_bytes == 0 {
            current.max_file_bytes
        } else {
            command.max_file_bytes
        };
        self.state
            .update_file_transfer_config(FileTransferConfig {
                receive_dir: command.receive_dir.clone(),
                organize_by_peer: command.organize_by_peer,
                auto_accept_trusted_peers: command.auto_accept_trusted_peers,
                max_file_bytes,
            })
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!(
                "file_transfer receive_dir={} organize_by_peer={} auto_accept_trusted_peers={} max_file_bytes={}",
                command.receive_dir,
                command.organize_by_peer,
                command.auto_accept_trusted_peers,
                max_file_bytes
            ),
        })
    }

    async fn cancel_file_transfer(
        &self,
        command: FileTransferActionCommand,
    ) -> Result<OperationReply> {
        let cancelled = self
            .state
            .cancel_file_transfer_by_id(&command.transfer_id, "user_cancelled")
            .await;
        Ok(OperationReply {
            ok: cancelled,
            message: if cancelled {
                format!("cancelled transfer {}", command.transfer_id)
            } else {
                format!("transfer {} not cancellable", command.transfer_id)
            },
        })
    }

    async fn retry_file_transfer(
        &self,
        command: FileTransferActionCommand,
    ) -> Result<OperationReply> {
        let new_transfer_id = self
            .state
            .retry_file_transfer_from_beginning(&command.transfer_id)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!(
                "queued retry {new_transfer_id} for transfer {}",
                command.transfer_id
            ),
        })
    }

    async fn clear_completed_file_transfers(&self) -> Result<OperationReply> {
        let removed = self.state.clear_completed_file_transfers().await;
        Ok(OperationReply {
            ok: true,
            message: format!("cleared {removed} completed transfer entries"),
        })
    }

    async fn set_input_handoff_config(
        &self,
        command: SetInputHandoffConfigCommand,
    ) -> Result<OperationReply> {
        self.state
            .update_input_handoff_config(InputHandoffConfig {
                block_screen_corners: command.block_screen_corners,
                corner_block_px: command.corner_block_px,
                relative_mouse: command.relative_mouse,
                hide_cursor_at_edge: command.hide_cursor_at_edge,
                draw_cursor_marker: command.draw_cursor_marker,
            })
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!(
                "input_handoff block_screen_corners={} corner_block_px={} relative_mouse={} hide_cursor_at_edge={} draw_cursor_marker={}",
                command.block_screen_corners,
                command.corner_block_px,
                command.relative_mouse,
                command.hide_cursor_at_edge,
                command.draw_cursor_marker
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

    async fn rotate_trust(&self, command: RotateTrustCommand) -> Result<OperationReply> {
        let machine_id = self.state.snapshot().await.machine_id;
        let expected = format!("rotate-trust:{machine_id}");
        if command.confirm != expected {
            anyhow::bail!("typed confirmation required: --confirm {expected}");
        }
        let message = self.state.rotate_trust().await?;
        Ok(OperationReply { ok: true, message })
    }

    async fn dump_diagnostics(
        &self,
        command: DiagnosticsDumpCommand,
    ) -> Result<DiagnosticsDumpReply> {
        let snapshot = build_console_snapshot(&self.state).await?;
        let mut bundle = build_online_bundle(
            snapshot,
            collect_service_diagnostics(),
            command.include_filenames,
        );
        insert_runtime_task_health(&mut bundle, self.state.runtime_task_snapshots());
        let export = write_diagnostic_bundle(
            bundle,
            DiagnosticExportOptions {
                output_path: command.output_path,
                include_filenames: command.include_filenames,
            },
        )
        .await?;
        Ok(DiagnosticsDumpReply {
            bundle_path: export.bundle_path,
            manifest_path: export.manifest_path,
            filenames_included: export.filenames_included,
        })
    }

    async fn safe_reset(&self, command: SafeResetCommand) -> Result<OperationReply> {
        let machine_id = self.state.snapshot().await.machine_id;
        let expected = if command.all {
            format!("safe-reset-all:{machine_id}")
        } else if command.network_only {
            format!("safe-reset-network:{machine_id}")
        } else {
            format!("safe-reset-runtime:{machine_id}")
        };
        if command.confirm != expected {
            anyhow::bail!("typed confirmation required: --confirm {expected}");
        }
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
        let transfer_id = self
            .state
            .queue_file_from_path(&command.peer_id, Path::new(&command.file_path))
            .await?;
        Ok(OperationReply {
            ok: true,
            message: format!("file transfer queued: {transfer_id}"),
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
            .map(|event| {
                let detail = sanitize_clipboard_event_output_detail(&event.kind, &event.detail);
                TransportEventSnapshot {
                    timestamp: event.timestamp.to_rfc3339(),
                    direction: event.direction,
                    kind: event.kind,
                    peer_id: event.peer_id,
                    detail,
                    size_bytes: event.size_bytes,
                }
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

    async fn attach_input_broker(
        &self,
        command: InputBrokerAttachCommand,
    ) -> Result<InputBrokerAttachSnapshot> {
        let outcome = self
            .state
            .attach_input_broker_versioned(
                broker_client_identity(command.verified_client),
                command.broker_version,
                command.lock_supported,
                command.protocol_revision,
            )
            .await;
        Ok(InputBrokerAttachSnapshot {
            accepted: outcome.accepted,
            broker_token: outcome.broker_token,
            message: outcome.message,
            protocol_revision: outcome.protocol_revision,
            delivery_epoch: outcome.delivery_epoch,
        })
    }

    async fn exchange_input_broker(
        &self,
        command: InputBrokerExchangeCommand,
    ) -> Result<InputBrokerExchangeSnapshot> {
        let outcome = self
            .state
            .exchange_input_broker(
                broker_client_identity(command.verified_client),
                &command.broker_token,
                crate::state::InputBrokerExchangeObservations {
                    captured_events: command.captured_events,
                    cursor: command.cursor,
                    virtual_bounds: command.virtual_bounds,
                    escape_unlock_count: command.escape_unlock_count,
                    lease_expired_unlock_count: command.lease_expired_unlock_count,
                    detector_unavailable_unlock_count: command.detector_unavailable_unlock_count,
                    handoff_probe: command.handoff_probe,
                    lock_active: command.lock_active,
                    dropped_event_count: command.dropped_event_count,
                    injected_frame_count: command.injected_frame_count,
                    inject_failure_count: command.inject_failure_count,
                    inject_backpressure: command.inject_backpressure,
                    acked_inject_batch_id: command.acked_inject_batch_id,
                    raw_device_wheel_event_count: command.raw_device_wheel_event_count,
                    raw_system_wheel_event_count: command.raw_system_wheel_event_count,
                    hook_wheel_event_count: command.hook_wheel_event_count,
                },
            )
            .await;
        Ok(InputBrokerExchangeSnapshot {
            accepted: outcome.accepted,
            message: outcome.message,
            inject_frames: outcome
                .inject_frames
                .into_iter()
                .map(|frame| InputBrokerInjectFrameSnapshot {
                    source_peer_id: frame.peer_id,
                    sequence: frame.sequence,
                    events: frame.events,
                })
                .collect(),
            lock_should_be_active: outcome.lock_should_be_active,
            capture_active: outcome.capture_active,
            capture_forwarding_authorized: outcome.capture_forwarding_authorized,
            inject_batch_id: outcome.inject_batch_id,
            inject_batch_cancelled: outcome.inject_batch_cancelled,
        })
    }

    async fn exchange_clipboard_broker(
        &self,
        command: ClipboardBrokerExchangeCommand,
    ) -> Result<ClipboardBrokerExchangeSnapshot> {
        let outcome = self
            .state
            .exchange_clipboard_broker(
                broker_client_identity(command.verified_client),
                &command.broker_token,
                command.local_payload,
                command.local_sequence,
                command
                    .apply_report
                    .map(|report| crate::state::ClipboardBrokerApplyReport {
                        source_peer_id: report.source_peer_id,
                        hash: report.hash,
                        applied: report.applied,
                        message: report.message,
                    }),
            )
            .await;

        let (remote_payload, remote_source_peer_id, remote_hash) =
            if let Some(remote) = outcome.remote_payload {
                (Some(remote.payload), remote.peer_id, remote.hash)
            } else {
                (None, String::new(), String::new())
            };

        Ok(ClipboardBrokerExchangeSnapshot {
            accepted: outcome.accepted,
            message: outcome.message,
            remote_payload,
            remote_source_peer_id,
            remote_hash,
            local_payload_disposition: match outcome.local_payload_disposition {
                crate::state::ClipboardBrokerLocalPayloadDisposition::NotSubmitted => {
                    ClipboardBrokerLocalPayloadDispositionSnapshot::NotSubmitted
                }
                crate::state::ClipboardBrokerLocalPayloadDisposition::Accepted => {
                    ClipboardBrokerLocalPayloadDispositionSnapshot::Accepted
                }
                crate::state::ClipboardBrokerLocalPayloadDisposition::TransientRejected => {
                    ClipboardBrokerLocalPayloadDispositionSnapshot::TransientRejected
                }
                crate::state::ClipboardBrokerLocalPayloadDisposition::DeterministicRejected => {
                    ClipboardBrokerLocalPayloadDispositionSnapshot::DeterministicRejected
                }
            },
        })
    }

    async fn detach_input_broker(
        &self,
        command: InputBrokerDetachCommand,
    ) -> Result<OperationReply> {
        let detached = self
            .state
            .detach_input_broker(
                broker_client_identity(command.verified_client),
                &command.broker_token,
                &command.delivery_epoch,
                command.acked_inject_batch_id,
            )
            .await;
        Ok(OperationReply {
            ok: detached,
            message: if detached {
                "input broker detached".to_string()
            } else {
                "input broker token was not attached".to_string()
            },
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
            &command.endpoint_candidates,
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
        let outcome = pairing_wire::submit_nearby_pairing_code(
            &self.state,
            pairing_wire::NearbySubmitCode {
                host: command.host,
                port: command.port,
                request_id: command.request_id,
                code: command.code,
                verification_nonce: command.verification_nonce,
                alias: command.alias,
                endpoint_candidates: command.endpoint_candidates,
            },
        )
        .await?;

        Ok(NearbyPairingCompletionSnapshot {
            ok: true,
            message: outcome.message,
            request_id,
            peer_machine_id: outcome.peer_machine_id,
            trust_committed: outcome.trust_committed,
            already_committed: outcome.already_committed,
            reconnect_status: outcome.reconnect_status,
        })
    }

    async fn start_nearby_pairing_join(
        &self,
        command: NearbyJoinStartCommand,
    ) -> Result<NearbyJoinStatusSnapshot> {
        let result = pairing_wire::start_nearby_pairing_join_with_role(
            &self.state,
            pairing_wire::NearbyJoinAttempt {
                host: &command.host,
                port: command.port,
                code: command.code,
                alias: command.alias,
                endpoint_candidates: &command.endpoint_candidates,
                role: command.role,
                attempt_id: command.attempt_id,
            },
        )
        .await?;
        Ok(map_nearby_join_status(result))
    }

    async fn check_nearby_pairing_join(
        &self,
        command: NearbyJoinStatusCommand,
    ) -> Result<NearbyJoinStatusSnapshot> {
        let result = pairing_wire::check_nearby_pairing_join_with_role(
            &self.state,
            pairing_wire::NearbyJoinStatusAttempt {
                host: &command.host,
                port: command.port,
                request_id: command.request_id,
                alias: command.alias,
                endpoint_candidates: &command.endpoint_candidates,
                role: command.role,
                attempt_id: command.attempt_id,
            },
        )
        .await?;
        Ok(map_nearby_join_status(result))
    }

    async fn approve_nearby_pairing_request(
        &self,
        command: NearbyPairingDecisionCommand,
    ) -> Result<OperationReply> {
        let result = self
            .state
            .approve_nearby_pairing_request(&command.request_id, command.alias)
            .await?;
        Ok(OperationReply {
            ok: true,
            message: result.message,
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

fn broker_client_identity(
    verified_client: Option<app_services::commands::VerifiedControlClient>,
) -> Option<crate::state::InputBrokerClientIdentity> {
    verified_client.map(|client| crate::state::InputBrokerClientIdentity {
        user_sid: client.user_sid,
        session_id: client.session_id,
    })
}

fn insert_runtime_task_health(bundle: &mut Value, snapshots: Vec<RuntimeTaskSnapshot>) {
    let Some(component_health) = bundle
        .get_mut("component_health")
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    component_health.insert("runtime_tasks".to_string(), task_health_json(&snapshots));
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
    let input_runtime = build_input_runtime_snapshot(&bundle);
    let clipboard_runtime = build_clipboard_runtime_snapshot(&bundle);

    Ok(UiSnapshot {
        generated_at: chrono::Utc::now().to_rfc3339(),
        daemon_online: true,
        machine_id: bundle.config.machine_id.clone(),
        layout_matrix: bundle.layout_matrix,
        features: bundle.features.clone(),
        hotkeys: bundle.config.hotkeys.clone(),
        discovered_peers,
        paired_peers,
        pending_requests,
        anti_idle_config: build_anti_idle_config_snapshot(bundle.anti_idle_config.clone()),
        anti_idle_status: build_anti_idle_status_snapshot(bundle.anti_idle_runtime),
        input_handoff_config: build_input_handoff_config_snapshot(
            bundle.input_handoff_config.clone(),
        ),
        input_runtime,
        clipboard_runtime,
        file_transfer_config: build_file_transfer_config_snapshot(bundle.config.file_transfer),
        file_transfers: build_file_transfer_snapshots(bundle.file_transfers),
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
    let input_runtime = build_input_runtime_snapshot(&bundle);
    let clipboard_runtime = build_clipboard_runtime_snapshot(&bundle);

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
            .map(|event| {
                let detail = sanitize_clipboard_event_output_detail(&event.kind, &event.detail);
                TransportEventSnapshot {
                    timestamp: event.timestamp.to_rfc3339(),
                    direction: event.direction,
                    kind: event.kind,
                    peer_id: event.peer_id,
                    detail,
                    size_bytes: event.size_bytes,
                }
            })
            .collect(),
        input_owner_peer_id: bundle.input_owner_peer_id,
        input_capture_target_peer_id: bundle.input_capture_target_peer_id,
        mdns_active: bundle.mdns_active,
        local_display_name: bundle.config.device_name,
        anti_idle_config: build_anti_idle_config_snapshot(bundle.anti_idle_config.clone()),
        anti_idle_status: build_anti_idle_status_snapshot(bundle.anti_idle_runtime),
        input_handoff_config: build_input_handoff_config_snapshot(
            bundle.input_handoff_config.clone(),
        ),
        input_runtime,
        clipboard_runtime,
        file_transfer_config: build_file_transfer_config_snapshot(bundle.config.file_transfer),
        file_transfers: build_file_transfer_snapshots(bundle.file_transfers),
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

fn build_file_transfer_config_snapshot(
    config: crate::config::FileTransferConfig,
) -> FileTransferConfigSnapshot {
    FileTransferConfigSnapshot {
        receive_dir: config.receive_dir,
        organize_by_peer: config.organize_by_peer,
        auto_accept_trusted_peers: config.auto_accept_trusted_peers,
        max_file_bytes: config.max_file_bytes,
    }
}

fn build_file_transfer_snapshots(
    records: Vec<crate::state::FileTransferRecord>,
) -> Vec<FileTransferSnapshot> {
    records
        .into_iter()
        .map(|record| FileTransferSnapshot {
            transfer_id: record.transfer_id,
            previous_transfer_id: record.previous_transfer_id,
            direction: record.direction.as_str().to_string(),
            peer_id: record.peer_id,
            file_name: record.file_name,
            state: record.state.as_str().to_string(),
            transferred_bytes: record.transferred_bytes,
            total_bytes: record.total_bytes,
            failure_reason: record.failure_reason,
            source_path: record.source_path.map(|path| path.display().to_string()),
            final_path: record.final_path.map(|path| path.display().to_string()),
            queued_at: record.queued_at.to_rfc3339(),
            updated_at: record.updated_at.to_rfc3339(),
        })
        .collect()
}

fn build_input_handoff_config_snapshot(
    config: crate::config::InputHandoffConfig,
) -> InputHandoffConfigSnapshot {
    InputHandoffConfigSnapshot {
        block_screen_corners: config.block_screen_corners,
        corner_block_px: config.corner_block_px,
        relative_mouse: config.relative_mouse,
        hide_cursor_at_edge: config.hide_cursor_at_edge,
        draw_cursor_marker: config.draw_cursor_marker,
    }
}

fn build_input_runtime_snapshot(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
) -> InputRuntimeSnapshot {
    InputRuntimeSnapshot {
        owner_peer_id: bundle.input_owner_peer_id.clone(),
        configured_capture_target_peer_id: bundle.input_capture_target_peer_id.clone(),
        active_capture_target_peer_id: bundle.active_input_capture_target_peer_id.clone(),
        lock_active: bundle.input_locked,
        lock_supported: bundle.input_lock_supported,
        capture_backend_mode: bundle.input_capture_backend_mode.clone(),
        pending_inject_frames: bundle.pending_inject_frames,
        pending_inject_high_water: bundle.pending_inject_high_water,
    }
}

fn build_clipboard_runtime_snapshot(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
) -> ClipboardRuntimeSnapshot {
    ClipboardRuntimeSnapshot {
        backend_mode: bundle.clipboard_backend_mode.clone(),
    }
}

fn build_paired_peers(bundle: &crate::state::ControlPlaneSnapshotBundle) -> Vec<UiPairedPeer> {
    bundle
        .peers
        .clone()
        .into_iter()
        .map(|peer| {
            let trust_record = bundle
                .trusted_records
                .iter()
                .find(|record| record.machine_id == peer.peer_id);
            UiPairedPeer {
                health_state: peer_health_state(bundle, &peer),
                health_reason: peer_health_reason(bundle, &peer),
                trust_state: if trust_record.is_some() {
                    "trusted".to_string()
                } else {
                    "missing_trust".to_string()
                },
                trusted_since: trust_record
                    .map(|record| record.added_at.to_rfc3339())
                    .unwrap_or_default(),
                trust_fingerprint: trust_record
                    .map(|record| fingerprint(&record.ca_cert_pem))
                    .unwrap_or_default(),
                device_identity: peer.peer_id.clone(),
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                address: peer.address,
                connected: peer.connected,
            }
        })
        .collect()
}

fn peer_health_state(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
    peer: &crate::config::PeerConfig,
) -> String {
    if peer.connected {
        return "connected".to_string();
    }
    peer_health_event(bundle, peer)
        .map(|event| match event.kind.as_str() {
            "pairing_connectivity_pending" | "pairing_reconnect_failed" => "connectivity_pending",
            "peer_reconnect_requested" | "peers_reconnect_requested" => "reconnecting",
            "transport_trust_error" => "trust_error",
            "transport_protocol_mismatch" => "protocol_mismatch",
            "transport_reachability_failed" => "reachability_failed",
            "transport_firewall_suspect" => "firewall_suspect",
            "transport_service_issue" => "service_issue",
            _ => "disconnected",
        })
        .unwrap_or("disconnected")
        .to_string()
}

fn peer_health_reason(
    bundle: &crate::state::ControlPlaneSnapshotBundle,
    peer: &crate::config::PeerConfig,
) -> String {
    if peer.connected {
        return "peer is connected".to_string();
    }
    peer_health_event(bundle, peer)
        .map(|event| match event.kind.as_str() {
            "pairing_connectivity_pending" => {
                "trust established; waiting for connectivity".to_string()
            }
            "pairing_reconnect_failed" => {
                "trust established; reconnect request failed; use reconnect or remove peer to re-pair"
                    .to_string()
            }
            "peer_reconnect_requested" | "peers_reconnect_requested" => {
                "manual or automatic reconnect requested".to_string()
            }
            "transport_trust_error" => "transport reported a trust failure".to_string(),
            "transport_protocol_mismatch" => "transport reported a protocol mismatch".to_string(),
            "transport_reachability_failed" => {
                format!("transport reachability failed; {}", event.detail)
            }
            "transport_firewall_suspect" => {
                "transport reported a firewall or reachability suspect".to_string()
            }
            "transport_service_issue" => "service mode issue reported".to_string(),
            _ => "peer is disconnected; no classified transport failure is available".to_string(),
        })
        .unwrap_or_else(|| "no recent peer health event".to_string())
}

fn peer_health_event<'a>(
    bundle: &'a crate::state::ControlPlaneSnapshotBundle,
    peer: &crate::config::PeerConfig,
) -> Option<&'a crate::state::TransportEventRecord> {
    bundle.transport_events.iter().rev().find(|event| {
        (event.peer_id == peer.peer_id
            || (event.peer_id == "all" && event.kind == "peers_reconnect_requested"))
            && matches!(
                event.kind.as_str(),
                "peer_reconnect_requested"
                    | "pairing_connectivity_pending"
                    | "pairing_reconnect_failed"
                    | "peers_reconnect_requested"
                    | "transport_trust_error"
                    | "transport_protocol_mismatch"
                    | "transport_reachability_failed"
                    | "transport_firewall_suspect"
                    | "transport_service_issue"
            )
    })
}

const BOUNDLESS_SERVICE_NAME: &str = "BoundlessService";

#[cfg(windows)]
fn collect_service_diagnostics() -> ServiceDiagnosticSnapshot {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(error) => {
            return ServiceDiagnosticSnapshot {
                platform: "windows".to_string(),
                service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                installed: false,
                state: "unknown".to_string(),
                process_id: None,
                binary_path: None,
                service_version: "unknown".to_string(),
                service_version_source: "service_manager_unavailable".to_string(),
                current_version,
                version_parity: "unknown".to_string(),
                error: Some(error.to_string()),
            };
        }
    };

    match manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(service) => {
            let status = service.query_status();
            let config = service.query_config();
            match (status, config) {
                (Ok(status), Ok(config)) => {
                    let binary =
                        extract_service_executable_path(&config.executable_path.to_string_lossy());
                    let (service_version, version_source) =
                        service_binary_manifest_version(&binary);
                    let known_service_version =
                        (service_version != "unknown").then_some(service_version.as_str());
                    ServiceDiagnosticSnapshot {
                        platform: "windows".to_string(),
                        service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                        installed: true,
                        state: format!("{:?}", status.current_state),
                        process_id: status.process_id,
                        binary_path: Some(binary.display().to_string()),
                        version_parity: service_version_parity(
                            known_service_version,
                            &current_version,
                        )
                        .to_string(),
                        service_version,
                        service_version_source: version_source.to_string(),
                        current_version,
                        error: None,
                    }
                }
                (status, config) => ServiceDiagnosticSnapshot {
                    platform: "windows".to_string(),
                    service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                    installed: true,
                    state: "unknown".to_string(),
                    process_id: None,
                    binary_path: None,
                    service_version: "unknown".to_string(),
                    service_version_source: "query_failed".to_string(),
                    current_version,
                    version_parity: "unknown".to_string(),
                    error: Some(format!(
                        "status_error={} config_error={}",
                        status
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_default(),
                        config
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_default()
                    )),
                },
            }
        }
        Err(error) => ServiceDiagnosticSnapshot {
            platform: "windows".to_string(),
            service_name: BOUNDLESS_SERVICE_NAME.to_string(),
            installed: false,
            state: "not_installed".to_string(),
            process_id: None,
            binary_path: None,
            service_version: "unknown".to_string(),
            service_version_source: "not_installed".to_string(),
            current_version,
            version_parity: "not_installed".to_string(),
            error: Some(error.to_string()),
        },
    }
}

#[cfg(not(windows))]
fn collect_service_diagnostics() -> ServiceDiagnosticSnapshot {
    ServiceDiagnosticSnapshot {
        platform: "non-windows".to_string(),
        service_name: BOUNDLESS_SERVICE_NAME.to_string(),
        installed: false,
        state: "unsupported".to_string(),
        process_id: None,
        binary_path: None,
        service_version: "unknown".to_string(),
        service_version_source: "unsupported".to_string(),
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        version_parity: "unsupported".to_string(),
        error: None,
    }
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
            endpoint_candidates: peer
                .endpoint_candidates
                .into_iter()
                .map(|endpoint| endpoint.to_string())
                .collect(),
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
                role: request.role.as_str().to_string(),
                attempt_id: request.attempt_id.unwrap_or_default(),
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
        role: result.role.as_str().to_string(),
        attempt_id: result.attempt_id.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rotate_trust_requires_machine_typed_confirmation() {
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-rotate-trust-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let machine_id = state.snapshot().await.machine_id;
        let app = DaemonControlPlaneApp::new(state);

        let err = app
            .rotate_trust(RotateTrustCommand {
                confirm: "rotate-trust:wrong-machine".to_string(),
            })
            .await
            .expect_err("wrong confirmation should fail");
        assert!(
            err.to_string()
                .contains(&format!("--confirm rotate-trust:{machine_id}")),
            "error should include exact confirmation token"
        );

        let reply = app
            .rotate_trust(RotateTrustCommand {
                confirm: format!("rotate-trust:{machine_id}"),
            })
            .await
            .expect("matching confirmation should rotate trust");
        assert!(reply.ok);
        assert!(reply.message.contains("restart_required=true"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn safe_reset_requires_typed_confirmation() {
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-safe-reset-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let machine_id = state.snapshot().await.machine_id;
        let app = DaemonControlPlaneApp::new(state);

        let err = app
            .safe_reset(SafeResetCommand {
                network_only: true,
                all: false,
                confirm: "safe-reset-network:wrong-machine".to_string(),
            })
            .await
            .expect_err("wrong confirmation should fail");
        assert!(
            err.to_string()
                .contains(&format!("--confirm safe-reset-network:{machine_id}")),
            "error should include exact confirmation token"
        );

        let reply = app
            .safe_reset(SafeResetCommand {
                network_only: true,
                all: false,
                confirm: format!("safe-reset-network:{machine_id}"),
            })
            .await
            .expect("matching confirmation should reset network");
        assert!(reply.ok);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn diagnostics_dump_writes_redacted_json_bundle() {
        const CLIPBOARD_HASH_SENTINEL: &str = "BOUNDLESS_SECRET_SENTINEL_clipboard_hash_529c748a";
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-diagnostics-bundle-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let output_dir = root.join("diagnostics");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let (code, _) = state.create_pairing_code(120).await;
        let layout_peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("Office Display".to_string()),
            )
            .await
            .expect("join layout peer");
        state
            .set_layout(format!("self,{layout_peer_id}"))
            .await
            .expect("set raw peer-id layout");
        state.record_transport_event(crate::state::TransportEventRecord {
            timestamp: chrono::Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: "peer-alpha".to_string(),
            detail: "password=hunter2 token=abc123".to_string(),
            size_bytes: 29,
        });
        for received_bytes in 61..=64 {
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "incoming".to_string(),
                kind: "clipboard_image_rejected".to_string(),
                peer_id: "peer-alpha".to_string(),
                detail: format!(
                    "payload_type=bmp disposition=rejected reason=hash_mismatch received_bytes={received_bytes} expected={CLIPBOARD_HASH_SENTINEL} actual={CLIPBOARD_HASH_SENTINEL}"
                ),
                size_bytes: received_bytes,
            });
        }
        state.record_transport_event(crate::state::TransportEventRecord {
            timestamp: chrono::Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file_transfer_started".to_string(),
            peer_id: "peer-alpha".to_string(),
            detail: "transfer_id=file-123 file_name=C:\\Users\\Alice\\taxes.pdf total_bytes=9"
                .to_string(),
            size_bytes: 9,
        });
        state.spawn_runtime_task(
            crate::runtime_tasks::RuntimeTaskSpec::new(
                "network.supervisor",
                crate::runtime_tasks::RuntimeTaskOwner::Network,
                crate::runtime_tasks::RuntimeTaskShutdown::AbortOnDaemonShutdown,
            ),
            async {
                std::future::pending::<()>().await;
            },
        );

        let app = DaemonControlPlaneApp::new(state);
        let raw_events = app.transport_events().await.expect("raw transport events");
        let raw_rendered = format!("{raw_events:?}");
        assert!(!raw_rendered.contains(CLIPBOARD_HASH_SENTINEL));
        assert!(raw_rendered.contains("reason=hash_mismatch"));
        assert!(raw_rendered.contains("sample_count=4"));

        let reply = app
            .dump_diagnostics(DiagnosticsDumpCommand {
                output_path: Some(output_dir.to_string_lossy().to_string()),
                include_filenames: false,
            })
            .await
            .expect("dump diagnostics");
        let content = std::fs::read_to_string(&reply.bundle_path).expect("read bundle");
        let manifest = std::fs::read_to_string(&reply.manifest_path).expect("read manifest");

        assert!(content.contains(r#""mode": "online""#));
        assert!(content.contains(r#""layout_matrix": "self,peer-2""#));
        assert!(content.contains("metadata_only=true"));
        assert!(content.contains("[redacted-file-name]"));
        assert!(content.contains(r#""peer_id": "peer-1""#));
        assert!(content.contains(r#""runtime_tasks""#));
        assert!(content.contains(r#""clipboard_runtime""#));
        assert!(content.contains(r#""backend_mode": "direct""#));
        assert!(content.contains(r#""name": "network.supervisor""#));
        assert!(content.contains(r#""owner": "network""#));
        assert!(content.contains(r#""shutdown": "abort_on_daemon_shutdown""#));
        assert!(!content.contains("hunter2"));
        assert!(!content.contains("abc123"));
        assert!(!content.contains(CLIPBOARD_HASH_SENTINEL));
        assert!(content.contains("reason=hash_mismatch"));
        assert!(content.contains("sample_count=4"));
        assert!(!content.contains("peer-alpha"));
        assert!(!content.contains(&layout_peer_id));
        assert!(!content.contains("Office Display"));
        assert!(!content.contains("file-123"));
        assert!(!content.contains("taxes.pdf"));
        assert!(manifest.contains("filenames_included=false"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn all_peer_reconnect_surfaces_reconnecting_health_per_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-peer-health-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        for (port, alias) in [(15100, "left"), (15101, "right")] {
            let (code, _) = state.create_pairing_code(120).await;
            state
                .join_peer(code, format!("127.0.0.1:{port}"), Some(alias.to_string()))
                .await
                .expect("join peer");
        }
        state
            .request_all_peers_reconnect_and_reset()
            .await
            .expect("reconnect all peers");

        let app = DaemonControlPlaneApp::new(state);
        let peers = app.list_peers().await.expect("list peers");
        assert_eq!(peers.len(), 2);
        assert!(
            peers.iter().all(|peer| peer.health_state == "reconnecting"),
            "all-peer reconnect event should apply to every paired peer"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pairing_commit_surfaces_trust_metadata_and_survives_restart() {
        let root = std::env::temp_dir().join(format!(
            "boundless-control-plane-pairing-trust-status-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state = AppState::load_or_create_with_paths(config_path.clone(), security_root.clone())
            .expect("load state");

        let remote_paths = core_security::SecurityPaths::for_root(root.join("remote-security"));
        let remote_identity = core_security::ensure_device_identity(
            &remote_paths,
            "remote-machine",
            "remote",
            Some("10.10.0.5"),
        )
        .expect("remote identity");
        let requester_bundle = core_security::TrustBundle {
            machine_id: "remote-machine".to_string(),
            display_name: "remote".to_string(),
            network_address: "10.10.0.5:15100".to_string(),
            ca_cert_pem: remote_identity.ca_cert_pem,
        };

        let challenge = state
            .queue_nearby_pairing_code_challenge(
                requester_bundle,
                Some("remote".to_string()),
                "10.10.0.5".parse().expect("source ip"),
                120,
            )
            .await
            .expect("queue challenge");
        let verification_code = challenge
            .verification_code
            .clone()
            .expect("verification code");
        let verification_nonce = challenge
            .verification_nonce
            .clone()
            .expect("verification nonce");
        let commit = state
            .submit_nearby_pairing_code(
                &challenge.request_id,
                &verification_code,
                &verification_nonce,
                None,
            )
            .await
            .expect("submit code");
        assert!(commit.trust_committed);
        assert_eq!(commit.reconnect_status, "connectivity_pending");

        let app = DaemonControlPlaneApp::new(state);
        let peers = app.list_peers().await.expect("list peers");
        let peer = peers
            .iter()
            .find(|peer| peer.peer_id == "remote-machine")
            .expect("remote peer");
        assert_eq!(peer.health_state, "connectivity_pending");
        assert_eq!(peer.trust_state, "trusted");
        assert!(!peer.trusted_since.is_empty());
        assert!(!peer.trust_fingerprint.is_empty());
        assert_eq!(peer.device_identity, "remote-machine");

        app.state.record_transport_event(crate::state::TransportEventRecord {
            timestamp: chrono::Utc::now(),
            direction: "outbound".to_string(),
            kind: "transport_reachability_failed".to_string(),
            peer_id: "remote-machine".to_string(),
            detail: "mdns_discovered=true tcp_transport_reachability=failed attempted=[source=mdns tcp ipv4 port 15100] next_action=verify Private network".to_string(),
            size_bytes: 0,
        });
        let peers = app.list_peers().await.expect("list peers after failure");
        let peer = peers
            .iter()
            .find(|peer| peer.peer_id == "remote-machine")
            .expect("remote peer after reachability failure");
        assert_eq!(peer.health_state, "reachability_failed");
        assert!(
            peer.health_reason.contains("transport reachability failed"),
            "reachability failure should be actionable"
        );
        assert!(
            peer.health_reason
                .contains("next_action=verify Private network"),
            "safe redacted next action should surface to tray and CLI"
        );

        let restarted =
            AppState::load_or_create_with_paths(config_path, security_root).expect("reload state");
        let restarted_app = DaemonControlPlaneApp::new(restarted);
        let restarted_peers = restarted_app.list_peers().await.expect("list peers");
        let restarted_peer = restarted_peers
            .iter()
            .find(|peer| peer.peer_id == "remote-machine")
            .expect("remote peer after restart");
        assert_eq!(restarted_peer.trust_state, "trusted");
        assert!(!restarted_peer.trusted_since.is_empty());
        assert!(!restarted_peer.trust_fingerprint.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
