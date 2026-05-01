use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCodeRequest {
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairJoinCommand {
    pub code: String,
    pub host: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutSetCommand {
    pub matrix_spec: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovePeerCommand {
    pub peer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSetCommand {
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAntiIdleConfigCommand {
    pub enabled: bool,
    pub recent_activity_window_secs: u32,
    pub allow_on_battery: bool,
    pub keep_display_on: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFileTransferConfigCommand {
    pub receive_dir: String,
    pub organize_by_peer: bool,
    pub auto_accept_trusted_peers: bool,
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetInputHandoffConfigCommand {
    pub block_screen_corners: bool,
    pub corner_block_px: u32,
    pub relative_mouse: bool,
    pub hide_cursor_at_edge: bool,
    pub draw_cursor_marker: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeySetCommand {
    pub action: String,
    pub combo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyTriggerCommand {
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportTrustBundleCommand {
    pub machine_id: String,
    pub display_name: String,
    pub network_address: String,
    pub ca_cert_pem: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsDumpCommand {
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeResetCommand {
    pub network_only: bool,
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendClipboardTextCommand {
    pub peer_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendClipboardImageCommand {
    pub peer_id: String,
    pub image_bmp: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendFileCommand {
    pub peer_id: String,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendInputMoveCommand {
    pub peer_id: String,
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendInputKeyCommand {
    pub peer_id: String,
    pub scan_code: u16,
    pub key_down: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputOwnerCommand {
    pub peer_id: String,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCaptureTargetCommand {
    pub peer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyRequestCodeCommand {
    pub host: String,
    pub port: u16,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbySubmitCodeCommand {
    pub host: String,
    pub port: u16,
    pub request_id: String,
    pub code: String,
    pub verification_nonce: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyJoinStartCommand {
    pub host: String,
    pub port: u16,
    pub code: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyJoinStatusCommand {
    pub host: String,
    pub port: u16,
    pub request_id: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearbyPairingDecisionCommand {
    pub request_id: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCodeReply {
    pub code: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairJoinReply {
    pub accepted: bool,
    pub peer_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationReply {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutReply {
    pub matrix_spec: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsDumpReply {
    pub bundle_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputOwnerReply {
    pub ok: bool,
    pub owner_peer_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCaptureTargetReply {
    pub ok: bool,
    pub peer_id: String,
    pub message: String,
}
