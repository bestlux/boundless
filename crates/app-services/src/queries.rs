use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiIdleConfigSnapshot {
    pub enabled: bool,
    pub recent_activity_window_secs: u32,
    pub allow_on_battery: bool,
    pub keep_display_on: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiIdleStatusSnapshot {
    pub supported: bool,
    pub enabled: bool,
    pub active: bool,
    pub display_required: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub daemon_version: String,
    pub machine_id: String,
    pub peer_count: u32,
    pub protocol_version: String,
    pub api_bind: String,
    pub api_transport: String,
    pub api_pipe_name: String,
    pub input_locked: bool,
    pub input_lock_supported: bool,
    pub capture_target_peer_id: Option<String>,
    pub anti_idle_supported: bool,
    pub anti_idle_enabled: bool,
    pub anti_idle_active: bool,
    pub anti_idle_display_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDiscoveredPeer {
    pub machine_id: String,
    pub display_name: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPairedPeer {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPendingRequest {
    pub request_id: String,
    pub requester_machine_id: String,
    pub requester_display_name: String,
    pub created_at: String,
    pub verification_code: String,
    pub verification_expires_at: String,
    pub requires_verification_code: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub generated_at: String,
    pub daemon_online: bool,
    pub machine_id: String,
    pub layout_matrix: String,
    pub discovered_peers: Vec<UiDiscoveredPeer>,
    pub paired_peers: Vec<UiPairedPeer>,
    pub pending_requests: Vec<UiPendingRequest>,
    pub anti_idle_config: AntiIdleConfigSnapshot,
    pub anti_idle_status: AntiIdleStatusSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleSnapshot {
    pub status: StatusSnapshot,
    pub layout_matrix: String,
    pub peers: Vec<UiPairedPeer>,
    pub features: BTreeMap<String, bool>,
    pub discovered_peers: Vec<UiDiscoveredPeer>,
    pub pending_requests: Vec<UiPendingRequest>,
    pub transport_events: Vec<TransportEventSnapshot>,
    pub input_owner_peer_id: Option<String>,
    pub input_capture_target_peer_id: Option<String>,
    pub mdns_active: bool,
    pub local_display_name: String,
    pub anti_idle_config: AntiIdleConfigSnapshot,
    pub anti_idle_status: AntiIdleStatusSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustBundleSnapshot {
    pub machine_id: String,
    pub display_name: String,
    pub network_address: String,
    pub ca_cert_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportEventSnapshot {
    pub timestamp: String,
    pub direction: String,
    pub kind: String,
    pub peer_id: String,
    pub detail: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyRequestCodeStartSnapshot {
    pub code_required: bool,
    pub request_id: String,
    pub verification_nonce: String,
    pub verification_expires_at: String,
    pub unsupported: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyJoinStatusSnapshot {
    pub request_id: String,
    pub status: String,
    pub message: String,
    pub peer_machine_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyPairingCompletionSnapshot {
    pub ok: bool,
    pub message: String,
    pub request_id: String,
    pub peer_machine_id: String,
}
