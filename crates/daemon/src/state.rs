use std::{
    collections::BTreeMap,
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use chrono::Utc;
pub use peer_transport::{
    MAX_TRANSPORT_EVENTS, OutboundPayload, OutgoingPeerQueues, RuntimeWakeSignal,
    TransportEventRecord, TransportSessionClaim,
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::sync::{Mutex, RwLock, watch};
use tracing::info;

use core_clipboard::{
    BmpValidationError, ClipboardPayload, ClipboardPolicy, ClipboardPolicyError, payload_hash_hex,
    validate_bmp_payload, validate_payload,
};
use core_discovery::parse_manual_target;
use core_input::{
    EasyMouseMode, InputEvent, InputFrame, InputRouter, InputSink, KeyState, MAX_EVENTS_PER_FRAME,
    RouteDecision, SwitchDirection,
};
use core_security::{
    DeviceIdentity, SecurityPaths, TrustBundle, TrustRecord, default_security_root,
    ensure_device_identity, ensure_trust_store, fingerprint, generate_pairing_code,
    load_or_create_device_secret, load_trust_records, remove_trust_record, rotate_device_identity,
    upsert_trust_record,
};
use core_transfer::validate_transfer_size_with_limit;

use crate::config::{
    AntiIdleConfig, ApiTransport, FileTransferConfig, InputHandoffConfig, PeerConfig,
    RuntimeConfig, config_path, load_or_create_config_at, save_config_at,
};
use crate::runtime_tasks::{RuntimeTaskRegistry, RuntimeTaskSnapshot, RuntimeTaskSpec};

const MAX_PENDING_REMOTE_CLIPBOARD_ITEMS: usize = 64;
const MAX_PENDING_INJECT_INPUT_FRAMES: usize = 128;
const MAX_PENDING_OUTGOING_INPUT_FRAMES: usize = 128;
const MAX_PENDING_NEARBY_PAIRING_REQUESTS: usize = 128;
const MAX_PENDING_NEARBY_CODE_CHALLENGES: usize = 64;
const MAX_PENDING_NEARBY_PAIRING_REQUESTS_PER_PEER: usize = 2;
const MAX_PENDING_NEARBY_PAIRING_REQUESTS_PER_SOURCE: usize = 8;
const MAX_FILE_TRANSFER_RECORDS: usize = 128;
const INPUT_OWNER_AUTO_STEAL_COOLDOWN_MS: u64 = 1_000;
const NEARBY_PAIRING_PENDING_REQUEST_TTL_SECONDS: i64 = 600;
const NEARBY_PAIRING_DECISION_RETENTION_MINUTES: i64 = 10;
const NEARBY_PAIRING_CHALLENGE_MAX_ATTEMPTS: u8 = 5;
const NEARBY_PAIRING_CODE_REQUEST_COOLDOWN_SECONDS: i64 = 3;
const NEARBY_PAIRING_CODE_SUBMISSION_FAILURE_WINDOW_SECONDS: i64 = 300;
const NEARBY_PAIRING_CODE_SUBMISSION_MAX_FAILURES: usize = 8;
const NEARBY_PAIRING_CODE_SUBMISSION_LOCKOUT_SECONDS: i64 = 600;
pub(crate) const FILE_TRANSFER_CHUNK_BYTES: usize = 48 * 1024;

mod anti_idle_ops;
mod anti_idle_state;
mod clipboard_ops;
mod clipboard_state;
mod config_ops;
mod control_plane_snapshot_ops;
mod core_ops;
mod diagnostics_ops;
mod discovery_state;
mod input_broker;
mod input_broker_ops;
mod input_ops;
mod input_state;
mod layout_resolver;
mod pairing_ops;
mod pairing_state;
mod peer_ops;
mod routing_helpers;
mod transfer_center_ops;
mod transport_ops;
mod transport_state;
mod validation;

pub(crate) use anti_idle_state::AntiIdleRuntimeState;
use anti_idle_state::AntiIdleState;
pub(crate) use clipboard_state::PendingRemoteClipboardPayload;
use clipboard_state::{ClipboardReplayState, ClipboardState, ClipboardSyncState};
pub(crate) use discovery_state::DiscoveredPeerEndpoint;
use discovery_state::DiscoveryState;
pub(crate) use input_broker::{
    CLIPBOARD_BROKER_UNAVAILABLE_MODE, CLIPBOARD_DIRECT_BACKEND_MODE,
    CLIPBOARD_USER_SESSION_BROKER_MODE, INPUT_BROKER_BACKEND_MODE, InputBrokerRelay,
    SERVICE_SESSION_UNSUPPORTED_BACKEND_MODE,
};
pub use input_broker_ops::{
    ClipboardBrokerApplyReport, ClipboardBrokerExchangeOutcome,
    ClipboardBrokerLocalPayloadDisposition, InputBrokerAttachOutcome, InputBrokerClientIdentity,
    InputBrokerExchangeObservations, InputBrokerExchangeOutcome,
};
use input_state::InputState;
#[cfg(test)]
use layout_resolver::resolve_capture_handoff_target;
use layout_resolver::{
    parse_layout_matrix, resolve_capture_handoff_target_with_fallback_from_matrix,
    resolve_switch_all_target_order_from_matrix,
};
#[cfg(test)]
use layout_resolver::{
    resolve_capture_handoff_target_with_fallback, resolve_switch_all_target_order,
};
pub(crate) use pairing_state::{
    NearbyPairingCommitResult, NearbyPairingStatus, PendingNearbyPairingRequest,
};
use pairing_state::{
    NearbyPairingDecision, NearbyPairingDecisionRecord, PairingState, PendingNearbyPairingMode,
    PendingNearbyPairingRequestRecord,
};
use routing_helpers::{describe_input_frame_decision, elapsed_ms};
use transport_state::TransportState;
use validation::{
    normalize_optional_alias, normalize_peer_address, sanitize_incoming_file_name,
    validate_and_consume_pairing_code, validate_bind_address, validate_ca_cert_pem,
    validate_pipe_name,
};

#[derive(Debug, Clone, Copy)]
pub struct InputFrameTiming {
    pub capture_timestamp_unix_ms: i64,
    pub received_timestamp_unix_ms: i64,
    pub queued_timestamp_unix_ms: i64,
}

#[derive(Debug, Clone)]
pub struct PendingInjectInputFrame {
    pub peer_id: String,
    pub sequence: u64,
    pub authorization_generation: u64,
    pub capture_timestamp_unix_ms: i64,
    pub received_timestamp_unix_ms: i64,
    pub queued_timestamp_unix_ms: i64,
    pub retry_count: u8,
    pub next_retry_at: Option<Instant>,
    pub events: Vec<InputEvent>,
}

struct OutboundFileTransfer {
    peer_id: String,
    file_name: String,
    source_path: PathBuf,
    total_bytes: u64,
    source_modified: Option<SystemTime>,
    offset_bytes: u64,
    source_file: Option<tokio::fs::File>,
}

#[derive(Debug, Clone)]
pub(crate) struct OutgoingFileTransferProjection {
    pub(crate) transfer_id: String,
    pub(crate) previous_transfer_id: Option<String>,
    pub(crate) peer_id: String,
    pub(crate) file_name: String,
    pub(crate) source_path: PathBuf,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileTransferDirection {
    Incoming,
    Outgoing,
}

impl FileTransferDirection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileTransferState {
    Queued,
    Active,
    Completed,
    Failed,
    Cancelled,
}

impl FileTransferState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FileTransferRecord {
    pub(crate) transfer_id: String,
    pub(crate) previous_transfer_id: Option<String>,
    pub(crate) direction: FileTransferDirection,
    pub(crate) peer_id: String,
    pub(crate) file_name: String,
    pub(crate) state: FileTransferState,
    pub(crate) transferred_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) failure_reason: Option<String>,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) final_path: Option<PathBuf>,
    pub(crate) queued_at: chrono::DateTime<Utc>,
    pub(crate) updated_at: chrono::DateTime<Utc>,
}

pub(crate) struct OutboundFileChunk {
    pub(crate) transfer_id: String,
    pub(crate) offset_bytes: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) finished: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ControlPlaneSnapshotBundle {
    pub(crate) config: RuntimeConfig,
    pub(crate) peers: Vec<PeerConfig>,
    pub(crate) layout_matrix: String,
    pub(crate) features: BTreeMap<String, bool>,
    pub(crate) discovered_endpoints: Vec<(String, DiscoveredPeerEndpoint)>,
    pub(crate) pending_requests: Vec<PendingNearbyPairingRequest>,
    pub(crate) trusted_records: Vec<TrustRecord>,
    pub(crate) transport_events: Vec<TransportEventRecord>,
    pub(crate) input_owner_peer_id: Option<String>,
    pub(crate) input_capture_target_peer_id: Option<String>,
    pub(crate) active_input_capture_target_peer_id: Option<String>,
    pub(crate) input_locked: bool,
    pub(crate) input_lock_supported: bool,
    pub(crate) mdns_active: bool,
    pub(crate) anti_idle_config: AntiIdleConfig,
    pub(crate) anti_idle_runtime: AntiIdleRuntimeState,
    pub(crate) input_handoff_config: InputHandoffConfig,
    pub(crate) input_capture_backend_mode: String,
    pub(crate) clipboard_backend_mode: String,
    pub(crate) pending_inject_frames: usize,
    pub(crate) pending_inject_high_water: usize,
    pub(crate) file_transfers: Vec<FileTransferRecord>,
}

impl PendingInjectInputFrame {
    pub fn timing(&self) -> InputFrameTiming {
        InputFrameTiming {
            capture_timestamp_unix_ms: self.capture_timestamp_unix_ms,
            received_timestamp_unix_ms: self.received_timestamp_unix_ms,
            queued_timestamp_unix_ms: self.queued_timestamp_unix_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureHandoffTarget {
    Local,
    Peer(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntiIdleAssertionReason {
    None,
    LocalRecentInput,
    RemoteRecentInput,
}

#[derive(Debug, Clone, Copy)]
pub struct AntiIdleOutboundPulse {
    pub keep_display_on: bool,
    pub interval: Duration,
}

impl AntiIdleAssertionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LocalRecentInput => "local_recent_input",
            Self::RemoteRecentInput => "remote_recent_input",
        }
    }
}

#[derive(Debug, Clone)]
struct ParsedLayoutMatrixCache {
    spec: String,
    matrix: Arc<Vec<Vec<String>>>,
}

#[derive(Clone)]
pub struct AppState {
    config_path: Arc<PathBuf>,
    config: Arc<RwLock<RuntimeConfig>>,
    clipboard: Arc<ClipboardState>,
    pairing: Arc<PairingState>,
    transport: Arc<TransportState>,
    discovery: Arc<DiscoveryState>,
    input: Arc<InputState>,
    input_broker: Arc<InputBrokerRelay>,
    pub(crate) input_capture_transition: Arc<Mutex<()>>,
    anti_idle: Arc<AntiIdleState>,
    outbound_file_transfers: Arc<RwLock<HashMap<String, OutboundFileTransfer>>>,
    file_transfer_records: Arc<RwLock<VecDeque<FileTransferRecord>>>,
    security_paths: Arc<SecurityPaths>,
    identity: Arc<DeviceIdentity>,
    device_fingerprint: Arc<String>,
    trust_rotation_pending_restart: Arc<AtomicBool>,
    parsed_layout_matrix_cache: Arc<RwLock<Option<ParsedLayoutMatrixCache>>>,
    input_capture_wake: Arc<RuntimeWakeSignal>,
    input_inject_wake: Arc<RuntimeWakeSignal>,
    anti_idle_wake: Arc<RuntimeWakeSignal>,
    runtime_tasks: RuntimeTaskRegistry,
}

#[cfg(test)]
mod tests;
