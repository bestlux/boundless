use std::path::Path;

use tonic::{Request, Response, Status};

use ipc_api::boundless::v1::{
    ConsoleSnapshotReply, DiagnosticsDumpReply, DiagnosticsDumpRequest, DiscoveredPeerInfo,
    DiscoveryPeersReply, Empty, FeatureListReply, FeatureSetRequest, HotkeySetRequest,
    HotkeyTriggerRequest, ImportTrustBundleRequest, InputCaptureTargetReply,
    InputCaptureTargetRequest, InputOwnerReply, InputOwnerRequest, LayoutReply, LayoutSetRequest,
    NearbyJoinStartRequest, NearbyJoinStatusReply, NearbyJoinStatusRequest,
    NearbyPairingCompletionReply, NearbyPairingDecisionRequest, NearbyPairingRequestInfo,
    NearbyPairingRequestsReply, NearbyRequestCodeStartReply, NearbyRequestCodeStartRequest,
    NearbySubmitCodeRequest, OperationReply, PairCreateCodeReply, PairCreateCodeRequest,
    PairJoinReply, PairJoinRequest, PeerInfo, PeerListReply, RemovePeerRequest, SafeResetRequest,
    SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest, SendInputKeyRequest,
    SendInputMoveRequest, StatusReply, StatusRequest, TransportEvent, TransportEventsReply,
    TrustBundleReply, UiSnapshotReply,
    control_plane_service_server::ControlPlaneService,
    daemon_service_server::{DaemonService, DaemonServiceServer},
    diagnostics_service_server::{DiagnosticsService, DiagnosticsServiceServer},
    feature_service_server::{FeatureService, FeatureServiceServer},
    pairing_service_server::{PairingService, PairingServiceServer},
    topology_service_server::{TopologyService, TopologyServiceServer},
};

use crate::state::AppState;

mod control_plane_api;
mod daemon_api;
mod diagnostics_api;
mod feature_api;
mod pairing_api;
mod topology_api;

use control_plane_api::ControlPlaneApi;
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
