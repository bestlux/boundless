use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::Utc;
pub use peer_transport::{
    MAX_TRANSPORT_EVENTS, OutboundPayload, OutgoingPeerQueues, RuntimeWakeSignal,
    TransportEventRecord,
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::sync::{RwLock, watch};
use tracing::info;

use core_clipboard::{
    ClipboardPayload, ClipboardPolicy, ClipboardPolicyError, payload_hash_hex,
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
    load_or_create_device_secret, load_trust_records, remove_trust_record, upsert_trust_record,
};
use core_transfer::{resolve_conflict_path, validate_transfer_size};

use crate::config::{
    ApiTransport, PeerConfig, RuntimeConfig, config_path, load_or_create_config_at, save_config_at,
};

const MAX_PENDING_REMOTE_CLIPBOARD_ITEMS: usize = 64;
const MAX_PENDING_INJECT_INPUT_FRAMES: usize = 128;
const MAX_PENDING_OUTGOING_INPUT_FRAMES: usize = 128;
const MAX_PENDING_NEARBY_PAIRING_REQUESTS: usize = 128;
const MAX_PENDING_NEARBY_CODE_CHALLENGES: usize = 64;
const INPUT_OWNER_AUTO_STEAL_COOLDOWN_MS: u64 = 1_000;
const NEARBY_PAIRING_DECISION_RETENTION_MINUTES: i64 = 10;
const NEARBY_PAIRING_CHALLENGE_MAX_ATTEMPTS: u8 = 5;
const NEARBY_PAIRING_CODE_REQUEST_COOLDOWN_SECONDS: i64 = 3;
const NEARBY_PAIRING_CODE_SUBMISSION_FAILURE_WINDOW_SECONDS: i64 = 300;
const NEARBY_PAIRING_CODE_SUBMISSION_MAX_FAILURES: usize = 8;
const NEARBY_PAIRING_CODE_SUBMISSION_LOCKOUT_SECONDS: i64 = 600;
pub(crate) const FILE_TRANSFER_CHUNK_BYTES: usize = 48 * 1024;

mod clipboard_ops;
mod clipboard_state;
mod config_ops;
mod core_ops;
mod diagnostics_ops;
mod discovery_state;
mod input_ops;
mod input_state;
mod layout_resolver;
mod pairing_ops;
mod pairing_state;
mod peer_ops;
mod routing_helpers;
mod transport_ops;
mod transport_state;
mod validation;

pub(crate) use clipboard_state::PendingRemoteClipboardPayload;
use clipboard_state::{ClipboardReplayState, ClipboardState, ClipboardSyncState};
pub(crate) use discovery_state::DiscoveredPeerEndpoint;
use discovery_state::DiscoveryState;
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
use pairing_state::{
    NearbyPairingDecision, NearbyPairingDecisionRecord, PairingState, PendingNearbyPairingMode,
    PendingNearbyPairingRequestRecord,
};
pub(crate) use pairing_state::{NearbyPairingStatus, PendingNearbyPairingRequest};
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
    pub capture_timestamp_unix_ms: i64,
    pub received_timestamp_unix_ms: i64,
    pub queued_timestamp_unix_ms: i64,
    pub retry_count: u8,
    pub next_retry_at: Option<Instant>,
    pub events: Vec<InputEvent>,
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
    security_paths: Arc<SecurityPaths>,
    identity: Arc<DeviceIdentity>,
    device_fingerprint: Arc<String>,
    inbox_root: Arc<PathBuf>,
    parsed_layout_matrix_cache: Arc<RwLock<Option<ParsedLayoutMatrixCache>>>,
    input_capture_wake: Arc<RuntimeWakeSignal>,
    input_inject_wake: Arc<RuntimeWakeSignal>,
}

#[cfg(test)]
mod tests;
