use std::time::Duration;

use app_services::{
    SharedControlPlaneApp,
    queries::{
        ConsoleSnapshot, StatusSnapshot, TransportEventSnapshot, UiDiscoveredPeer, UiPairedPeer,
        UiPendingRequest, UiSnapshot,
    },
};
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use ipc_api::boundless::v1::{
    ConsoleSnapshotReply, DiagnosticsDumpReply, DiagnosticsDumpRequest, DiscoveredPeerInfo, Empty,
    FeatureListReply, FeatureSetRequest, HotkeySetRequest, HotkeyTriggerRequest,
    ImportTrustBundleRequest, InputCaptureTargetReply, InputCaptureTargetRequest, InputOwnerReply,
    InputOwnerRequest, LayoutReply, LayoutSetRequest, NearbyJoinStartRequest,
    NearbyJoinStatusReply, NearbyJoinStatusRequest, NearbyPairingCompletionReply,
    NearbyPairingDecisionRequest, NearbyPairingRequestInfo, NearbyRequestCodeStartReply,
    NearbyRequestCodeStartRequest, NearbySubmitCodeRequest, OperationReply, PairCreateCodeReply,
    PairCreateCodeRequest, PairJoinReply, PairJoinRequest, PeerInfo, PeerListReply,
    RemovePeerRequest, SafeResetRequest, SendClipboardImageRequest, SendClipboardTextRequest,
    SendFileRequest, SendInputKeyRequest, SendInputMoveRequest, StatusReply, StatusRequest,
    TransportEvent, TransportEventsReply, TrustBundleReply, UiSnapshotReply,
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
        _request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        Err(Status::unimplemented(
            "create_pairing_code is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn join_with_pairing_code(
        &self,
        _request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        Err(Status::unimplemented(
            "join_with_pairing_code is not implemented in adapter-ipc-grpc",
        ))
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
        _request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "layout_set is not implemented in adapter-ipc-grpc",
        ))
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
        _request: Request<RemovePeerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "remove_peer is not implemented in adapter-ipc-grpc",
        ))
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
        _request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "set_feature is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn set_hotkey(
        &self,
        _request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "set_hotkey is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn trigger_hotkey_action(
        &self,
        _request: Request<HotkeyTriggerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "trigger_hotkey_action is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn export_trust_bundle(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TrustBundleReply>, Status> {
        Err(Status::unimplemented(
            "export_trust_bundle is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn import_trust_bundle(
        &self,
        _request: Request<ImportTrustBundleRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "import_trust_bundle is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn dump_diagnostics(
        &self,
        _request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        Err(Status::unimplemented(
            "dump_diagnostics is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn safe_reset(
        &self,
        _request: Request<SafeResetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "safe_reset is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn send_clipboard_text(
        &self,
        _request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "send_clipboard_text is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn send_clipboard_image(
        &self,
        _request: Request<SendClipboardImageRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "send_clipboard_image is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn send_file(
        &self,
        _request: Request<SendFileRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "send_file is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn send_input_move(
        &self,
        _request: Request<SendInputMoveRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "send_input_move is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn send_input_key(
        &self,
        _request: Request<SendInputKeyRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "send_input_key is not implemented in adapter-ipc-grpc",
        ))
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
        _request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        Err(Status::unimplemented(
            "claim_input_owner is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn release_input_owner(
        &self,
        _request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        Err(Status::unimplemented(
            "release_input_owner is not implemented in adapter-ipc-grpc",
        ))
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
        _request: Request<InputCaptureTargetRequest>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        Err(Status::unimplemented(
            "set_input_capture_target is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn clear_input_capture_target(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        Err(Status::unimplemented(
            "clear_input_capture_target is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn request_nearby_pairing_code(
        &self,
        _request: Request<NearbyRequestCodeStartRequest>,
    ) -> Result<Response<NearbyRequestCodeStartReply>, Status> {
        Err(Status::unimplemented(
            "request_nearby_pairing_code is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn submit_nearby_pairing_code(
        &self,
        _request: Request<NearbySubmitCodeRequest>,
    ) -> Result<Response<NearbyPairingCompletionReply>, Status> {
        Err(Status::unimplemented(
            "submit_nearby_pairing_code is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn start_nearby_pairing_join(
        &self,
        _request: Request<NearbyJoinStartRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        Err(Status::unimplemented(
            "start_nearby_pairing_join is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn check_nearby_pairing_join(
        &self,
        _request: Request<NearbyJoinStatusRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        Err(Status::unimplemented(
            "check_nearby_pairing_join is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn approve_nearby_pairing_request(
        &self,
        _request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "approve_nearby_pairing_request is not implemented in adapter-ipc-grpc",
        ))
    }

    async fn reject_nearby_pairing_request(
        &self,
        _request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        Err(Status::unimplemented(
            "reject_nearby_pairing_request is not implemented in adapter-ipc-grpc",
        ))
    }
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
    }
}

fn map_peer_info(peer: UiPairedPeer) -> PeerInfo {
    PeerInfo {
        peer_id: peer.peer_id,
        display_name: peer.display_name,
        address: peer.address,
        connected: peer.connected,
    }
}

fn map_discovered_peer(peer: UiDiscoveredPeer) -> DiscoveredPeerInfo {
    DiscoveredPeerInfo {
        machine_id: peer.machine_id,
        display_name: peer.display_name,
        endpoint: peer.endpoint,
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
