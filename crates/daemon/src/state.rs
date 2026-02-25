use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::{
    sync::{RwLock, watch},
    task::AbortHandle,
};
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

const MAX_TRANSPORT_EVENTS: usize = 512;
const MAX_PENDING_REMOTE_CLIPBOARD_ITEMS: usize = 64;
const MAX_PENDING_INJECT_INPUT_FRAMES: usize = 128;
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
mod config_ops;
mod diagnostics_ops;
mod input_ops;
mod layout_resolver;
mod pairing_ops;
mod peer_ops;
mod routing_helpers;
mod transport_ops;
mod validation;

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
use routing_helpers::{describe_input_frame_decision, elapsed_ms};
use validation::{
    normalize_optional_alias, normalize_peer_address, sanitize_incoming_file_name,
    validate_and_consume_pairing_code, validate_bind_address, validate_ca_cert_pem,
    validate_pipe_name,
};

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    ClipboardText {
        text: String,
    },
    ClipboardImage {
        image_bmp: Vec<u8>,
    },
    FileStart {
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        source_path: PathBuf,
        offset_bytes: u64,
        length_bytes: usize,
    },
    FileEnd {
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    InputFrame {
        sequence: u64,
        timestamp_unix_ms: i64,
        events: Vec<InputEvent>,
    },
}

#[derive(Debug, Default)]
struct OutgoingPeerQueues {
    input: VecDeque<OutboundPayload>,
    bulk: VecDeque<OutboundPayload>,
}

#[derive(Debug, Clone)]
pub struct TransportEventRecord {
    pub timestamp: DateTime<Utc>,
    pub direction: String,
    pub kind: String,
    pub peer_id: String,
    pub detail: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct PendingRemoteClipboardPayload {
    pub peer_id: String,
    pub payload: ClipboardPayload,
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct DiscoveredPeerEndpoint {
    pub display_name: String,
    pub endpoint: SocketAddr,
}

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

#[derive(Debug, Clone)]
pub struct PendingNearbyPairingRequest {
    pub request_id: String,
    pub requester_machine_id: String,
    pub requester_display_name: String,
    pub created_at: DateTime<Utc>,
    pub verification_code: Option<String>,
    pub verification_nonce: Option<String>,
    pub verification_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub enum NearbyPairingStatus {
    Pending,
    Approved { responder_bundle: TrustBundle },
    Rejected { message: String },
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureHandoffTarget {
    Local,
    Peer(String),
}

#[derive(Debug, Clone)]
struct PendingNearbyPairingRequestRecord {
    summary: PendingNearbyPairingRequest,
    requester_bundle: TrustBundle,
    requester_alias: Option<String>,
    mode: PendingNearbyPairingMode,
}

#[derive(Debug, Clone)]
enum PendingNearbyPairingMode {
    ManualApproval,
    CodeChallenge {
        code: String,
        nonce: String,
        expires_at: DateTime<Utc>,
        attempts_left: u8,
    },
}

#[derive(Debug, Clone)]
enum NearbyPairingDecision {
    Approved { responder_bundle: TrustBundle },
    Rejected { message: String },
}

#[derive(Debug, Clone)]
struct NearbyPairingDecisionRecord {
    decision: NearbyPairingDecision,
    decided_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct ClipboardSyncState {
    last_observed_hash: Option<String>,
    suppress_echo_hash: Option<String>,
    pending_remote: VecDeque<PendingRemoteClipboardPayload>,
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
    pairing_codes: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    security_paths: Arc<SecurityPaths>,
    identity: Arc<DeviceIdentity>,
    device_fingerprint: Arc<String>,
    outgoing_input_payloads: Arc<RwLock<HashMap<String, VecDeque<OutboundPayload>>>>,
    outgoing_bulk_payloads: Arc<RwLock<HashMap<String, VecDeque<OutboundPayload>>>>,
    transport_events: Arc<std::sync::Mutex<VecDeque<TransportEventRecord>>>,
    clipboard_sync: Arc<RwLock<ClipboardSyncState>>,
    discovered_endpoints: Arc<RwLock<HashMap<String, DiscoveredPeerEndpoint>>>,
    mdns_active: Arc<RwLock<bool>>,
    inbox_root: Arc<PathBuf>,
    parsed_layout_matrix_cache: Arc<RwLock<Option<ParsedLayoutMatrixCache>>>,
    input_router: Arc<RwLock<InputRouter>>,
    input_sequence_by_peer: Arc<RwLock<HashMap<String, u64>>>,
    pending_inject_input_frames: Arc<RwLock<VecDeque<PendingInjectInputFrame>>>,
    input_capture_target_peer_id: Arc<RwLock<Option<String>>>,
    input_owner_last_changed_at: Arc<RwLock<Option<Instant>>>,
    input_lock_active: Arc<RwLock<bool>>,
    input_lock_supported: Arc<RwLock<bool>>,
    reconnect_generation_by_peer: Arc<RwLock<HashMap<String, u64>>>,
    outgoing_flush_signal: watch::Sender<u64>,
    outgoing_flush_generation: Arc<std::sync::atomic::AtomicU64>,
    pending_nearby_pairing_requests:
        Arc<RwLock<HashMap<String, PendingNearbyPairingRequestRecord>>>,
    nearby_pairing_decisions: Arc<RwLock<HashMap<String, NearbyPairingDecisionRecord>>>,
    nearby_code_request_last_seen_by_ip: Arc<RwLock<HashMap<IpAddr, DateTime<Utc>>>>,
    nearby_code_submission_failures_by_ip: Arc<RwLock<HashMap<IpAddr, Vec<DateTime<Utc>>>>>,
    nearby_code_submission_lockout_by_ip: Arc<RwLock<HashMap<IpAddr, DateTime<Utc>>>>,
    pending_transport_session_abort_handles: Arc<RwLock<HashMap<u64, AbortHandle>>>,
    transport_session_abort_handles_by_peer:
        Arc<RwLock<HashMap<String, HashMap<u64, AbortHandle>>>>,
    next_transport_session_id: Arc<std::sync::atomic::AtomicU64>,
}

impl AppState {
    pub fn load_or_create() -> Result<Self> {
        let config_path = config_path();
        let security_root = default_security_root();
        Self::load_or_create_with_paths(config_path, security_root)
    }

    pub fn load_or_create_with_paths(config_path: PathBuf, security_root: PathBuf) -> Result<Self> {
        let config = load_or_create_config_at(&config_path)?;

        let paths = SecurityPaths::for_root(security_root);
        let secret = load_or_create_device_secret(&paths)?;
        ensure_trust_store(&paths)?;
        let advertised_host = std::env::var("BOUNDLESS_ADVERTISE_HOST").ok();
        let identity = ensure_device_identity(
            &paths,
            &config.machine_id,
            &config.device_name,
            advertised_host.as_deref(),
        )?;

        // Ensure self trust record exists. This enables symmetric mTLS setups and local test loops.
        upsert_trust_record(
            &paths,
            TrustRecord {
                machine_id: config.machine_id.clone(),
                ca_cert_pem: identity.ca_cert_pem.clone(),
                added_at: Utc::now(),
            },
        )?;

        let inbox_root = if let Ok(path) = std::env::var("BOUNDLESS_INBOX_ROOT") {
            PathBuf::from(path)
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Boundless")
                .join("inbox")
        };
        std::fs::create_dir_all(&inbox_root)?;

        let fingerprint = fingerprint(&secret);

        info!(
            machine_id = %config.machine_id,
            config_path = %config_path.display(),
            security_root = %paths.root.display(),
            inbox_root = %inbox_root.display(),
            "state loaded"
        );

        let input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        let (outgoing_flush_signal, _outgoing_flush_rx) = watch::channel(0u64);

        Ok(Self {
            config_path: Arc::new(config_path),
            config: Arc::new(RwLock::new(config)),
            pairing_codes: Arc::new(RwLock::new(HashMap::new())),
            security_paths: Arc::new(paths),
            identity: Arc::new(identity),
            device_fingerprint: Arc::new(fingerprint),
            outgoing_input_payloads: Arc::new(RwLock::new(HashMap::new())),
            outgoing_bulk_payloads: Arc::new(RwLock::new(HashMap::new())),
            transport_events: Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
                MAX_TRANSPORT_EVENTS,
            ))),
            clipboard_sync: Arc::new(RwLock::new(ClipboardSyncState::default())),
            discovered_endpoints: Arc::new(RwLock::new(HashMap::new())),
            mdns_active: Arc::new(RwLock::new(false)),
            inbox_root: Arc::new(inbox_root),
            parsed_layout_matrix_cache: Arc::new(RwLock::new(None)),
            input_router: Arc::new(RwLock::new(InputRouter::new(input_enabled))),
            input_sequence_by_peer: Arc::new(RwLock::new(HashMap::new())),
            pending_inject_input_frames: Arc::new(RwLock::new(VecDeque::new())),
            input_capture_target_peer_id: Arc::new(RwLock::new(None)),
            input_owner_last_changed_at: Arc::new(RwLock::new(None)),
            input_lock_active: Arc::new(RwLock::new(false)),
            input_lock_supported: Arc::new(RwLock::new(cfg!(windows))),
            reconnect_generation_by_peer: Arc::new(RwLock::new(HashMap::new())),
            outgoing_flush_signal,
            outgoing_flush_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_nearby_pairing_requests: Arc::new(RwLock::new(HashMap::new())),
            nearby_pairing_decisions: Arc::new(RwLock::new(HashMap::new())),
            nearby_code_request_last_seen_by_ip: Arc::new(RwLock::new(HashMap::new())),
            nearby_code_submission_failures_by_ip: Arc::new(RwLock::new(HashMap::new())),
            nearby_code_submission_lockout_by_ip: Arc::new(RwLock::new(HashMap::new())),
            pending_transport_session_abort_handles: Arc::new(RwLock::new(HashMap::new())),
            transport_session_abort_handles_by_peer: Arc::new(RwLock::new(HashMap::new())),
            next_transport_session_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    pub fn fingerprint(&self) -> &str {
        self.device_fingerprint.as_ref().as_str()
    }

    pub fn identity(&self) -> &DeviceIdentity {
        self.identity.as_ref()
    }

    pub async fn trusted_records(&self) -> Result<Vec<TrustRecord>> {
        load_trust_records(&self.security_paths)
    }

    pub async fn export_trust_bundle(&self) -> Result<TrustBundle> {
        let snapshot = self.snapshot().await;

        let advertised_host = std::env::var("BOUNDLESS_ADVERTISE_HOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| snapshot.device_name.clone());

        Ok(TrustBundle {
            machine_id: snapshot.machine_id,
            display_name: snapshot.device_name,
            network_address: format!("{advertised_host}:{}", snapshot.network_port),
            ca_cert_pem: self.identity.ca_cert_pem.clone(),
        })
    }

    pub async fn import_trust_bundle(
        &self,
        bundle: TrustBundle,
        alias: Option<String>,
    ) -> Result<()> {
        validate_ca_cert_pem(&bundle.ca_cert_pem)?;
        let default_port = self.config.read().await.network_port;
        let normalized_address = normalize_peer_address(&bundle.network_address, default_port)?;

        upsert_trust_record(
            &self.security_paths,
            TrustRecord {
                machine_id: bundle.machine_id.clone(),
                ca_cert_pem: bundle.ca_cert_pem.clone(),
                added_at: Utc::now(),
            },
        )?;

        let mut config = self.config.write().await;

        if let Some(peer) = config
            .peers
            .iter_mut()
            .find(|p| p.peer_id == bundle.machine_id)
        {
            peer.address = normalized_address;
            peer.display_name = alias.unwrap_or(bundle.display_name);
            peer.connected = false;
            peer.last_seen = Utc::now();
        } else {
            config.peers.push(PeerConfig {
                peer_id: bundle.machine_id,
                display_name: alias.unwrap_or(bundle.display_name),
                address: normalized_address,
                connected: false,
                last_seen: Utc::now(),
            });
        }

        save_config_at(&self.config_path, &config)
    }

    pub async fn snapshot(&self) -> RuntimeConfig {
        self.config.read().await.clone()
    }

    pub(crate) async fn cached_layout_matrix_for_spec(&self, spec: &str) -> Arc<Vec<Vec<String>>> {
        if let Some(cached) = self.parsed_layout_matrix_cache.read().await.as_ref()
            && cached.spec == spec
        {
            return cached.matrix.clone();
        }

        let parsed = Arc::new(parse_layout_matrix(spec));
        let mut cache = self.parsed_layout_matrix_cache.write().await;
        if let Some(cached) = cache.as_ref()
            && cached.spec == spec
        {
            return cached.matrix.clone();
        }
        *cache = Some(ParsedLayoutMatrixCache {
            spec: spec.to_string(),
            matrix: parsed.clone(),
        });
        parsed
    }

    pub(crate) async fn invalidate_cached_layout_matrix(&self) {
        *self.parsed_layout_matrix_cache.write().await = None;
    }

    pub fn subscribe_outgoing_flush_signal(&self) -> watch::Receiver<u64> {
        self.outgoing_flush_signal.subscribe()
    }

    pub(crate) fn notify_outgoing_flush_signal(&self) {
        let next = self
            .outgoing_flush_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let _ = self.outgoing_flush_signal.send(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn minimal_bmp_payload(red: u8) -> Vec<u8> {
        vec![
            b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, red, 0,
        ]
    }

    #[test]
    fn resolve_capture_handoff_target_uses_layout_neighbors() {
        let config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: ",up,;left,self,right;,down,".to_string(),
            peers: vec![
                PeerConfig {
                    peer_id: "peer-left".to_string(),
                    display_name: "left".to_string(),
                    address: "127.0.0.1:15100".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-right".to_string(),
                    display_name: "right".to_string(),
                    address: "127.0.0.1:15101".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-up".to_string(),
                    display_name: "up".to_string(),
                    address: "127.0.0.1:15102".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-down".to_string(),
                    display_name: "down".to_string(),
                    address: "127.0.0.1:15103".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Left),
            Some(CaptureHandoffTarget::Peer("peer-left".to_string()))
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Right),
            Some(CaptureHandoffTarget::Peer("peer-right".to_string()))
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Up),
            Some(CaptureHandoffTarget::Peer("peer-up".to_string()))
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Down),
            Some(CaptureHandoffTarget::Peer("peer-down".to_string()))
        );
    }

    #[test]
    fn resolve_capture_handoff_target_ignores_disconnected_and_requires_single_local_cell() {
        let mut config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: "local,right".to_string(),
            peers: vec![PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: false,
                last_seen: Utc::now(),
            }],
            ..Default::default()
        };
        assert!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Right).is_none(),
            "disconnected neighbors should not be selected"
        );

        config.peers[0].connected = true;
        config.layout_matrix = "self,right;local,right".to_string();
        assert!(
            resolve_capture_handoff_target(&config, None, SwitchDirection::Right).is_none(),
            "multiple local cells should invalidate edge handoff resolution"
        );
    }

    #[test]
    fn resolve_capture_handoff_target_supports_peer_chain_and_return_to_local() {
        let config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: "left,self,right".to_string(),
            peers: vec![
                PeerConfig {
                    peer_id: "peer-left".to_string(),
                    display_name: "left".to_string(),
                    address: "127.0.0.1:15100".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-right".to_string(),
                    display_name: "right".to_string(),
                    address: "127.0.0.1:15101".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            resolve_capture_handoff_target(&config, Some("peer-left"), SwitchDirection::Right),
            Some(CaptureHandoffTarget::Local)
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, Some("peer-right"), SwitchDirection::Left),
            Some(CaptureHandoffTarget::Local)
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, Some("peer-left"), SwitchDirection::Left),
            None
        );
    }

    #[test]
    fn resolve_capture_handoff_target_with_fallback_switches_single_peer_when_layout_is_unusable() {
        let config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: "A,B;C,D".to_string(),
            peers: vec![PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: true,
                last_seen: Utc::now(),
            }],
            ..Default::default()
        };

        assert_eq!(
            resolve_capture_handoff_target_with_fallback(&config, None, SwitchDirection::Right),
            Some(CaptureHandoffTarget::Peer("peer-right".to_string()))
        );
        assert_eq!(
            resolve_capture_handoff_target_with_fallback(
                &config,
                Some("peer-right"),
                SwitchDirection::Left
            ),
            Some(CaptureHandoffTarget::Local)
        );
    }

    #[test]
    fn resolve_capture_handoff_target_with_fallback_respects_actionable_layout_edges() {
        let config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: "self,right".to_string(),
            peers: vec![PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: true,
                last_seen: Utc::now(),
            }],
            ..Default::default()
        };

        assert_eq!(
            resolve_capture_handoff_target_with_fallback(
                &config,
                Some("peer-right"),
                SwitchDirection::Right
            ),
            None,
            "with actionable layout, pushing deeper into the same edge should stay unresolved"
        );
    }

    #[test]
    fn resolve_switch_all_target_order_prefers_layout_then_connected_remainder() {
        let config = RuntimeConfig {
            machine_id: "local-id".to_string(),
            device_name: "local-device".to_string(),
            layout_matrix: "right,self,left".to_string(),
            peers: vec![
                PeerConfig {
                    peer_id: "peer-left".to_string(),
                    display_name: "left".to_string(),
                    address: "127.0.0.1:15100".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-right".to_string(),
                    display_name: "right".to_string(),
                    address: "127.0.0.1:15101".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
                PeerConfig {
                    peer_id: "peer-zeta".to_string(),
                    display_name: "zeta".to_string(),
                    address: "127.0.0.1:15102".to_string(),
                    connected: true,
                    last_seen: Utc::now(),
                },
            ],
            ..Default::default()
        };

        let order = resolve_switch_all_target_order(&config);
        assert_eq!(
            order,
            vec![
                "peer-right".to_string(),
                "peer-left".to_string(),
                "peer-zeta".to_string()
            ]
        );
    }

    #[test]
    fn validate_bind_address_rejects_invalid_input() {
        let err = validate_bind_address("not-an-addr").expect_err("must fail");
        assert!(err.to_string().contains("invalid bind address"));
    }

    #[test]
    fn validate_bind_address_accepts_socket_addr() {
        validate_bind_address("127.0.0.1:50051").expect("valid bind");
    }

    #[test]
    fn normalize_peer_address_adds_default_port_for_ip() {
        let normalized = normalize_peer_address("127.0.0.1", 15100).expect("normalize");
        assert_eq!(normalized, "127.0.0.1:15100");
    }

    #[test]
    fn normalize_peer_address_keeps_hostname_with_port() {
        let normalized = normalize_peer_address("node-a.local:15100", 15100).expect("normalize");
        assert_eq!(normalized, "node-a.local:15100");
    }

    #[test]
    fn normalize_peer_address_rejects_empty_input() {
        let err = normalize_peer_address("   ", 15100).expect_err("must fail");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_pipe_name_rejects_empty() {
        let err = validate_pipe_name("   ").expect_err("must fail");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_pipe_name_rejects_path_separators() {
        let err = validate_pipe_name("bad/name").expect_err("must fail");
        assert!(err.to_string().contains("path separators"));
    }

    #[test]
    fn validate_pipe_name_accepts_plain_name() {
        validate_pipe_name("boundlessd-api").expect("must accept");
    }

    #[test]
    fn pairing_code_validation_consumes_valid_code() {
        let now = Utc::now();
        let mut codes = HashMap::from([("ABC-123".to_string(), now + Duration::minutes(5))]);

        validate_and_consume_pairing_code(&mut codes, "ABC-123", now).expect("must pass");
        assert!(codes.is_empty(), "valid code should be consumed");
    }

    #[test]
    fn pairing_code_validation_rejects_unknown_code() {
        let now = Utc::now();
        let mut codes = HashMap::new();

        let err =
            validate_and_consume_pairing_code(&mut codes, "MISSING", now).expect_err("must reject");
        assert!(err.to_string().contains("invalid or was already used"));
    }

    #[test]
    fn pairing_code_validation_rejects_and_consumes_expired_code() {
        let now = Utc::now();
        let mut codes = HashMap::from([("ABC-123".to_string(), now - Duration::minutes(1))]);

        let err =
            validate_and_consume_pairing_code(&mut codes, "ABC-123", now).expect_err("must reject");
        assert!(err.to_string().contains("has expired"));
        assert!(codes.is_empty(), "expired code should be consumed");
    }

    #[test]
    fn ca_pem_validation_rejects_invalid_pem() {
        let err = validate_ca_cert_pem("not-pem").expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("certificate") || message.contains("PEM"));
    }

    #[test]
    fn ca_pem_validation_accepts_generated_ca() {
        let root = std::env::temp_dir().join(format!("boundless-ca-test-{}", uuid::Uuid::new_v4()));
        let paths = core_security::SecurityPaths::for_root(root.join("security"));
        let identity =
            core_security::ensure_device_identity(&paths, "m1", "machine", None).expect("identity");
        validate_ca_cert_pem(&identity.ca_cert_pem).expect("must accept");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sanitize_incoming_file_name_rejects_paths() {
        let err = sanitize_incoming_file_name("../evil.txt").expect_err("must reject");
        assert!(err.to_string().contains("path separators"));
    }

    #[test]
    fn sanitize_incoming_file_name_accepts_plain_name() {
        let name = sanitize_incoming_file_name("report.txt").expect("must accept");
        assert_eq!(name, "report.txt");
    }

    #[tokio::test]
    async fn join_peer_requires_issued_code_and_consumes_it() {
        let root =
            std::env::temp_dir().join(format!("boundless-state-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let missing_err = state
            .join_peer("missing".to_string(), "127.0.0.1:15100".to_string(), None)
            .await
            .expect_err("must reject unknown code");
        assert!(
            missing_err
                .to_string()
                .contains("invalid or was already used")
        );

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(code.clone(), "127.0.0.1:15100".to_string(), None)
            .await
            .expect("issued code should join");
        assert!(!peer_id.is_empty());

        let reused_err = state
            .join_peer(code, "127.0.0.1:15100".to_string(), None)
            .await
            .expect_err("reused code must fail");
        assert!(
            reused_err
                .to_string()
                .contains("invalid or was already used")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn touch_peer_does_not_persist_config_on_heartbeat() {
        let root =
            std::env::temp_dir().join(format!("boundless-touch-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
            .expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(code, "127.0.0.1:15100".to_string(), None)
            .await
            .expect("join");

        let before = std::fs::read_to_string(&config_path).expect("read before");
        state.touch_peer(&peer_id).await.expect("touch");
        let after = std::fs::read_to_string(&config_path).expect("read after");

        assert_eq!(before, after, "touch should not write config file");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remove_peer_clears_input_owner_and_allows_new_claim() {
        let root = std::env::temp_dir().join(format!(
            "boundless-remove-owner-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_one, _) = state.create_pairing_code(120).await;
        let peer_one = state
            .join_peer(
                code_one,
                "127.0.0.1:15100".to_string(),
                Some("peer-one".to_string()),
            )
            .await
            .expect("join peer one");

        let (code_two, _) = state.create_pairing_code(120).await;
        let peer_two = state
            .join_peer(
                code_two,
                "127.0.0.1:15101".to_string(),
                Some("peer-two".to_string()),
            )
            .await
            .expect("join peer two");

        let claimed = state
            .claim_input_owner(&peer_one, false)
            .await
            .expect("claim owner");
        assert!(claimed);
        assert_eq!(
            state.input_owner().await.as_deref(),
            Some(peer_one.as_str())
        );

        let removed = state.remove_peer(&peer_one).await.expect("remove peer");
        assert!(removed);
        assert!(
            state.input_owner().await.is_none(),
            "owner should be cleared"
        );

        let claimed_second = state
            .claim_input_owner(&peer_two, false)
            .await
            .expect("claim second peer");
        assert!(
            claimed_second,
            "new claim should not be blocked by stale owner"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn input_capture_target_requires_known_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-capture-target-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let err = state
            .set_input_capture_target(Some("missing-peer"))
            .await
            .expect_err("unknown peer must fail");
        assert!(err.to_string().contains("unknown peer"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn active_input_capture_target_requires_connected_peer_and_feature_enabled() {
        let root = std::env::temp_dir().join(format!(
            "boundless-capture-active-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let set = state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set capture target");
        assert_eq!(set.as_deref(), Some(peer_id.as_str()));
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );
        assert!(
            state.active_input_capture_target().await.is_none(),
            "disconnected peer must not be capture-active"
        );

        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");
        assert_eq!(
            state.active_input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );

        state
            .set_feature("share_input".to_string(), false)
            .await
            .expect("disable input share");
        assert!(
            state.active_input_capture_target().await.is_none(),
            "disabled share_input must block capture"
        );

        state
            .set_feature("share_input".to_string(), true)
            .await
            .expect("enable input share");
        assert_eq!(
            state.active_input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );

        state.clear_input_capture_target().await;
        assert!(state.input_capture_target().await.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remove_peer_clears_input_capture_target() {
        let root = std::env::temp_dir().join(format!(
            "boundless-remove-capture-target-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );

        state.remove_peer(&peer_id).await.expect("remove");
        assert!(
            state.input_capture_target().await.is_none(),
            "removed peer should clear capture target"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn disconnect_peer_clears_input_capture_target() {
        let root = std::env::temp_dir().join(format!(
            "boundless-disconnect-capture-target-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );

        state
            .set_peer_connected(&peer_id, false)
            .await
            .expect("disconnect peer");
        assert!(
            state.input_capture_target().await.is_none(),
            "disconnected peer should clear capture target"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn switch_all_capture_target_cycles_connected_layout_peers() {
        let root = std::env::temp_dir().join(format!(
            "boundless-switch-all-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_one, _) = state.create_pairing_code(120).await;
        let left_peer = state
            .join_peer(
                code_one,
                "127.0.0.1:15100".to_string(),
                Some("left".to_string()),
            )
            .await
            .expect("join left");

        let (code_two, _) = state.create_pairing_code(120).await;
        let right_peer = state
            .join_peer(
                code_two,
                "127.0.0.1:15101".to_string(),
                Some("right".to_string()),
            )
            .await
            .expect("join right");

        state
            .set_layout("right,self,left".to_string())
            .await
            .expect("set layout");
        state
            .set_peer_connected(&left_peer, true)
            .await
            .expect("connect left");
        state
            .set_peer_connected(&right_peer, true)
            .await
            .expect("connect right");

        assert_eq!(
            state.apply_switch_all_capture_target().await.as_deref(),
            Some(right_peer.as_str())
        );
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(right_peer.as_str())
        );

        assert_eq!(
            state.apply_switch_all_capture_target().await.as_deref(),
            Some(left_peer.as_str())
        );
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(left_peer.as_str())
        );

        state
            .set_peer_connected(&left_peer, false)
            .await
            .expect("disconnect left");
        assert_eq!(
            state.apply_switch_all_capture_target().await.as_deref(),
            Some(right_peer.as_str()),
            "disconnected peers must be skipped from switch-all rotation"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn abort_transport_sessions_for_peer_cancels_registered_tasks() {
        let root = std::env::temp_dir().join(format!(
            "boundless-transport-abort-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let session = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        state
            .register_transport_session_for_peer(&peer_id, session.abort_handle())
            .await;

        let aborted = state.abort_transport_sessions_for_peer(&peer_id).await;
        assert_eq!(aborted, 1);
        let join_error = session.await.expect_err("session should be aborted");
        assert!(join_error.is_cancelled());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn request_peer_reconnect_and_reset_clears_input_state_and_sessions() {
        let root = std::env::temp_dir().join(format!(
            "boundless-reconnect-reset-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner"),
            "owner claim should succeed"
        );
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set capture target");

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("queue incoming frame");

        let session = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        state
            .register_transport_session_for_peer(&peer_id, session.abort_handle())
            .await;

        let (generation, aborted_sessions) = state
            .request_peer_reconnect_and_reset(&peer_id)
            .await
            .expect("request reconnect reset");
        assert!(generation > 0, "reconnect generation should increment");
        assert_eq!(aborted_sessions, 1, "active session should be aborted");

        assert_eq!(state.input_owner().await, None, "owner should be released");
        assert!(
            state.input_capture_target().await.is_none(),
            "capture target should be cleared"
        );
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "pending inject frames should be cleared"
        );
        let peer = state
            .get_peer(&peer_id)
            .await
            .expect("peer must still exist");
        assert!(!peer.connected, "peer should be marked disconnected");
        assert!(
            state.peer_reconnect_generation(&peer_id).await >= generation,
            "reconnect generation should be visible"
        );

        let join_error = session.await.expect_err("session should be aborted");
        assert!(join_error.is_cancelled());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn request_all_peers_reconnect_and_reset_clears_shared_input_state() {
        let root = std::env::temp_dir().join(format!(
            "boundless-reconnect-reset-all-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_one, _) = state.create_pairing_code(120).await;
        let peer_one = state
            .join_peer(
                code_one,
                "127.0.0.1:15100".to_string(),
                Some("peer-one".to_string()),
            )
            .await
            .expect("join peer one");
        let (code_two, _) = state.create_pairing_code(120).await;
        let peer_two = state
            .join_peer(
                code_two,
                "127.0.0.1:15101".to_string(),
                Some("peer-two".to_string()),
            )
            .await
            .expect("join peer two");

        state
            .set_peer_connected(&peer_one, true)
            .await
            .expect("connect peer one");
        state
            .set_peer_connected(&peer_two, true)
            .await
            .expect("connect peer two");
        assert!(
            state
                .claim_input_owner(&peer_one, false)
                .await
                .expect("claim owner"),
            "owner claim should succeed"
        );
        state
            .set_input_capture_target(Some(&peer_one))
            .await
            .expect("set capture target");

        state
            .route_incoming_input_frame(
                &peer_one,
                InputFrame {
                    source_peer_id: peer_one.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("queue incoming frame");

        let session_one = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let session_two = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        state
            .register_transport_session_for_peer(&peer_one, session_one.abort_handle())
            .await;
        state
            .register_transport_session_for_peer(&peer_two, session_two.abort_handle())
            .await;

        let (disconnected, aborted_sessions) = state
            .request_all_peers_reconnect_and_reset()
            .await
            .expect("request all reconnect reset");
        assert_eq!(
            disconnected, 2,
            "both connected peers should be disconnected"
        );
        assert_eq!(
            aborted_sessions, 2,
            "both active sessions should be aborted"
        );

        assert_eq!(state.input_owner().await, None, "owner should be released");
        assert!(
            state.input_capture_target().await.is_none(),
            "capture target should be cleared"
        );
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "pending inject frames should be cleared"
        );

        let peers = state.list_peers().await;
        assert!(
            peers.iter().all(|peer| !peer.connected),
            "all peers should be marked disconnected"
        );
        assert!(
            state.peer_reconnect_generation(&peer_one).await > 0,
            "peer one reconnect generation should increment"
        );
        assert!(
            state.peer_reconnect_generation(&peer_two).await > 0,
            "peer two reconnect generation should increment"
        );

        let join_error_one = session_one
            .await
            .expect_err("session one should be aborted");
        assert!(join_error_one.is_cancelled());
        let join_error_two = session_two
            .await
            .expect_err("session two should be aborted");
        assert!(join_error_two.is_cancelled());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn store_incoming_file_rejects_unsafe_name() {
        let root = std::env::temp_dir().join(format!(
            "boundless-incoming-file-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let err = state
            .store_incoming_file("peer-a", "../evil.txt", b"bad".to_vec())
            .await
            .expect_err("must reject unsafe file name");
        assert!(err.to_string().contains("path separators"));

        let escaped_path = root.join("evil.txt");
        assert!(!escaped_path.exists(), "unsafe path must never be created");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn disconnect_clears_inbound_sequence_state_for_reconnect() {
        let root = std::env::temp_dir().join(format!(
            "boundless-reconnect-seq-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim")
        );
        let first = state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("first route");
        assert!(matches!(first, RouteDecision::Applied { .. }));

        state
            .set_peer_connected(&peer_id, false)
            .await
            .expect("disconnect");
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("re-claim")
        );

        let second = state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 2,
                    events: vec![InputEvent::MouseMove { dx: 2, dy: 2 }],
                },
            )
            .await
            .expect("sequence should restart after disconnect");
        assert!(matches!(second, RouteDecision::Applied { .. }));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn route_incoming_input_frame_queues_for_injection_when_applied() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-queue-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        let decision = state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![
                        InputEvent::MouseMove { dx: 2, dy: -1 },
                        InputEvent::Key {
                            scan_code: 30,
                            state: KeyState::Down,
                        },
                    ],
                },
            )
            .await
            .expect("route");
        assert!(matches!(
            decision,
            RouteDecision::Applied { event_count: 2 }
        ));

        let queued = state
            .dequeue_pending_inject_input_frame()
            .await
            .expect("queued frame");
        assert_eq!(queued.peer_id, peer_id);
        assert_eq!(queued.sequence, 1);
        assert_eq!(queued.events.len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn route_incoming_input_frame_auto_claims_owner_when_missing() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-auto-claim-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let decision = state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("route");
        assert!(matches!(decision, RouteDecision::Applied { .. }));
        assert_eq!(state.input_owner().await.as_deref(), Some(peer_id.as_str()));

        let incoming = state
            .transport_events()
            .await
            .into_iter()
            .find(|event| event.kind == "input_frame" && event.direction == "incoming")
            .expect("incoming event");
        assert!(incoming.detail.contains("auto_claimed_owner=true"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn route_incoming_input_frame_auto_steals_owner_when_mismatched() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-auto-steal-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_a, _) = state.create_pairing_code(120).await;
        let peer_a = state
            .join_peer(
                code_a,
                "127.0.0.1:15100".to_string(),
                Some("peer-a".to_string()),
            )
            .await
            .expect("join peer-a");
        let (code_b, _) = state.create_pairing_code(120).await;
        let peer_b = state
            .join_peer(
                code_b,
                "127.0.0.1:15101".to_string(),
                Some("peer-b".to_string()),
            )
            .await
            .expect("join peer-b");

        assert!(
            state
                .claim_input_owner(&peer_a, false)
                .await
                .expect("claim")
        );
        assert_eq!(state.input_owner().await.as_deref(), Some(peer_a.as_str()));

        let decision = state
            .route_incoming_input_frame(
                &peer_b,
                InputFrame {
                    source_peer_id: peer_b.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 2,
                    events: vec![InputEvent::MouseMove { dx: 2, dy: 2 }],
                },
            )
            .await
            .expect("route");
        assert!(matches!(decision, RouteDecision::Applied { .. }));
        assert_eq!(state.input_owner().await.as_deref(), Some(peer_b.as_str()));

        let incoming = state
            .transport_events()
            .await
            .into_iter()
            .find(|event| {
                event.kind == "input_frame"
                    && event.direction == "incoming"
                    && event.peer_id == peer_b
            })
            .expect("incoming event");
        assert!(incoming.detail.contains("auto_claimed_owner=true"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn route_incoming_input_frame_records_latency_detail_fields() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-latency-detail-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 42,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("route");

        let events = state.transport_events().await;
        let incoming = events
            .iter()
            .find(|event| event.kind == "input_frame" && event.direction == "incoming")
            .expect("incoming input frame event");
        assert!(incoming.detail.contains("sequence=42"));
        assert!(incoming.detail.contains("capture_to_receive_ms="));

        let queued = events
            .iter()
            .find(|event| event.kind == "input_inject_queued" && event.direction == "local")
            .expect("queued input inject event");
        assert!(queued.detail.contains("sequence=42"));
        assert!(queued.detail.contains("capture_to_queue_ms="));
        assert!(queued.detail.contains("receive_to_queue_ms="));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn transport_events_ring_buffer_keeps_most_recent_records() {
        let root = std::env::temp_dir().join(format!(
            "boundless-transport-event-ring-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        for index in 0..(MAX_TRANSPORT_EVENTS + 8) {
            state.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "ring_probe".to_string(),
                peer_id: "peer-a".to_string(),
                detail: format!("idx={index}"),
                size_bytes: index as u64,
            });
        }

        let events = state.transport_events().await;
        assert_eq!(events.len(), MAX_TRANSPORT_EVENTS);
        assert!(
            events
                .first()
                .is_some_and(|event| event.detail == "idx=8" && event.size_bytes == 8),
            "oldest retained event should reflect dropped head records"
        );
        assert!(
            events
                .last()
                .is_some_and(|event| event.detail == format!("idx={}", MAX_TRANSPORT_EVENTS + 7)),
            "newest event should always be retained"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn disconnect_clears_pending_injection_frames_for_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-clear-queue-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                },
            )
            .await
            .expect("route");

        state
            .set_peer_connected(&peer_id, false)
            .await
            .expect("disconnect");
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "disconnect should clear queued injection frames for that peer"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pending_input_injection_queue_drops_oldest_when_full() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-overflow-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        for sequence in 1..=(MAX_PENDING_INJECT_INPUT_FRAMES as u64 + 1) {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms: sequence as i64,
                        events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
                    },
                )
                .await
                .expect("route");
        }

        let first = state
            .dequeue_pending_inject_input_frame()
            .await
            .expect("first queued");
        assert_eq!(first.sequence, 2, "oldest frame should have been dropped");

        let mut count = 1usize;
        while state.dequeue_pending_inject_input_frame().await.is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_PENDING_INJECT_INPUT_FRAMES);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn queue_input_events_validates_size_and_increments_sequence() {
        let root = std::env::temp_dir().join(format!(
            "boundless-queue-input-events-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let empty_err = state
            .queue_input_events(&peer_id, Vec::new())
            .await
            .expect_err("empty events must fail");
        assert!(empty_err.to_string().contains("at least one event"));

        let too_many = vec![InputEvent::MouseMove { dx: 1, dy: 1 }; MAX_EVENTS_PER_FRAME + 1];
        let too_many_err = state
            .queue_input_events(&peer_id, too_many)
            .await
            .expect_err("oversized frame must fail");
        assert!(too_many_err.to_string().contains("exceeds limit"));

        state
            .queue_input_events(
                &peer_id,
                vec![
                    InputEvent::MouseMove { dx: 2, dy: 3 },
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    },
                ],
            )
            .await
            .expect("queue frame 1");
        state
            .queue_input_events(
                &peer_id,
                vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up,
                }],
            )
            .await
            .expect("queue frame 2");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::InputFrame { sequence: 1, events, .. }) if events.len() == 2
        ));
        assert!(matches!(
            queued.get(1),
            Some(OutboundPayload::InputFrame { sequence: 2, events, .. }) if events.len() == 1
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn queue_input_events_notifies_outgoing_flush_signal() {
        let root = std::env::temp_dir().join(format!(
            "boundless-outgoing-flush-signal-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let mut flush_signal = state.subscribe_outgoing_flush_signal();
        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 1, dy: 1 }])
            .await
            .expect("queue frame");

        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            flush_signal.changed(),
        )
        .await
        .expect("flush signal should be observed")
        .expect("flush signal channel should remain open");
        assert!(
            *flush_signal.borrow_and_update() > 0,
            "flush signal generation should advance after enqueue"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn drain_outgoing_prioritizes_input_frames_over_bulk_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-outgoing-priority-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        state
            .queue_clipboard_text(&peer_id, "bulk".to_string())
            .await
            .expect("queue bulk");
        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 1, dy: 2 }])
            .await
            .expect("queue input");

        let drained = state.drain_outgoing(&peer_id).await;
        assert_eq!(drained.len(), 2);
        assert!(
            matches!(drained.first(), Some(OutboundPayload::InputFrame { .. })),
            "input frame should drain before bulk payloads"
        );
        assert!(
            matches!(drained.get(1), Some(OutboundPayload::ClipboardText { .. })),
            "bulk payload should follow drained input frame"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn drain_outgoing_bulk_respects_max_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-outgoing-bulk-limit-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        state
            .queue_clipboard_text(&peer_id, "one".to_string())
            .await
            .expect("queue one");
        state
            .queue_clipboard_text(&peer_id, "two".to_string())
            .await
            .expect("queue two");
        state
            .queue_clipboard_text(&peer_id, "three".to_string())
            .await
            .expect("queue three");

        let first_batch = state.drain_outgoing_bulk(&peer_id, 2).await;
        assert_eq!(first_batch.len(), 2);
        assert!(matches!(
            first_batch.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "one"
        ));
        assert!(matches!(
            first_batch.get(1),
            Some(OutboundPayload::ClipboardText { text }) if text == "two"
        ));

        let second_batch = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        assert_eq!(second_batch.len(), 1);
        assert!(matches!(
            second_batch.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "three"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn queue_file_from_path_enqueues_chunked_bulk_transfer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-file-outgoing-chunk-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let file_path = root.join("payload.bin");
        let payload = vec![7u8; FILE_TRANSFER_CHUNK_BYTES * 2 + 17];
        tokio::fs::write(&file_path, &payload)
            .await
            .expect("write payload");

        state
            .queue_file_from_path(&peer_id, &file_path)
            .await
            .expect("queue file");

        let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
        assert_eq!(queued.len(), 5, "start + 3 chunks + end expected");

        let transfer_id = match queued.first() {
            Some(OutboundPayload::FileStart {
                transfer_id,
                file_name,
                total_bytes,
            }) => {
                assert_eq!(file_name, "payload.bin");
                assert_eq!(*total_bytes, payload.len() as u64);
                transfer_id.clone()
            }
            other => panic!("expected file start payload, got {other:?}"),
        };

        let expected_chunks = [
            (0u64, FILE_TRANSFER_CHUNK_BYTES),
            (FILE_TRANSFER_CHUNK_BYTES as u64, FILE_TRANSFER_CHUNK_BYTES),
            ((FILE_TRANSFER_CHUNK_BYTES * 2) as u64, 17usize),
        ];
        for (payload_item, (expected_offset, expected_size)) in queued
            .iter()
            .skip(1)
            .take(3)
            .zip(expected_chunks.into_iter())
        {
            match payload_item {
                OutboundPayload::FileChunk {
                    transfer_id: chunk_transfer_id,
                    source_path,
                    offset_bytes,
                    length_bytes,
                } => {
                    assert_eq!(chunk_transfer_id, &transfer_id);
                    assert_eq!(source_path, &file_path);
                    assert_eq!(*offset_bytes, expected_offset);
                    assert_eq!(*length_bytes, expected_size);
                }
                other => panic!("expected file chunk payload, got {other:?}"),
            }
        }

        match queued.get(4) {
            Some(OutboundPayload::FileEnd {
                transfer_id: end_transfer_id,
                file_name,
                total_bytes,
            }) => {
                assert_eq!(end_transfer_id, &transfer_id);
                assert_eq!(file_name, "payload.bin");
                assert_eq!(*total_bytes, payload.len() as u64);
            }
            other => panic!("expected file end payload, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn queue_file_from_path_rejects_non_regular_paths() {
        let root = std::env::temp_dir().join(format!(
            "boundless-file-outgoing-invalid-path-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");

        let err = state
            .queue_file_from_path(&peer_id, &root)
            .await
            .expect_err("directory path must fail");
        assert!(
            err.to_string().contains("regular file"),
            "error should indicate non-regular input"
        );
        assert!(
            state
                .drain_outgoing_bulk(&peer_id, usize::MAX)
                .await
                .is_empty(),
            "invalid source path must not enqueue bulk payloads"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn import_trust_bundle_rejects_invalid_address_without_persisting_trust() {
        let root = std::env::temp_dir().join(format!(
            "boundless-import-bundle-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state = AppState::load_or_create_with_paths(config_path, security_root.clone())
            .expect("load state");

        let remote_paths = core_security::SecurityPaths::for_root(root.join("remote-security"));
        let remote_identity = core_security::ensure_device_identity(
            &remote_paths,
            "remote-machine",
            "remote",
            Some("127.0.0.1"),
        )
        .expect("remote identity");

        let err = state
            .import_trust_bundle(
                core_security::TrustBundle {
                    machine_id: "remote-machine".to_string(),
                    display_name: "remote".to_string(),
                    network_address: "   ".to_string(),
                    ca_cert_pem: remote_identity.ca_cert_pem,
                },
                None,
            )
            .await
            .expect_err("invalid address must fail");
        assert!(err.to_string().contains("peer address must not be empty"));

        let trusted = state.trusted_records().await.expect("read trust");
        assert!(
            trusted
                .iter()
                .all(|record| record.machine_id != "remote-machine"),
            "invalid bundle import must not persist trust records"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remove_peer_revokes_trust_and_reimport_resets_reconnect_generation() {
        let root = std::env::temp_dir().join(format!(
            "boundless-trust-lifecycle-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state = AppState::load_or_create_with_paths(config_path, security_root.clone())
            .expect("load state");

        let remote_paths = core_security::SecurityPaths::for_root(root.join("remote-security"));
        let remote_identity = core_security::ensure_device_identity(
            &remote_paths,
            "remote-machine",
            "remote",
            Some("127.0.0.1"),
        )
        .expect("remote identity");

        let initial_bundle = core_security::TrustBundle {
            machine_id: "remote-machine".to_string(),
            display_name: "remote".to_string(),
            network_address: "10.10.0.5:15100".to_string(),
            ca_cert_pem: remote_identity.ca_cert_pem.clone(),
        };

        state
            .import_trust_bundle(initial_bundle, Some("remote-alpha".to_string()))
            .await
            .expect("import trust bundle");
        assert!(
            state.get_peer("remote-machine").await.is_some(),
            "import must create peer entry keyed by machine id"
        );
        assert!(
            state
                .trusted_records()
                .await
                .expect("read trust")
                .iter()
                .any(|record| record.machine_id == "remote-machine"),
            "import must persist trust record"
        );

        state.request_peer_reconnect("remote-machine").await;
        state.request_peer_reconnect("remote-machine").await;
        assert_eq!(
            state.peer_reconnect_generation("remote-machine").await,
            2,
            "generation should increment while peer is present"
        );

        let removed = state
            .remove_peer("remote-machine")
            .await
            .expect("remove peer");
        assert!(removed, "existing peer should be removed");
        assert!(
            state.get_peer("remote-machine").await.is_none(),
            "remove should delete peer config"
        );
        assert!(
            state
                .trusted_records()
                .await
                .expect("read trust")
                .iter()
                .all(|record| record.machine_id != "remote-machine"),
            "remove should revoke trust record"
        );
        assert_eq!(
            state.peer_reconnect_generation("remote-machine").await,
            0,
            "remove should clear reconnect generation state"
        );
        assert!(
            !state
                .remove_peer("remote-machine")
                .await
                .expect("remove missing peer"),
            "second remove should be deterministic no-op"
        );

        let reimport_bundle = core_security::TrustBundle {
            machine_id: "remote-machine".to_string(),
            display_name: "remote".to_string(),
            network_address: "10.10.0.77:15100".to_string(),
            ca_cert_pem: remote_identity.ca_cert_pem,
        };
        state
            .import_trust_bundle(reimport_bundle, Some("remote-beta".to_string()))
            .await
            .expect("re-import trust bundle");

        let reimported_peer = state
            .get_peer("remote-machine")
            .await
            .expect("peer after re-import");
        assert_eq!(reimported_peer.display_name, "remote-beta");
        assert_eq!(reimported_peer.address, "10.10.0.77:15100");
        assert_eq!(
            state.request_peer_reconnect("remote-machine").await,
            1,
            "reconnect generation should restart after re-import"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn diagnostics_dump_reports_nonce_challenge_rejections() {
        let root = std::env::temp_dir().join(format!(
            "boundless-pairing-diagnostics-dump-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let requester_paths =
            core_security::SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = core_security::ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = core_security::TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "10.10.0.5:15100".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let challenge = state
            .queue_nearby_pairing_code_challenge(requester_bundle, None, 120)
            .await
            .expect("queue challenge");
        let request_id = challenge.request_id.clone();
        let verification_code = challenge
            .verification_code
            .clone()
            .expect("verification code");

        for _ in 0..5 {
            let _ = state
                .submit_nearby_pairing_code(&request_id, &verification_code, "wrong-nonce", None)
                .await;
        }
        assert!(
            matches!(
                state.nearby_pairing_status(&request_id).await,
                NearbyPairingStatus::Rejected { .. }
            ),
            "nonce failures should reject the request after max attempts"
        );

        let output_dir = root.join("diagnostics");
        let dump_path = state
            .diagnostics_dump(Some(output_dir.to_string_lossy().to_string()))
            .await
            .expect("diagnostics dump path");
        let dump_content = std::fs::read_to_string(&dump_path).expect("read diagnostics dump");
        assert!(
            dump_content.contains("Pairing Diagnostics"),
            "diagnostics dump should include pairing diagnostics section"
        );
        assert!(
            dump_content.contains("pairing_decisions_rejected=1"),
            "diagnostics should include one rejected decision"
        );
        assert!(
            dump_content.contains("pairing_rejections_nonce_attempts=1"),
            "diagnostics should classify nonce challenge rejections"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_sync_dedupes_and_suppresses_remote_echo() {
        let root =
            std::env::temp_dir().join(format!("boundless-clipboard-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_a, _) = state.create_pairing_code(120).await;
        let peer_a = state
            .join_peer(
                code_a,
                "127.0.0.1:15100".to_string(),
                Some("peer-a".to_string()),
            )
            .await
            .expect("join peer-a");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        let queued = state
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("queue local hello");
        assert!(queued, "initial clipboard text should be queued");

        let first = state.drain_outgoing(&peer_a).await;
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "hello"
        ));

        let deduped = state
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("dedupe");
        assert!(!deduped, "unchanged clipboard text should be ignored");

        state
            .enqueue_remote_clipboard_text(&peer_a, "remote".to_string())
            .await
            .expect("enqueue remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        assert!(matches!(
            remote.payload,
            ClipboardPayload::Text(ref text) if text == "remote"
        ));
        state.mark_remote_clipboard_applied(&remote.hash).await;

        let suppressed = state
            .queue_local_clipboard_text_for_connected_peers("remote".to_string())
            .await
            .expect("suppress remote echo");
        assert!(
            !suppressed,
            "clipboard observer should suppress immediate echo after remote apply"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_sync_does_not_cache_hash_when_no_peers_connected() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-connect-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_a, _) = state.create_pairing_code(120).await;
        let peer_a = state
            .join_peer(
                code_a,
                "127.0.0.1:15100".to_string(),
                Some("peer-a".to_string()),
            )
            .await
            .expect("join peer-a");

        let queued_disconnected = state
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("queue while disconnected");
        assert!(
            !queued_disconnected,
            "must not queue without connected peers"
        );
        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "no payloads should be queued while disconnected"
        );

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        let queued_connected = state
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("queue on connect");
        assert!(
            queued_connected,
            "same clipboard value should queue once peers are connected"
        );
        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(outgoing.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_sync_clears_stale_suppress_token_after_mismatch() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-stale-suppress-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_a, _) = state.create_pairing_code(120).await;
        let peer_a = state
            .join_peer(
                code_a,
                "127.0.0.1:15100".to_string(),
                Some("peer-a".to_string()),
            )
            .await
            .expect("join peer-a");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        state
            .enqueue_remote_clipboard_text(&peer_a, "remote".to_string())
            .await
            .expect("enqueue remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state.mark_remote_clipboard_applied(&remote.hash).await;

        let different = state
            .queue_local_clipboard_text_for_connected_peers("different".to_string())
            .await
            .expect("queue different");
        assert!(different, "different local clipboard value should queue");

        let remote_again = state
            .queue_local_clipboard_text_for_connected_peers("remote".to_string())
            .await
            .expect("queue remote again");
        assert!(
            remote_again,
            "stale suppression token must not suppress later legitimate reuse"
        );

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(outgoing.len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_image_sync_dedupes_and_suppresses_remote_echo() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-image-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_a, _) = state.create_pairing_code(120).await;
        let peer_a = state
            .join_peer(
                code_a,
                "127.0.0.1:15100".to_string(),
                Some("peer-a".to_string()),
            )
            .await
            .expect("join peer-a");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        let image = minimal_bmp_payload(255);
        let queued = state
            .queue_local_clipboard_image_for_connected_peers(image.clone())
            .await
            .expect("queue local image");
        assert!(queued, "initial clipboard image should be queued");

        let first = state.drain_outgoing(&peer_a).await;
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first.first(),
            Some(OutboundPayload::ClipboardImage { image_bmp }) if image_bmp == &image
        ));

        let deduped = state
            .queue_local_clipboard_image_for_connected_peers(image.clone())
            .await
            .expect("dedupe image");
        assert!(!deduped, "unchanged clipboard image should be ignored");

        state
            .enqueue_remote_clipboard_image(&peer_a, image.clone())
            .await
            .expect("enqueue remote image");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote image item");
        assert!(matches!(
            remote.payload,
            ClipboardPayload::Image(ref image_bmp) if image_bmp == &image
        ));
        state.mark_remote_clipboard_applied(&remote.hash).await;

        let suppressed = state
            .queue_local_clipboard_image_for_connected_peers(image.clone())
            .await
            .expect("suppress remote image echo");
        assert!(
            !suppressed,
            "clipboard observer should suppress immediate image echo after remote apply"
        );

        let changed = state
            .queue_local_clipboard_image_for_connected_peers(minimal_bmp_payload(64))
            .await
            .expect("queue changed image");
        assert!(changed, "different clipboard image should queue");

        let _ = std::fs::remove_dir_all(&root);
    }
}
