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
    OutboundPayload, OutgoingPeerQueues, RuntimeWakeSignal, TransportEventRecord,
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

const MAX_TRANSPORT_EVENTS: usize = 512;
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

        Ok(Self {
            config_path: Arc::new(config_path),
            config: Arc::new(RwLock::new(config)),
            clipboard: Arc::new(ClipboardState::default()),
            pairing: Arc::new(PairingState::default()),
            transport: Arc::new(TransportState::default()),
            discovery: Arc::new(DiscoveryState::default()),
            input: Arc::new(InputState::new(input_enabled)),
            security_paths: Arc::new(paths),
            identity: Arc::new(identity),
            device_fingerprint: Arc::new(fingerprint),
            inbox_root: Arc::new(inbox_root),
            parsed_layout_matrix_cache: Arc::new(RwLock::new(None)),
            input_capture_wake: Arc::new(RuntimeWakeSignal::default()),
            input_inject_wake: Arc::new(RuntimeWakeSignal::default()),
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
        self.transport.outgoing_flush_signal.subscribe()
    }

    pub(crate) fn notify_outgoing_flush_signal(&self) {
        let next = self
            .transport
            .outgoing_flush_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let _ = self.transport.outgoing_flush_signal.send(next);
    }

    fn record_runtime_wake(&self, channel: &str, source: &str) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "runtime_wake".to_string(),
            peer_id: "none".to_string(),
            detail: format!("channel={channel} source={source}"),
            size_bytes: 0,
        });
    }

    pub(crate) fn notify_input_capture_wake(&self, source: &str) {
        if self.input_capture_wake.trigger() {
            self.record_runtime_wake("input_capture", source);
            self.input_capture_wake.notify_one();
        }
    }

    pub(crate) fn input_capture_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.input_capture_wake.clone()
    }

    pub(crate) fn input_inject_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.input_inject_wake.clone()
    }

    pub(crate) fn notify_input_inject_wake(&self, source: &str) {
        if self.input_inject_wake.trigger() {
            self.record_runtime_wake("input_inject", source);
            self.input_inject_wake.notify_one();
        }
    }

    pub(crate) fn notify_peer_reconcile_wake(&self, source: &str) {
        if self.transport.peer_reconcile_wake.trigger() {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "peer_reconcile_trigger".to_string(),
                peer_id: "all".to_string(),
                detail: format!("source={source}"),
                size_bytes: 0,
            });
            self.transport.peer_reconcile_wake.notify_one();
        }
    }

    pub(crate) fn peer_reconcile_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.transport.peer_reconcile_wake.clone()
    }

    pub(crate) fn record_input_queue_high_water(
        &self,
        queue_name: &str,
        peer_id: &str,
        depth: usize,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_high_water".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("queue={queue_name} depth={depth}"),
            size_bytes: depth as u64,
        });
    }

    pub(crate) fn record_input_queue_overflow_drop(
        &self,
        queue_name: &str,
        peer_id: &str,
        sequence: u64,
        reason: &str,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_overflow_drop".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("queue={queue_name} sequence={sequence} reason={reason}"),
            size_bytes: 0,
        });
    }

    pub(crate) fn record_input_queue_coalesced(
        &self,
        queue_name: &str,
        peer_id: &str,
        older_sequence: u64,
        newer_sequence: u64,
        merged_event_count: usize,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_queue_coalesced".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "queue={queue_name} older_sequence={older_sequence} newer_sequence={newer_sequence} merged_events={merged_event_count}"
            ),
            size_bytes: merged_event_count as u64,
        });
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
    async fn set_peer_connected_rolls_back_in_memory_state_when_config_save_fails() {
        let root = std::env::temp_dir().join(format!(
            "boundless-peer-connect-save-fail-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let mut state =
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

        let blocked_path = root.join("blocked-config-path");
        std::fs::create_dir_all(&blocked_path).expect("create blocked path directory");
        *std::sync::Arc::make_mut(&mut state.config_path) = blocked_path;

        let error = state
            .set_peer_connected(&peer_id, true)
            .await
            .expect_err("save failure should bubble up");
        assert!(
            error.to_string().contains("write"),
            "unexpected error: {error:#}"
        );

        let peer = state
            .get_peer(&peer_id)
            .await
            .expect("peer must still exist");
        assert!(
            !peer.connected,
            "failed persistence must not leave the in-memory peer marked connected"
        );

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
    async fn pending_input_injection_queue_coalesces_adjacent_move_frames() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-coalesce-test-{}",
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

        for sequence in 1..=2u64 {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms: sequence as i64,
                        events: vec![InputEvent::MouseMove {
                            dx: sequence as i32,
                            dy: 1,
                        }],
                    },
                )
                .await
                .expect("route");
        }

        let merged = state
            .dequeue_pending_inject_input_frame()
            .await
            .expect("first queued");
        assert_eq!(
            merged.sequence, 2,
            "merged frame should keep newest sequence"
        );
        assert!(matches!(
            merged.events.as_slice(),
            [InputEvent::MouseMove { dx, dy }] if *dx == 3 && *dy == 2
        ));
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "adjacent move frames should collapse into one queue entry"
        );

        let events = state.transport_events().await;
        assert!(
            events.iter().any(|event| {
                event.kind == "input_queue_coalesced"
                    && event.detail.contains("queue=inject")
                    && event.peer_id == peer_id
            }),
            "inject coalescing should be observable in diagnostics"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pending_input_injection_queue_drops_new_move_when_full_of_non_move_frames() {
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

        for sequence in 1..=(MAX_PENDING_INJECT_INPUT_FRAMES as u64) {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms: sequence as i64,
                        events: vec![InputEvent::Key {
                            scan_code: (sequence % 64) as u16 + 1,
                            state: KeyState::Down,
                        }],
                    },
                )
                .await
                .expect("route");
        }

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: MAX_PENDING_INJECT_INPUT_FRAMES as u64 + 1,
                    timestamp_unix_ms: 999,
                    events: vec![InputEvent::MouseMove { dx: 5, dy: 7 }],
                },
            )
            .await
            .expect("route overflow");

        let first = state
            .dequeue_pending_inject_input_frame()
            .await
            .expect("first queued");
        assert_eq!(
            first.sequence, 1,
            "new move should be dropped before older non-move control events"
        );

        let mut count = 1usize;
        while state.dequeue_pending_inject_input_frame().await.is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_PENDING_INJECT_INPUT_FRAMES);

        let events = state.transport_events().await;
        assert!(
            events.iter().any(|event| {
                event.kind == "input_queue_overflow_drop"
                    && event.detail.contains("queue=inject")
                    && event.detail.contains("reason=drop_new_move")
            }),
            "overflow policy should record why the move frame was dropped"
        );

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
    async fn route_incoming_input_frame_notifies_inject_wake_signal() {
        let root = std::env::temp_dir().join(format!(
            "boundless-inject-wake-test-{}",
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

        let signal = state.input_inject_wake_signal();
        let notified = signal.notified();
        tokio::pin!(notified);

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

        tokio::time::timeout(std::time::Duration::from_millis(200), &mut notified)
            .await
            .expect("inject wake should fire promptly");
        assert!(
            signal.take_pending(),
            "inject wake should remain pending for the runtime loop"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn set_input_capture_target_notifies_capture_wake_signal() {
        let root = std::env::temp_dir().join(format!(
            "boundless-capture-wake-test-{}",
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

        let signal = state.input_capture_wake_signal();
        let notified = signal.notified();
        tokio::pin!(notified);

        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        tokio::time::timeout(std::time::Duration::from_millis(200), &mut notified)
            .await
            .expect("capture wake should fire promptly");
        assert!(
            signal.take_pending(),
            "capture wake should remain pending for the runtime loop"
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
    async fn queue_input_events_coalesces_adjacent_move_frames() {
        let root = std::env::temp_dir().join(format!(
            "boundless-outgoing-input-coalesce-test-{}",
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
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 2, dy: 3 }])
            .await
            .expect("queue move one");
        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: -1, dy: 4 }])
            .await
            .expect("queue move two");

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(
            queued.len(),
            1,
            "adjacent outgoing move frames should collapse"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::InputFrame { sequence: 2, events, .. })
                if matches!(events.as_slice(), [InputEvent::MouseMove { dx, dy }] if *dx == 1 && *dy == 7)
        ));

        let events = state.transport_events().await;
        assert!(
            events.iter().any(|event| {
                event.kind == "input_queue_coalesced"
                    && event.detail.contains("queue=outgoing_input")
                    && event.peer_id == peer_id
            }),
            "outgoing coalescing should be observable in diagnostics"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn requeue_outgoing_front_coalesces_move_frames_across_boundary() {
        let root = std::env::temp_dir().join(format!(
            "boundless-outgoing-requeue-coalesce-test-{}",
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
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 3, dy: 1 }])
            .await
            .expect("queue first");
        let drained = state.drain_outgoing_input(&peer_id, 1).await;
        state
            .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 4, dy: -2 }])
            .await
            .expect("queue second");

        state.requeue_outgoing_front(&peer_id, drained).await;

        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(
            queued.len(),
            1,
            "requeue boundary should still collapse adjacent moves"
        );
        assert!(matches!(
            queued.first(),
            Some(OutboundPayload::InputFrame { sequence: 2, events, .. })
                if matches!(events.as_slice(), [InputEvent::MouseMove { dx, dy }] if *dx == 7 && *dy == -1)
        ));

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
            &remote.payload,
            ClipboardPayload::Text(text) if text == "remote"
        ));
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

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
    async fn clipboard_sync_persists_disconnected_local_text_for_replay_without_immediate_queueing()
    {
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

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "reconnect should replay retained text once"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "hello"
        ));
        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "replayed clipboard snapshot should not remain queued after one drain"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_sync_persists_disconnected_local_image_for_replay() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-image-replay-test-{}",
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

        let image = minimal_bmp_payload(128);
        let queued_disconnected = state
            .queue_local_clipboard_image_for_connected_peers(image.clone())
            .await
            .expect("queue image while disconnected");
        assert!(
            !queued_disconnected,
            "must not queue image payloads without connected peers"
        );
        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "no image payload should be queued while disconnected"
        );

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "reconnect should replay retained image once"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardImage { image_bmp }) if image_bmp == &image
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn peer_connect_schedules_clipboard_replay() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-reconnect-schedule-test-{}",
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
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("queue while disconnected");
        let mut flush_signal = state.subscribe_outgoing_flush_signal();

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            flush_signal.changed(),
        )
        .await
        .expect("connect should notify replay work")
        .expect("flush signal channel should remain open");
        assert!(
            *flush_signal.borrow_and_update() > 0,
            "replay scheduling should advance the flush generation"
        );

        let outgoing = state.drain_outgoing_bulk(&peer_a, usize::MAX).await;
        assert_eq!(
            outgoing.len(),
            1,
            "connect should schedule one replay payload"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "hello"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn mixed_topology_reconnect_replays_latest_clipboard_snapshot_to_late_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-mixed-reconnect-test-{}",
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

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        let queued = state
            .queue_local_clipboard_text_for_connected_peers("shared".to_string())
            .await
            .expect("queue local clipboard");
        assert!(
            queued,
            "connected peers should receive the live clipboard update"
        );

        let outgoing_a = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing_a.len(),
            1,
            "connected peer should get direct payload"
        );
        assert!(matches!(
            outgoing_a.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "shared"
        ));
        assert!(
            state.drain_outgoing(&peer_b).await.is_empty(),
            "disconnected peer must not receive a queued payload before reconnect"
        );

        state
            .set_peer_connected(&peer_b, true)
            .await
            .expect("connect peer-b");

        let outgoing_b = state.drain_outgoing(&peer_b).await;
        assert_eq!(
            outgoing_b.len(),
            1,
            "late peer should receive the retained latest snapshot on reconnect"
        );
        assert!(matches!(
            outgoing_b.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "shared"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_origin_reconnect_replays_latest_clipboard_snapshot_to_late_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-origin-reconnect-test-{}",
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

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");

        state
            .enqueue_remote_clipboard_text(&peer_a, "remote-shared".to_string())
            .await
            .expect("enqueue remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "applying a remote payload should not immediately echo it back to the source peer"
        );

        state
            .set_peer_connected(&peer_b, true)
            .await
            .expect("connect peer-b");

        let outgoing_b = state.drain_outgoing(&peer_b).await;
        assert_eq!(
            outgoing_b.len(),
            1,
            "late peer should receive the applied remote snapshot on reconnect"
        );
        assert!(matches!(
            outgoing_b.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "remote-shared"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_origin_snapshot_is_not_replayed_back_to_source_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-origin-no-echo-replay-test-{}",
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
            .enqueue_remote_clipboard_text(&peer_a, "remote-shared".to_string())
            .await
            .expect("enqueue remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        state
            .set_peer_connected(&peer_a, false)
            .await
            .expect("disconnect peer-a");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("reconnect peer-a");

        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "the source peer must not receive its own remote-applied snapshot back on reconnect"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn current_snapshot_resend_tracks_all_source_peers() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-current-snapshot-source-peer-set-test-{}",
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

        state
            .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
            .await
            .expect("enqueue first remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        state
            .enqueue_remote_clipboard_text(&peer_b, "dup".to_string())
            .await
            .expect("enqueue resend of current snapshot from second peer");
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "resend of the current authoritative snapshot should still be suppressed"
        );

        state
            .set_peer_connected(&peer_b, true)
            .await
            .expect("connect peer-b");

        assert!(
            state.drain_outgoing(&peer_b).await.is_empty(),
            "all peers that originated the current snapshot should be suppressed from replay"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reconnect_does_not_schedule_duplicate_replay_when_live_payload_is_already_queued() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-no-duplicate-reconnect-test-{}",
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
        let queued = state
            .queue_local_clipboard_text_for_connected_peers("hello".to_string())
            .await
            .expect("queue local clipboard");
        assert!(
            queued,
            "connected peer should get the live clipboard payload"
        );

        state
            .set_peer_connected(&peer_a, false)
            .await
            .expect("disconnect peer-a");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("reconnect peer-a");

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "reconnect should not schedule a duplicate replay when the same payload is already queued"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "hello"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn newer_local_snapshot_prunes_stale_queued_clipboard_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-prune-stale-local-queue-test-{}",
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
            .queue_local_clipboard_text_for_connected_peers("old".to_string())
            .await
            .expect("queue old local clipboard");
        state
            .set_peer_connected(&peer_a, false)
            .await
            .expect("disconnect peer-a");

        state
            .queue_local_clipboard_text_for_connected_peers("new".to_string())
            .await
            .expect("queue new local clipboard while disconnected");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("reconnect peer-a");

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "new authoritative local snapshot should prune stale queued clipboard payloads"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "new"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stale_drained_replay_is_dropped_after_newer_local_snapshot_supersedes_it() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-stale-drained-replay-drop-test-{}",
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
            .queue_local_clipboard_text_for_connected_peers("stale".to_string())
            .await
            .expect("queue stale while disconnected");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a and schedule stale replay");

        let drained = state.drain_outgoing_bulk(&peer_a, usize::MAX).await;
        assert_eq!(
            drained.len(),
            1,
            "expected one drained stale replay payload"
        );
        assert!(matches!(
            drained.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "stale"
        ));

        let queued = state
            .queue_local_clipboard_text_for_connected_peers("fresh".to_string())
            .await
            .expect("queue fresh local clipboard");
        assert!(
            queued,
            "fresh local clipboard should queue for the connected peer"
        );

        state.requeue_outgoing_front(&peer_a, drained).await;

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "stale drained replay must be dropped instead of reentering the bulk queue"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "fresh"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn newer_remote_snapshot_prunes_stale_queued_clipboard_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-prune-stale-remote-queue-test-{}",
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

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a");
        state
            .queue_local_clipboard_text_for_connected_peers("old".to_string())
            .await
            .expect("queue old local clipboard");
        state
            .set_peer_connected(&peer_a, false)
            .await
            .expect("disconnect peer-a");

        state
            .enqueue_remote_clipboard_text(&peer_b, "new".to_string())
            .await
            .expect("enqueue remote clipboard");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("reconnect peer-a");

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "new authoritative remote snapshot should prune stale queued local clipboard payloads"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "new"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn live_local_update_supersedes_stale_pending_replay_for_connected_peer() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-live-local-supersedes-replay-test-{}",
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
            .queue_local_clipboard_text_for_connected_peers("stale".to_string())
            .await
            .expect("queue stale while disconnected");
        state
            .set_peer_connected(&peer_a, true)
            .await
            .expect("connect peer-a and schedule stale replay");

        let queued = state
            .queue_local_clipboard_text_for_connected_peers("fresh".to_string())
            .await
            .expect("queue fresh local clipboard");
        assert!(
            queued,
            "fresh local clipboard should use the live connected-peer path"
        );

        let outgoing = state.drain_outgoing(&peer_a).await;
        assert_eq!(
            outgoing.len(),
            1,
            "fresh live payload should replace the stale pending replay for this peer"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "fresh"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_apply_cancels_pending_replay() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-cancel-test-{}",
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
            .queue_local_clipboard_text_for_connected_peers("local".to_string())
            .await
            .expect("queue while disconnected");
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
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        assert!(
            state.drain_outgoing(&peer_a).await.is_empty(),
            "remote apply should cancel stale replay before it is sent"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_queue_dedupes_consecutive_duplicate_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-dedupe-test-{}",
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
            .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
            .await
            .expect("enqueue first duplicate");
        state
            .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
            .await
            .expect("enqueue second duplicate");

        let first = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("first item");
        assert!(matches!(
            first.payload,
            ClipboardPayload::Text(ref text) if text == "dup"
        ));
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "consecutive duplicate remote payloads should collapse to one queued item"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_queue_suppresses_current_snapshot_resend() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-current-resend-test-{}",
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
            .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
            .await
            .expect("enqueue first remote");
        let remote = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("remote item");
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

        state
            .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
            .await
            .expect("enqueue resend of current snapshot");
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "resend of the current authoritative snapshot should not be requeued"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_image_queue_dedupes_consecutive_duplicate_payloads() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-image-dedupe-test-{}",
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

        let image = minimal_bmp_payload(55);
        state
            .enqueue_remote_clipboard_image(&peer_a, image.clone())
            .await
            .expect("enqueue first duplicate image");
        state
            .enqueue_remote_clipboard_image(&peer_a, image.clone())
            .await
            .expect("enqueue second duplicate image");

        let first = state
            .dequeue_remote_clipboard_payload()
            .await
            .expect("first image item");
        assert!(matches!(
            first.payload,
            ClipboardPayload::Image(ref image_bmp) if image_bmp == &image
        ));
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "consecutive duplicate remote image payloads should collapse to one queued item"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_queue_evicts_oldest_item_at_capacity() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-eviction-test-{}",
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

        for index in 0..=MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            state
                .enqueue_remote_clipboard_text(&peer_a, format!("payload-{index}"))
                .await
                .expect("enqueue remote payload");
        }

        let mut drained = Vec::new();
        while let Some(item) = state.dequeue_remote_clipboard_payload().await {
            drained.push(item);
        }

        assert_eq!(
            drained.len(),
            MAX_PENDING_REMOTE_CLIPBOARD_ITEMS,
            "remote clipboard queue should remain bounded at capacity"
        );
        assert!(matches!(
            drained.first(),
            Some(PendingRemoteClipboardPayload {
                payload: ClipboardPayload::Text(text),
                ..
            }) if text == "payload-1"
        ));
        assert!(matches!(
            drained.last(),
            Some(PendingRemoteClipboardPayload {
                payload: ClipboardPayload::Text(text),
                ..
            }) if text == &format!("payload-{MAX_PENDING_REMOTE_CLIPBOARD_ITEMS}")
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_clipboard_requeue_front_evicts_newest_item_at_capacity() {
        let root = std::env::temp_dir().join(format!(
            "boundless-clipboard-remote-requeue-eviction-test-{}",
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

        for index in 0..MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            state
                .enqueue_remote_clipboard_text(&peer_a, format!("payload-{index}"))
                .await
                .expect("enqueue remote payload");
        }

        state
            .requeue_remote_clipboard_payload_front(PendingRemoteClipboardPayload {
                peer_id: peer_a.clone(),
                payload: ClipboardPayload::Text("requeued".to_string()),
                hash: payload_hash_hex(&ClipboardPayload::Text("requeued".to_string())),
                retry_count: 1,
            })
            .await;

        let mut drained = Vec::new();
        while let Some(item) = state.dequeue_remote_clipboard_payload().await {
            drained.push(item);
        }

        assert_eq!(
            drained.len(),
            MAX_PENDING_REMOTE_CLIPBOARD_ITEMS,
            "front requeue should keep the remote clipboard queue bounded"
        );
        assert!(matches!(
            drained.first(),
            Some(PendingRemoteClipboardPayload {
                payload: ClipboardPayload::Text(text),
                ..
            }) if text == "requeued"
        ));
        assert!(matches!(
            drained.last(),
            Some(PendingRemoteClipboardPayload {
                payload: ClipboardPayload::Text(text),
                ..
            }) if text == &format!("payload-{}", MAX_PENDING_REMOTE_CLIPBOARD_ITEMS - 2)
        ));

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
        state
            .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
            .await;

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
        assert_eq!(
            outgoing.len(),
            1,
            "later authoritative clipboard value should replace the earlier queued clipboard payload"
        );
        assert!(matches!(
            outgoing.first(),
            Some(OutboundPayload::ClipboardText { text }) if text == "remote"
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn clipboard_image_sync_dedupes_current_snapshot_and_blocks_echo() {
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
        assert!(
            state.dequeue_remote_clipboard_payload().await.is_none(),
            "resend of the current authoritative image snapshot should not be requeued"
        );

        let suppressed = state
            .queue_local_clipboard_image_for_connected_peers(image.clone())
            .await
            .expect("ignore image echo");
        assert!(
            !suppressed,
            "clipboard observer should still ignore the unchanged image after a remote resend"
        );

        let changed = state
            .queue_local_clipboard_image_for_connected_peers(minimal_bmp_payload(64))
            .await
            .expect("queue changed image");
        assert!(changed, "different clipboard image should queue");

        let _ = std::fs::remove_dir_all(&root);
    }
}
