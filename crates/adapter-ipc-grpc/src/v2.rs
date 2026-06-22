use std::{path::PathBuf, time::Duration};

use app_services::{
    SharedControlPlaneApp, commands as app_commands,
    queries::{
        AntiIdleConfigSnapshot, AntiIdleStatusSnapshot, ConsoleSnapshot,
        FileTransferConfigSnapshot, FileTransferSnapshot, InputHandoffConfigSnapshot,
        InputRuntimeSnapshot, StatusSnapshot, TransportEventSnapshot, UiDiscoveredPeer,
        UiPairedPeer, UiPendingRequest, UiSnapshot,
    },
};
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use ipc_api::boundless::v1::{
    AntiIdleConfigReply, AntiIdleSetRequest, AntiIdleStatusReply, ConsoleSnapshotReply,
    DiagnosticsDumpReply, DiagnosticsDumpRequest, DiscoveredPeerInfo, Empty, FeatureListReply,
    FeatureSetRequest, FileTransferActionRequest, FileTransferConfigReply, FileTransferInfo,
    FileTransferSetRequest, HotkeySetRequest, HotkeyTriggerRequest, ImportTrustBundleRequest,
    InputCaptureTargetReply, InputCaptureTargetRequest, InputHandoffConfigReply,
    InputHandoffSetRequest, InputOwnerReply, InputOwnerRequest, InputRuntimeStatusReply,
    LayoutReply, LayoutSetRequest, NearbyJoinStartRequest, NearbyJoinStatusReply,
    NearbyJoinStatusRequest, NearbyPairingCompletionReply, NearbyPairingDecisionRequest,
    NearbyPairingRequestInfo, NearbyRequestCodeStartReply, NearbyRequestCodeStartRequest,
    NearbySubmitCodeRequest, OperationReply, PairCreateCodeReply, PairCreateCodeRequest,
    PairJoinReply, PairJoinRequest, PeerInfo, PeerListReply, RemovePeerRequest, RotateTrustRequest,
    SafeResetRequest, SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest,
    SendInputKeyRequest, SendInputMoveRequest, StatusReply, StatusRequest, TransportEvent,
    TransportEventsReply, TrustBundleReply, UiSnapshotReply,
    control_plane_service_server::{ControlPlaneService, ControlPlaneServiceServer},
};

#[derive(Clone)]
pub struct ControlPlaneApi {
    app: SharedControlPlaneApp,
    watch_interval: Duration,
}

pub type ControlPlaneServer = ControlPlaneServiceServer<ControlPlaneApi>;

impl ControlPlaneApi {
    pub fn new(app: SharedControlPlaneApp) -> Self {
        Self {
            app,
            watch_interval: Duration::from_secs(2),
        }
    }

    pub fn into_server(self) -> ControlPlaneServer {
        ControlPlaneServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl ControlPlaneService for ControlPlaneApi {
    type WatchUiStream = ReceiverStream<Result<UiSnapshotReply, Status>>;

    async fn get_ui_snapshot(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<UiSnapshotReply>, Status> {
        let snapshot = self
            .app
            .ui_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build ui snapshot: {error:#}")))?;
        Ok(Response::new(map_ui_snapshot(snapshot)))
    }

    async fn get_console_snapshot(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ConsoleSnapshotReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(map_console_snapshot(snapshot)))
    }

    async fn get_status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusReply>, Status> {
        let snapshot = self
            .app
            .status_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build status snapshot: {error:#}")))?;
        Ok(Response::new(map_status_snapshot(snapshot)))
    }

    async fn create_pairing_code(
        &self,
        request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        let ttl_seconds = request.into_inner().ttl_seconds.max(1) as u64;
        let reply = self
            .app
            .create_pairing_code(app_commands::PairingCodeRequest { ttl_seconds })
            .await
            .map_err(|error| Status::internal(format!("create pairing code: {error:#}")))?;
        Ok(Response::new(PairCreateCodeReply {
            code: reply.code,
            expires_at: reply.expires_at,
        }))
    }

    async fn join_with_pairing_code(
        &self,
        request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        let request = request.into_inner();
        let code = parse_required_field("code", &request.code)?;
        let host = parse_required_field("host", &request.host)?;
        let alias = parse_optional_alias(request.alias);

        let reply = self
            .app
            .join_with_pairing_code(app_commands::PairJoinCommand { code, host, alias })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(PairJoinReply {
            accepted: reply.accepted,
            peer_id: reply.peer_id,
            message: reply.message,
        }))
    }

    async fn watch_ui(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::WatchUiStream>, Status> {
        let app = self.app.clone();
        let watch_interval = self.watch_interval;
        let (tx, rx) = mpsc::channel(8);

        tokio::spawn(async move {
            if send_snapshot(&app, &tx).await.is_err() {
                return;
            }

            let mut interval = time::interval(watch_interval);
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if send_snapshot(&app, &tx).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn layout_set(
        &self,
        request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let matrix_spec = request.into_inner().matrix_spec;
        let reply = self
            .app
            .set_layout(app_commands::LayoutSetCommand { matrix_spec })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn list_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<PeerListReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(PeerListReply {
            peers: snapshot.peers.into_iter().map(map_peer_info).collect(),
        }))
    }

    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let peer_id = request.into_inner().peer_id;
        let reply = self
            .app
            .remove_peer(app_commands::RemovePeerCommand { peer_id })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn layout_show(&self, _request: Request<Empty>) -> Result<Response<LayoutReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(LayoutReply {
            matrix_spec: snapshot.layout_matrix,
        }))
    }

    async fn list_features(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<FeatureListReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(FeatureListReply {
            features: snapshot.features.into_iter().collect(),
        }))
    }

    async fn set_feature(
        &self,
        request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .set_feature(app_commands::FeatureSetCommand {
                name: request.name,
                enabled: request.enabled,
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn get_anti_idle_config(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<AntiIdleConfigReply>, Status> {
        let snapshot = self
            .app
            .anti_idle_config()
            .await
            .map_err(|error| Status::internal(format!("build anti-idle config: {error:#}")))?;
        Ok(Response::new(map_anti_idle_config(snapshot)))
    }

    async fn get_anti_idle_status(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<AntiIdleStatusReply>, Status> {
        let snapshot = self
            .app
            .anti_idle_status()
            .await
            .map_err(|error| Status::internal(format!("build anti-idle status: {error:#}")))?;
        Ok(Response::new(map_anti_idle_status(snapshot)))
    }

    async fn set_anti_idle_config(
        &self,
        request: Request<AntiIdleSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .set_anti_idle_config(app_commands::SetAntiIdleConfigCommand {
                enabled: request.enabled,
                recent_activity_window_secs: request.recent_activity_window_secs,
                allow_on_battery: request.allow_on_battery,
                keep_display_on: request.keep_display_on,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn get_file_transfer_config(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<FileTransferConfigReply>, Status> {
        let snapshot =
            self.app.file_transfer_config().await.map_err(|error| {
                Status::internal(format!("build file-transfer config: {error:#}"))
            })?;
        Ok(Response::new(map_file_transfer_config(snapshot)))
    }

    async fn set_file_transfer_config(
        &self,
        request: Request<FileTransferSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .set_file_transfer_config(app_commands::SetFileTransferConfigCommand {
                receive_dir: request.receive_dir,
                organize_by_peer: request.organize_by_peer,
                auto_accept_trusted_peers: request.auto_accept_trusted_peers,
                max_file_bytes: request.max_file_bytes,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn cancel_file_transfer(
        &self,
        request: Request<FileTransferActionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .cancel_file_transfer(app_commands::FileTransferActionCommand {
                transfer_id: parse_required_field("transfer_id", &request.transfer_id)?,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn retry_file_transfer(
        &self,
        request: Request<FileTransferActionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .retry_file_transfer(app_commands::FileTransferActionCommand {
                transfer_id: parse_required_field("transfer_id", &request.transfer_id)?,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn clear_completed_file_transfers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<OperationReply>, Status> {
        let reply = self
            .app
            .clear_completed_file_transfers()
            .await
            .map_err(|error| Status::internal(format!("clear completed transfers: {error:#}")))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn set_input_handoff_config(
        &self,
        request: Request<InputHandoffSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .set_input_handoff_config(app_commands::SetInputHandoffConfigCommand {
                block_screen_corners: request.block_screen_corners,
                corner_block_px: request.corner_block_px,
                relative_mouse: request.relative_mouse,
                hide_cursor_at_edge: request.hide_cursor_at_edge,
                draw_cursor_marker: request.draw_cursor_marker,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn set_hotkey(
        &self,
        request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .set_hotkey(app_commands::HotkeySetCommand {
                action: request.action,
                combo: request.combo,
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn trigger_hotkey_action(
        &self,
        request: Request<HotkeyTriggerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let action = request.into_inner().action;
        let reply = self
            .app
            .trigger_hotkey_action(app_commands::HotkeyTriggerCommand { action })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn export_trust_bundle(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TrustBundleReply>, Status> {
        let bundle = self
            .app
            .export_trust_bundle()
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(TrustBundleReply {
            machine_id: bundle.machine_id,
            display_name: bundle.display_name,
            network_address: bundle.network_address,
            ca_cert_pem: bundle.ca_cert_pem,
        }))
    }

    async fn import_trust_bundle(
        &self,
        request: Request<ImportTrustBundleRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .import_trust_bundle(app_commands::ImportTrustBundleCommand {
                machine_id: request.machine_id,
                display_name: request.display_name,
                network_address: request.network_address,
                ca_cert_pem: request.ca_cert_pem,
                alias: parse_optional_alias(request.alias),
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn rotate_trust(
        &self,
        request: Request<RotateTrustRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .rotate_trust(app_commands::RotateTrustCommand {
                confirm: request.confirm,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn dump_diagnostics(
        &self,
        request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        let request = request.into_inner();
        let output_path = parse_optional_alias(request.output_path);
        let reply = self
            .app
            .dump_diagnostics(app_commands::DiagnosticsDumpCommand {
                output_path,
                include_filenames: request.include_filenames,
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(DiagnosticsDumpReply {
            bundle_path: reply.bundle_path,
            manifest_path: reply.manifest_path,
            filenames_included: reply.filenames_included,
        }))
    }

    async fn safe_reset(
        &self,
        request: Request<SafeResetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .safe_reset(app_commands::SafeResetCommand {
                network_only: request.network_only,
                all: request.all,
                confirm: request.confirm,
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn send_clipboard_text(
        &self,
        request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .send_clipboard_text(app_commands::SendClipboardTextCommand {
                peer_id: request.peer_id,
                text: request.text,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn send_clipboard_image(
        &self,
        request: Request<SendClipboardImageRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .send_clipboard_image(app_commands::SendClipboardImageCommand {
                peer_id: request.peer_id,
                image_bmp: request.image_bmp,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn send_file(
        &self,
        request: Request<SendFileRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .send_file(app_commands::SendFileCommand {
                peer_id: request.peer_id,
                file_path: PathBuf::from(request.file_path),
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn send_input_move(
        &self,
        request: Request<SendInputMoveRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .send_input_move(app_commands::SendInputMoveCommand {
                peer_id: request.peer_id,
                dx: request.dx,
                dy: request.dy,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn send_input_key(
        &self,
        request: Request<SendInputKeyRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let scan_code = u16::try_from(request.scan_code)
            .map_err(|_| Status::invalid_argument("scan_code must be in 0..=65535"))?;
        let reply = self
            .app
            .send_input_key(app_commands::SendInputKeyCommand {
                peer_id: request.peer_id,
                scan_code,
                key_down: request.key_down,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn list_transport_events(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TransportEventsReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(TransportEventsReply {
            events: snapshot
                .transport_events
                .into_iter()
                .map(map_transport_event)
                .collect(),
        }))
    }

    async fn get_input_owner(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(InputOwnerReply {
            ok: true,
            owner_peer_id: snapshot.input_owner_peer_id.unwrap_or_default(),
            message: "input owner fetched".to_string(),
        }))
    }

    async fn claim_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .claim_input_owner(app_commands::InputOwnerCommand {
                peer_id: request.peer_id,
                force: request.force,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(InputOwnerReply {
            ok: reply.ok,
            owner_peer_id: reply.owner_peer_id,
            message: reply.message,
        }))
    }

    async fn release_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .release_input_owner(app_commands::InputOwnerCommand {
                peer_id: request.peer_id,
                force: request.force,
            })
            .await
            .map_err(|error| Status::internal(format!("release input owner: {error:#}")))?;
        Ok(Response::new(InputOwnerReply {
            ok: reply.ok,
            owner_peer_id: reply.owner_peer_id,
            message: reply.message,
        }))
    }

    async fn get_input_capture_target(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        let snapshot = self
            .app
            .console_snapshot()
            .await
            .map_err(|error| Status::internal(format!("build console snapshot: {error:#}")))?;
        Ok(Response::new(InputCaptureTargetReply {
            ok: true,
            peer_id: snapshot.input_capture_target_peer_id.unwrap_or_default(),
            message: "input capture target fetched".to_string(),
        }))
    }

    async fn set_input_capture_target(
        &self,
        request: Request<InputCaptureTargetRequest>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        let peer_id = request.into_inner().peer_id;
        let reply = self
            .app
            .set_input_capture_target(app_commands::InputCaptureTargetCommand { peer_id })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(InputCaptureTargetReply {
            ok: reply.ok,
            peer_id: reply.peer_id,
            message: reply.message,
        }))
    }

    async fn clear_input_capture_target(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        let reply = self
            .app
            .clear_input_capture_target()
            .await
            .map_err(|error| Status::internal(format!("clear input capture target: {error:#}")))?;
        Ok(Response::new(InputCaptureTargetReply {
            ok: reply.ok,
            peer_id: reply.peer_id,
            message: reply.message,
        }))
    }

    async fn request_nearby_pairing_code(
        &self,
        request: Request<NearbyRequestCodeStartRequest>,
    ) -> Result<Response<NearbyRequestCodeStartReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let reply = self
            .app
            .request_nearby_pairing_code(app_commands::NearbyRequestCodeCommand {
                host,
                port,
                alias: parse_optional_alias(request.alias),
                endpoint_candidates: request.endpoint_candidates,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(NearbyRequestCodeStartReply {
            code_required: reply.code_required,
            request_id: reply.request_id,
            verification_nonce: reply.verification_nonce,
            verification_expires_at: reply.verification_expires_at,
            unsupported: reply.unsupported,
            message: reply.message,
        }))
    }

    async fn submit_nearby_pairing_code(
        &self,
        request: Request<NearbySubmitCodeRequest>,
    ) -> Result<Response<NearbyPairingCompletionReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let request_id = parse_required_field("request_id", &request.request_id)?;
        let code = parse_required_field("code", &request.code)?;
        let verification_nonce =
            parse_required_field("verification_nonce", &request.verification_nonce)?;

        let reply = self
            .app
            .submit_nearby_pairing_code(app_commands::NearbySubmitCodeCommand {
                host,
                port,
                request_id,
                code,
                verification_nonce,
                alias: parse_optional_alias(request.alias),
                endpoint_candidates: request.endpoint_candidates,
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(NearbyPairingCompletionReply {
            ok: reply.ok,
            message: reply.message,
            request_id: reply.request_id,
            peer_machine_id: reply.peer_machine_id,
            trust_committed: reply.trust_committed,
            already_committed: reply.already_committed,
            reconnect_status: reply.reconnect_status,
        }))
    }

    async fn start_nearby_pairing_join(
        &self,
        request: Request<NearbyJoinStartRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let code = parse_required_field("code", &request.code)?;
        let reply = self
            .app
            .start_nearby_pairing_join(app_commands::NearbyJoinStartCommand {
                host,
                port,
                code,
                alias: parse_optional_alias(request.alias),
                endpoint_candidates: request.endpoint_candidates,
                role: app_commands::NearbyPairingRole::parse_or_default(&request.role),
                attempt_id: parse_optional_alias(request.attempt_id),
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(NearbyJoinStatusReply {
            request_id: reply.request_id,
            status: reply.status,
            message: reply.message,
            peer_machine_id: reply.peer_machine_id.unwrap_or_default(),
            role: reply.role,
            attempt_id: reply.attempt_id,
        }))
    }

    async fn check_nearby_pairing_join(
        &self,
        request: Request<NearbyJoinStatusRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let request_id = parse_required_field("request_id", &request.request_id)?;
        let reply = self
            .app
            .check_nearby_pairing_join(app_commands::NearbyJoinStatusCommand {
                host,
                port,
                request_id,
                alias: parse_optional_alias(request.alias),
                endpoint_candidates: request.endpoint_candidates,
                role: app_commands::NearbyPairingRole::parse_or_default(&request.role),
                attempt_id: parse_optional_alias(request.attempt_id),
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(NearbyJoinStatusReply {
            request_id: reply.request_id,
            status: reply.status,
            message: reply.message,
            peer_machine_id: reply.peer_machine_id.unwrap_or_default(),
            role: reply.role,
            attempt_id: reply.attempt_id,
        }))
    }

    async fn approve_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .approve_nearby_pairing_request(app_commands::NearbyPairingDecisionCommand {
                request_id: request.request_id,
                alias: parse_optional_alias(request.alias),
            })
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }

    async fn reject_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let reply = self
            .app
            .reject_nearby_pairing_request(app_commands::NearbyPairingDecisionCommand {
                request_id: request.request_id,
                alias: parse_optional_alias(request.alias),
            })
            .await
            .map_err(|error| {
                Status::internal(format!("reject nearby pairing request: {error:#}"))
            })?;
        Ok(Response::new(OperationReply {
            ok: reply.ok,
            message: reply.message,
        }))
    }
}

fn parse_host(value: &str) -> Result<String, Status> {
    let host = value.trim();
    if host.is_empty() {
        return Err(Status::invalid_argument("host must not be empty"));
    }
    Ok(host.to_string())
}

fn parse_port(value: u32) -> Result<u16, Status> {
    if value == 0 || value > u16::MAX as u32 {
        return Err(Status::invalid_argument(
            "port must be in the range 1..=65535",
        ));
    }
    Ok(value as u16)
}

fn parse_optional_alias(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn parse_required_field(name: &str, value: &str) -> Result<String, Status> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{name} must not be empty"
        )));
    }
    Ok(trimmed.to_string())
}

async fn send_snapshot(
    app: &SharedControlPlaneApp,
    tx: &mpsc::Sender<Result<UiSnapshotReply, Status>>,
) -> Result<(), ()> {
    let next = app
        .ui_snapshot()
        .await
        .map(map_ui_snapshot)
        .map_err(|error| Status::internal(format!("build ui snapshot: {error:#}")));
    tx.send(next).await.map_err(|_| ())
}

fn map_ui_snapshot(snapshot: UiSnapshot) -> UiSnapshotReply {
    UiSnapshotReply {
        generated_at: snapshot.generated_at,
        daemon_online: snapshot.daemon_online,
        machine_id: snapshot.machine_id,
        layout_matrix: snapshot.layout_matrix,
        discovered_peers: snapshot
            .discovered_peers
            .into_iter()
            .map(map_discovered_peer)
            .collect(),
        paired_peers: snapshot
            .paired_peers
            .into_iter()
            .map(map_peer_info)
            .collect(),
        pending_requests: snapshot
            .pending_requests
            .into_iter()
            .map(map_pending_request)
            .collect(),
        anti_idle_config: Some(map_anti_idle_config(snapshot.anti_idle_config)),
        anti_idle_status: Some(map_anti_idle_status(snapshot.anti_idle_status)),
        file_transfer_config: Some(map_file_transfer_config(snapshot.file_transfer_config)),
        input_handoff_config: Some(map_input_handoff_config(snapshot.input_handoff_config)),
        input_runtime: Some(map_input_runtime(snapshot.input_runtime)),
        features: snapshot.features.into_iter().collect(),
        hotkeys: snapshot.hotkeys.into_iter().collect(),
        file_transfers: snapshot
            .file_transfers
            .into_iter()
            .map(map_file_transfer)
            .collect(),
    }
}

fn map_console_snapshot(snapshot: ConsoleSnapshot) -> ConsoleSnapshotReply {
    ConsoleSnapshotReply {
        status: Some(map_status_snapshot(snapshot.status)),
        peers: snapshot.peers.into_iter().map(map_peer_info).collect(),
        features: snapshot.features.into_iter().collect(),
        discovered_peers: snapshot
            .discovered_peers
            .into_iter()
            .map(map_discovered_peer)
            .collect(),
        pending_requests: snapshot
            .pending_requests
            .into_iter()
            .map(map_pending_request)
            .collect(),
        input_owner_peer_id: snapshot.input_owner_peer_id.unwrap_or_default(),
        input_capture_target_peer_id: snapshot.input_capture_target_peer_id.unwrap_or_default(),
        mdns_active: snapshot.mdns_active,
        local_display_name: snapshot.local_display_name,
        anti_idle_config: Some(map_anti_idle_config(snapshot.anti_idle_config)),
        anti_idle_status: Some(map_anti_idle_status(snapshot.anti_idle_status)),
        file_transfer_config: Some(map_file_transfer_config(snapshot.file_transfer_config)),
        input_handoff_config: Some(map_input_handoff_config(snapshot.input_handoff_config)),
        input_runtime: Some(map_input_runtime(snapshot.input_runtime)),
        file_transfers: snapshot
            .file_transfers
            .into_iter()
            .map(map_file_transfer)
            .collect(),
    }
}

fn map_status_snapshot(snapshot: StatusSnapshot) -> StatusReply {
    StatusReply {
        daemon_version: snapshot.daemon_version,
        running: true,
        machine_id: snapshot.machine_id,
        peer_count: snapshot.peer_count,
        protocol_version: snapshot.protocol_version,
        api_bind: snapshot.api_bind,
        api_transport: snapshot.api_transport,
        api_pipe_name: snapshot.api_pipe_name,
        input_locked: snapshot.input_locked,
        input_lock_supported: snapshot.input_lock_supported,
        capture_target_peer_id: snapshot.capture_target_peer_id.unwrap_or_default(),
        anti_idle_supported: snapshot.anti_idle_supported,
        anti_idle_enabled: snapshot.anti_idle_enabled,
        anti_idle_active: snapshot.anti_idle_active,
        anti_idle_display_required: snapshot.anti_idle_display_required,
    }
}

fn map_anti_idle_config(snapshot: AntiIdleConfigSnapshot) -> AntiIdleConfigReply {
    AntiIdleConfigReply {
        enabled: snapshot.enabled,
        recent_activity_window_secs: snapshot.recent_activity_window_secs,
        allow_on_battery: snapshot.allow_on_battery,
        keep_display_on: snapshot.keep_display_on,
    }
}

fn map_anti_idle_status(snapshot: AntiIdleStatusSnapshot) -> AntiIdleStatusReply {
    AntiIdleStatusReply {
        supported: snapshot.supported,
        enabled: snapshot.enabled,
        active: snapshot.active,
        display_required: snapshot.display_required,
        reason: snapshot.reason,
    }
}

fn map_file_transfer_config(snapshot: FileTransferConfigSnapshot) -> FileTransferConfigReply {
    FileTransferConfigReply {
        receive_dir: snapshot.receive_dir,
        organize_by_peer: snapshot.organize_by_peer,
        auto_accept_trusted_peers: snapshot.auto_accept_trusted_peers,
        max_file_bytes: snapshot.max_file_bytes,
    }
}

fn map_file_transfer(snapshot: FileTransferSnapshot) -> FileTransferInfo {
    FileTransferInfo {
        transfer_id: snapshot.transfer_id,
        previous_transfer_id: snapshot.previous_transfer_id.unwrap_or_default(),
        direction: snapshot.direction,
        peer_id: snapshot.peer_id,
        file_name: snapshot.file_name,
        state: snapshot.state,
        transferred_bytes: snapshot.transferred_bytes,
        total_bytes: snapshot.total_bytes,
        failure_reason: snapshot.failure_reason.unwrap_or_default(),
        source_path: snapshot.source_path.unwrap_or_default(),
        final_path: snapshot.final_path.unwrap_or_default(),
        queued_at: snapshot.queued_at,
        updated_at: snapshot.updated_at,
    }
}

fn map_input_handoff_config(snapshot: InputHandoffConfigSnapshot) -> InputHandoffConfigReply {
    InputHandoffConfigReply {
        block_screen_corners: snapshot.block_screen_corners,
        corner_block_px: snapshot.corner_block_px,
        relative_mouse: snapshot.relative_mouse,
        hide_cursor_at_edge: snapshot.hide_cursor_at_edge,
        draw_cursor_marker: snapshot.draw_cursor_marker,
    }
}

fn map_input_runtime(snapshot: InputRuntimeSnapshot) -> InputRuntimeStatusReply {
    InputRuntimeStatusReply {
        owner_peer_id: snapshot.owner_peer_id.unwrap_or_default(),
        configured_capture_target_peer_id: snapshot
            .configured_capture_target_peer_id
            .unwrap_or_default(),
        active_capture_target_peer_id: snapshot.active_capture_target_peer_id.unwrap_or_default(),
        lock_active: snapshot.lock_active,
        lock_supported: snapshot.lock_supported,
        capture_backend_mode: snapshot.capture_backend_mode,
        pending_inject_frames: snapshot.pending_inject_frames as u32,
        pending_inject_high_water: snapshot.pending_inject_high_water as u32,
    }
}

fn map_peer_info(peer: UiPairedPeer) -> PeerInfo {
    PeerInfo {
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
    }
}

fn map_discovered_peer(peer: UiDiscoveredPeer) -> DiscoveredPeerInfo {
    DiscoveredPeerInfo {
        machine_id: peer.machine_id,
        display_name: peer.display_name,
        endpoint: peer.endpoint,
        endpoint_candidates: peer.endpoint_candidates,
    }
}

fn map_pending_request(request: UiPendingRequest) -> NearbyPairingRequestInfo {
    NearbyPairingRequestInfo {
        request_id: request.request_id,
        requester_machine_id: request.requester_machine_id,
        requester_display_name: request.requester_display_name,
        created_at: request.created_at,
        verification_code: request.verification_code,
        verification_expires_at: request.verification_expires_at,
        requires_verification_code: request.requires_verification_code,
        role: request.role,
        attempt_id: request.attempt_id,
    }
}

fn map_transport_event(event: TransportEventSnapshot) -> TransportEvent {
    TransportEvent {
        timestamp: event.timestamp,
        direction: event.direction,
        kind: event.kind,
        peer_id: event.peer_id,
        detail: event.detail,
        size_bytes: event.size_bytes,
    }
}
