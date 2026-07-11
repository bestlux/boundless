use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use core_clipboard::ClipboardPayload;

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
pub struct FileTransferConfigSnapshot {
    pub receive_dir: String,
    pub organize_by_peer: bool,
    pub auto_accept_trusted_peers: bool,
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransferSnapshot {
    pub transfer_id: String,
    pub previous_transfer_id: Option<String>,
    pub direction: String,
    pub peer_id: String,
    pub file_name: String,
    pub state: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub failure_reason: Option<String>,
    pub source_path: Option<String>,
    pub final_path: Option<String>,
    pub queued_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputHandoffConfigSnapshot {
    pub block_screen_corners: bool,
    pub corner_block_px: u32,
    pub relative_mouse: bool,
    pub hide_cursor_at_edge: bool,
    pub draw_cursor_marker: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRuntimeSnapshot {
    pub owner_peer_id: Option<String>,
    pub configured_capture_target_peer_id: Option<String>,
    pub active_capture_target_peer_id: Option<String>,
    pub lock_active: bool,
    pub lock_supported: bool,
    pub capture_backend_mode: String,
    pub pending_inject_frames: usize,
    pub pending_inject_high_water: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardRuntimeSnapshot {
    pub backend_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputBrokerAttachSnapshot {
    pub accepted: bool,
    pub broker_token: String,
    pub message: String,
    pub protocol_revision: u32,
    pub delivery_epoch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputBrokerInjectFrameSnapshot {
    pub source_peer_id: String,
    pub sequence: u64,
    pub events: Vec<core_input::InputEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputBrokerExchangeSnapshot {
    pub accepted: bool,
    pub message: String,
    pub inject_frames: Vec<InputBrokerInjectFrameSnapshot>,
    pub lock_should_be_active: bool,
    pub capture_active: bool,
    pub capture_forwarding_authorized: bool,
    pub inject_batch_id: u64,
    pub inject_batch_cancelled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipboardBrokerExchangeSnapshot {
    pub accepted: bool,
    pub message: String,
    pub remote_payload: Option<ClipboardPayload>,
    pub remote_source_peer_id: String,
    pub remote_hash: String,
    pub local_payload_disposition: ClipboardBrokerLocalPayloadDispositionSnapshot,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardBrokerLocalPayloadDispositionSnapshot {
    #[default]
    NotSubmitted,
    Accepted,
    TransientRejected,
    DeterministicRejected,
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
    #[serde(default)]
    pub endpoint_candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPairedPeer {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    pub connected: bool,
    pub health_state: String,
    pub health_reason: String,
    pub trust_state: String,
    pub trusted_since: String,
    pub trust_fingerprint: String,
    pub device_identity: String,
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
    pub role: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiSnapshot {
    pub generated_at: String,
    pub daemon_online: bool,
    pub machine_id: String,
    pub layout_matrix: String,
    pub features: BTreeMap<String, bool>,
    pub hotkeys: BTreeMap<String, String>,
    pub discovered_peers: Vec<UiDiscoveredPeer>,
    pub paired_peers: Vec<UiPairedPeer>,
    pub pending_requests: Vec<UiPendingRequest>,
    pub anti_idle_config: AntiIdleConfigSnapshot,
    pub anti_idle_status: AntiIdleStatusSnapshot,
    pub file_transfer_config: FileTransferConfigSnapshot,
    pub file_transfers: Vec<FileTransferSnapshot>,
    pub input_handoff_config: InputHandoffConfigSnapshot,
    pub input_runtime: InputRuntimeSnapshot,
    pub clipboard_runtime: ClipboardRuntimeSnapshot,
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
    pub file_transfer_config: FileTransferConfigSnapshot,
    pub file_transfers: Vec<FileTransferSnapshot>,
    pub input_handoff_config: InputHandoffConfigSnapshot,
    pub input_runtime: InputRuntimeSnapshot,
    pub clipboard_runtime: ClipboardRuntimeSnapshot,
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
    pub role: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyPairingCompletionSnapshot {
    pub ok: bool,
    pub message: String,
    pub request_id: String,
    pub peer_machine_id: String,
    pub trust_committed: bool,
    pub already_committed: bool,
    pub reconnect_status: String,
}
