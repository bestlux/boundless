use std::path::Path;

use tonic::{Request, Response, Status};

use ipc_api::boundless::v1::{
    DiagnosticsDumpReply, DiagnosticsDumpRequest, Empty, FeatureListReply, FeatureSetRequest,
    HotkeySetRequest, ImportTrustBundleRequest, InputOwnerReply, InputOwnerRequest, LayoutReply,
    LayoutSetRequest, OperationReply, PairCreateCodeReply, PairCreateCodeRequest, PairJoinReply,
    PairJoinRequest, PeerInfo, PeerListReply, RemovePeerRequest, SafeResetRequest,
    SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest, SendInputMoveRequest,
    StatusReply, StatusRequest, TransportEvent, TransportEventsReply, TrustBundleReply,
    daemon_service_server::{DaemonService, DaemonServiceServer},
    diagnostics_service_server::{DiagnosticsService, DiagnosticsServiceServer},
    feature_service_server::{FeatureService, FeatureServiceServer},
    pairing_service_server::{PairingService, PairingServiceServer},
    topology_service_server::{TopologyService, TopologyServiceServer},
};

use crate::state::AppState;

#[derive(Clone)]
pub struct ServiceBundle {
    pub daemon: DaemonServiceServer<DaemonApi>,
    pub pairing: PairingServiceServer<PairingApi>,
    pub topology: TopologyServiceServer<TopologyApi>,
    pub feature: FeatureServiceServer<FeatureApi>,
    pub diagnostics: DiagnosticsServiceServer<DiagnosticsApi>,
}

impl ServiceBundle {
    pub fn new(state: AppState) -> Self {
        Self {
            daemon: DaemonServiceServer::new(DaemonApi(state.clone())),
            pairing: PairingServiceServer::new(PairingApi(state.clone())),
            topology: TopologyServiceServer::new(TopologyApi(state.clone())),
            feature: FeatureServiceServer::new(FeatureApi(state.clone())),
            diagnostics: DiagnosticsServiceServer::new(DiagnosticsApi(state)),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DaemonApi(AppState);

#[tonic::async_trait]
impl DaemonService for DaemonApi {
    async fn get_status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusReply>, Status> {
        let snapshot = self.0.snapshot().await;
        let effective_api_transport = snapshot.api_transport.effective();
        let api_pipe_name = if matches!(
            effective_api_transport,
            crate::config::ApiTransport::NamedPipe
        ) {
            snapshot.api_pipe_name
        } else {
            String::new()
        };

        Ok(Response::new(StatusReply {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            running: true,
            machine_id: snapshot.machine_id,
            peer_count: snapshot.peers.len() as u32,
            protocol_version: snapshot.protocol_version,
            api_bind: snapshot.api_bind,
            api_transport: effective_api_transport.as_str().to_string(),
            api_pipe_name,
        }))
    }
}

#[derive(Clone)]
pub(crate) struct PairingApi(AppState);

#[tonic::async_trait]
impl PairingService for PairingApi {
    async fn create_code(
        &self,
        request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        let ttl = request.into_inner().ttl_seconds.max(30);
        let (code, expires_at) = self.0.create_pairing_code(ttl as u64).await;

        Ok(Response::new(PairCreateCodeReply {
            code,
            expires_at: expires_at.to_rfc3339(),
        }))
    }

    async fn join(
        &self,
        request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        let request = request.into_inner();
        let peer_id = self
            .0
            .join_peer(
                request.code,
                request.host,
                if request.alias.is_empty() {
                    None
                } else {
                    Some(request.alias)
                },
            )
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(PairJoinReply {
            accepted: true,
            peer_id,
            message: "Pairing request accepted".to_string(),
        }))
    }

    async fn export_trust_bundle(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TrustBundleReply>, Status> {
        let bundle = self
            .0
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
        self.0
            .import_trust_bundle(
                core_security::TrustBundle {
                    machine_id: request.machine_id,
                    display_name: request.display_name,
                    network_address: request.network_address,
                    ca_cert_pem: request.ca_cert_pem,
                },
                if request.alias.is_empty() {
                    None
                } else {
                    Some(request.alias)
                },
            )
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "trust bundle imported".to_string(),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct TopologyApi(AppState);

#[tonic::async_trait]
impl TopologyService for TopologyApi {
    async fn list_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<PeerListReply>, Status> {
        let peers = self
            .0
            .list_peers()
            .await
            .into_iter()
            .map(|peer| PeerInfo {
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                address: peer.address,
                connected: peer.connected,
            })
            .collect();

        Ok(Response::new(PeerListReply { peers }))
    }

    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let peer_id = request.into_inner().peer_id;
        let removed = self
            .0
            .remove_peer(&peer_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: removed,
            message: if removed {
                format!("Removed peer {peer_id}")
            } else {
                format!("Peer {peer_id} not found")
            },
        }))
    }

    async fn layout_show(&self, _request: Request<Empty>) -> Result<Response<LayoutReply>, Status> {
        let matrix_spec = self.0.layout().await;
        Ok(Response::new(LayoutReply { matrix_spec }))
    }

    async fn layout_set(
        &self,
        request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let matrix = request.into_inner().matrix_spec;
        self.0
            .set_layout(matrix)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "Layout updated".to_string(),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct FeatureApi(AppState);

#[tonic::async_trait]
impl FeatureService for FeatureApi {
    async fn set_feature(
        &self,
        request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_feature(request.name.clone(), request.enabled)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("{}={}", request.name, request.enabled),
        }))
    }

    async fn list_features(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<FeatureListReply>, Status> {
        let features = self.0.feature_map().await.into_iter().collect();
        Ok(Response::new(FeatureListReply { features }))
    }

    async fn set_hotkey(
        &self,
        request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_hotkey(request.action.clone(), request.combo.clone())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("hotkey {}={}", request.action, request.combo),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticsApi(AppState);

#[tonic::async_trait]
impl DiagnosticsService for DiagnosticsApi {
    async fn dump(
        &self,
        request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        let output = request.into_inner().output_path;
        let output = if output.is_empty() {
            None
        } else {
            Some(output)
        };

        let bundle_path = self
            .0
            .diagnostics_dump(output)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(DiagnosticsDumpReply { bundle_path }))
    }

    async fn safe_reset(
        &self,
        request: Request<SafeResetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();

        self.0
            .safe_reset(request.network_only, request.all)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "safe reset complete".to_string(),
        }))
    }

    async fn send_clipboard_text(
        &self,
        request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_clipboard_text(&request.peer_id, request.text)
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "clipboard payload queued".to_string(),
        }))
    }

    async fn send_clipboard_image(
        &self,
        request: Request<SendClipboardImageRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_clipboard_image(&request.peer_id, request.image_bmp)
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "clipboard image payload queued".to_string(),
        }))
    }

    async fn send_file(
        &self,
        request: Request<SendFileRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_file_from_path(&request.peer_id, Path::new(&request.file_path))
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "file payload queued".to_string(),
        }))
    }

    async fn send_input_move(
        &self,
        request: Request<SendInputMoveRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_input_move(&request.peer_id, request.dx, request.dy)
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "input move frame queued".to_string(),
        }))
    }

    async fn list_transport_events(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TransportEventsReply>, Status> {
        let events = self
            .0
            .transport_events()
            .await
            .into_iter()
            .map(|event| TransportEvent {
                timestamp: event.timestamp.to_rfc3339(),
                direction: event.direction,
                kind: event.kind,
                peer_id: event.peer_id,
                detail: event.detail,
                size_bytes: event.size_bytes,
            })
            .collect();

        Ok(Response::new(TransportEventsReply { events }))
    }

    async fn get_input_owner(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: true,
            owner_peer_id: owner,
            message: "input owner fetched".to_string(),
        }))
    }

    async fn claim_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let acquired = self
            .0
            .claim_input_owner(&request.peer_id, request.force)
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: acquired,
            owner_peer_id: owner.clone(),
            message: if acquired {
                format!("input owner set to {owner}")
            } else {
                format!("input owner remains {owner}")
            },
        }))
    }

    async fn release_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let released = self.0.release_input_owner(&request.peer_id).await;
        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: released,
            owner_peer_id: owner,
            message: if released {
                "input owner released".to_string()
            } else {
                "peer did not hold input owner".to_string()
            },
        }))
    }
}
