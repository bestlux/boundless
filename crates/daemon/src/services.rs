use adapter_ipc_grpc::ControlPlaneApi as SharedControlPlaneApi;
use tonic::{Request, Response, Status};

use ipc_api::boundless::v1::{
    DiagnosticsDumpReply, DiagnosticsDumpRequest, DiscoveryPeersReply, Empty, FeatureListReply,
    FeatureSetRequest, HotkeySetRequest, HotkeyTriggerRequest, ImportTrustBundleRequest,
    InputCaptureTargetReply, InputCaptureTargetRequest, InputOwnerReply, InputOwnerRequest,
    LayoutReply, LayoutSetRequest, NearbyPairingDecisionRequest, NearbyPairingRequestsReply,
    OperationReply, PairCreateCodeReply, PairCreateCodeRequest, PairJoinReply, PairJoinRequest,
    PeerListReply, RemovePeerRequest, SafeResetRequest, SendClipboardImageRequest,
    SendClipboardTextRequest, SendFileRequest, SendInputKeyRequest, SendInputMoveRequest,
    StatusReply, StatusRequest, TransportEventsReply, TrustBundleReply,
    control_plane_service_server::ControlPlaneService,
    daemon_service_server::{DaemonService, DaemonServiceServer},
    diagnostics_service_server::{DiagnosticsService, DiagnosticsServiceServer},
    feature_service_server::{FeatureService, FeatureServiceServer},
    pairing_service_server::{PairingService, PairingServiceServer},
    topology_service_server::{TopologyService, TopologyServiceServer},
};

use crate::{shared_control_plane_app, state::AppState};

mod daemon_api;
mod diagnostics_api;
mod feature_api;
mod pairing_api;
mod topology_api;

use daemon_api::DaemonApi;
use diagnostics_api::DiagnosticsApi;
use feature_api::FeatureApi;
use pairing_api::PairingApi;
use topology_api::TopologyApi;

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

pub(super) fn control_plane_api(state: AppState) -> SharedControlPlaneApi {
    SharedControlPlaneApi::new(shared_control_plane_app(state))
}
