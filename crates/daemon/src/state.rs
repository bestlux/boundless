use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::{sync::RwLock, task::AbortHandle};
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
    load_or_create_device_secret, load_trust_records, upsert_trust_record,
};
use core_transfer::{resolve_conflict_path, validate_transfer_size};

use crate::config::{
    ApiTransport, PeerConfig, RuntimeConfig, config_path, load_or_create_config_at, save_config_at,
};

const MAX_TRANSPORT_EVENTS: usize = 512;
const MAX_PENDING_REMOTE_CLIPBOARD_ITEMS: usize = 64;
const MAX_PENDING_INJECT_INPUT_FRAMES: usize = 128;
const NEARBY_PAIRING_DECISION_RETENTION_MINUTES: i64 = 10;

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    ClipboardText {
        text: String,
    },
    ClipboardImage {
        image_bmp: Vec<u8>,
    },
    File {
        file_name: String,
        bytes: Vec<u8>,
    },
    InputFrame {
        sequence: u64,
        timestamp_unix_ms: i64,
        events: Vec<InputEvent>,
    },
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
}

#[derive(Debug, Clone)]
pub enum NearbyPairingStatus {
    Pending,
    Approved { responder_bundle: TrustBundle },
    Rejected { message: String },
    Missing,
}

#[derive(Debug, Clone)]
struct PendingNearbyPairingRequestRecord {
    summary: PendingNearbyPairingRequest,
    requester_bundle: TrustBundle,
    requester_alias: Option<String>,
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

#[derive(Clone)]
pub struct AppState {
    config_path: Arc<PathBuf>,
    config: Arc<RwLock<RuntimeConfig>>,
    pairing_codes: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    security_paths: Arc<SecurityPaths>,
    identity: Arc<DeviceIdentity>,
    device_fingerprint: Arc<String>,
    outgoing_payloads: Arc<RwLock<HashMap<String, VecDeque<OutboundPayload>>>>,
    transport_events: Arc<RwLock<VecDeque<TransportEventRecord>>>,
    clipboard_sync: Arc<RwLock<ClipboardSyncState>>,
    discovered_endpoints: Arc<RwLock<HashMap<String, SocketAddr>>>,
    inbox_root: Arc<PathBuf>,
    input_router: Arc<RwLock<InputRouter>>,
    input_sequence_by_peer: Arc<RwLock<HashMap<String, u64>>>,
    pending_inject_input_frames: Arc<RwLock<VecDeque<PendingInjectInputFrame>>>,
    input_capture_target_peer_id: Arc<RwLock<Option<String>>>,
    reconnect_generation_by_peer: Arc<RwLock<HashMap<String, u64>>>,
    pending_nearby_pairing_requests:
        Arc<RwLock<HashMap<String, PendingNearbyPairingRequestRecord>>>,
    nearby_pairing_decisions: Arc<RwLock<HashMap<String, NearbyPairingDecisionRecord>>>,
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

        Ok(Self {
            config_path: Arc::new(config_path),
            config: Arc::new(RwLock::new(config)),
            pairing_codes: Arc::new(RwLock::new(HashMap::new())),
            security_paths: Arc::new(paths),
            identity: Arc::new(identity),
            device_fingerprint: Arc::new(fingerprint),
            outgoing_payloads: Arc::new(RwLock::new(HashMap::new())),
            transport_events: Arc::new(RwLock::new(VecDeque::new())),
            clipboard_sync: Arc::new(RwLock::new(ClipboardSyncState::default())),
            discovered_endpoints: Arc::new(RwLock::new(HashMap::new())),
            inbox_root: Arc::new(inbox_root),
            input_router: Arc::new(RwLock::new(InputRouter::new(input_enabled))),
            input_sequence_by_peer: Arc::new(RwLock::new(HashMap::new())),
            pending_inject_input_frames: Arc::new(RwLock::new(VecDeque::new())),
            input_capture_target_peer_id: Arc::new(RwLock::new(None)),
            reconnect_generation_by_peer: Arc::new(RwLock::new(HashMap::new())),
            pending_nearby_pairing_requests: Arc::new(RwLock::new(HashMap::new())),
            nearby_pairing_decisions: Arc::new(RwLock::new(HashMap::new())),
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

    pub async fn update_bind(&self, bind: String) -> Result<()> {
        validate_bind_address(&bind)?;

        let mut config = self.config.write().await;
        config.api_bind = bind;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_api_transport(&self, api_transport: ApiTransport) -> Result<()> {
        let mut config = self.config.write().await;
        config.api_transport = api_transport;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_api_pipe_name(&self, pipe_name: String) -> Result<()> {
        validate_pipe_name(&pipe_name)?;

        let mut config = self.config.write().await;
        config.api_pipe_name = pipe_name;
        save_config_at(&self.config_path, &config)
    }

    pub async fn update_network_port(&self, port: u16) -> Result<()> {
        let mut config = self.config.write().await;
        config.network_port = port;
        save_config_at(&self.config_path, &config)
    }

    pub async fn set_discovered_endpoint(
        &self,
        machine_id: &str,
        endpoint: SocketAddr,
    ) -> Option<SocketAddr> {
        self.discovered_endpoints
            .write()
            .await
            .insert(machine_id.to_string(), endpoint)
    }

    pub async fn clear_discovered_endpoint(&self, machine_id: &str) -> Option<SocketAddr> {
        self.discovered_endpoints.write().await.remove(machine_id)
    }

    pub async fn discovered_endpoint(&self, machine_id: &str) -> Option<SocketAddr> {
        self.discovered_endpoints
            .read()
            .await
            .get(machine_id)
            .copied()
    }

    pub async fn create_pairing_code(&self, ttl_secs: u64) -> (String, DateTime<Utc>) {
        let code = generate_pairing_code(Duration::from_secs(ttl_secs));
        self.pairing_codes
            .write()
            .await
            .insert(code.value.clone(), code.expires_at);
        (code.value, code.expires_at)
    }

    pub async fn consume_pairing_code(&self, code: &str) -> Result<()> {
        let now = Utc::now();
        let mut pairing_codes = self.pairing_codes.write().await;
        validate_and_consume_pairing_code(&mut pairing_codes, code, now)?;
        pairing_codes.retain(|_, expires_at| *expires_at >= now);
        Ok(())
    }

    pub async fn queue_nearby_pairing_request(
        &self,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
    ) -> PendingNearbyPairingRequest {
        let summary = PendingNearbyPairingRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            requester_machine_id: requester_bundle.machine_id.clone(),
            requester_display_name: requester_bundle.display_name.clone(),
            created_at: Utc::now(),
        };
        let request_id = summary.request_id.clone();

        self.pending_nearby_pairing_requests.write().await.insert(
            request_id,
            PendingNearbyPairingRequestRecord {
                summary: summary.clone(),
                requester_bundle,
                requester_alias: requester_alias.and_then(normalize_optional_alias),
            },
        );
        summary
    }

    pub async fn list_pending_nearby_pairing_requests(&self) -> Vec<PendingNearbyPairingRequest> {
        let mut requests = self
            .pending_nearby_pairing_requests
            .read()
            .await
            .values()
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    pub async fn nearby_pairing_status(&self, request_id: &str) -> NearbyPairingStatus {
        if self
            .pending_nearby_pairing_requests
            .read()
            .await
            .contains_key(request_id)
        {
            return NearbyPairingStatus::Pending;
        }

        let now = Utc::now();
        let mut decisions = self.nearby_pairing_decisions.write().await;
        decisions.retain(|_, record| {
            record.decided_at
                + chrono::TimeDelta::minutes(NEARBY_PAIRING_DECISION_RETENTION_MINUTES)
                >= now
        });
        if let Some(record) = decisions.get(request_id) {
            return match &record.decision {
                NearbyPairingDecision::Approved { responder_bundle } => {
                    NearbyPairingStatus::Approved {
                        responder_bundle: responder_bundle.clone(),
                    }
                }
                NearbyPairingDecision::Rejected { message } => NearbyPairingStatus::Rejected {
                    message: message.clone(),
                },
            };
        }

        NearbyPairingStatus::Missing
    }

    pub async fn approve_nearby_pairing_request(
        &self,
        request_id: &str,
        alias_override: Option<String>,
    ) -> Result<TrustBundle> {
        let pending = {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .remove(request_id)
        }
        .ok_or_else(|| anyhow::anyhow!("nearby pairing request not found"))?;
        let peer_id = pending.summary.requester_machine_id.clone();
        let effective_alias = alias_override
            .and_then(normalize_optional_alias)
            .or(pending.requester_alias.clone());

        if let Err(error) = self
            .import_trust_bundle(pending.requester_bundle.clone(), effective_alias)
            .await
        {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            return Err(error);
        }

        let responder_bundle = self.export_trust_bundle().await?;
        self.nearby_pairing_decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Approved {
                    responder_bundle: responder_bundle.clone(),
                },
                decided_at: Utc::now(),
            },
        );
        self.request_peer_reconnect(&peer_id).await;
        Ok(responder_bundle)
    }

    pub async fn reject_nearby_pairing_request(&self, request_id: &str) -> bool {
        let removed = self
            .pending_nearby_pairing_requests
            .write()
            .await
            .remove(request_id);
        if removed.is_none() {
            return false;
        }

        self.nearby_pairing_decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Rejected {
                    message: "nearby pairing request rejected".to_string(),
                },
                decided_at: Utc::now(),
            },
        );
        true
    }

    pub async fn join_peer(
        &self,
        code: String,
        host: String,
        alias: Option<String>,
    ) -> Result<String> {
        let now = Utc::now();
        self.consume_pairing_code(&code).await?;

        let mut config = self.config.write().await;
        let normalized_address = normalize_peer_address(&host, config.network_port)?;
        let peer_id = uuid::Uuid::new_v4().to_string();

        let peer = PeerConfig {
            peer_id: peer_id.clone(),
            display_name: alias.unwrap_or_else(|| format!("peer-{}", &peer_id[..8])),
            address: normalized_address,
            connected: false,
            last_seen: now,
        };

        config.peers.push(peer);
        save_config_at(&self.config_path, &config)?;
        Ok(peer_id)
    }

    pub async fn list_peers(&self) -> Vec<PeerConfig> {
        self.config.read().await.peers.clone()
    }

    pub async fn get_peer(&self, peer_id: &str) -> Option<PeerConfig> {
        self.config
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.peer_id == peer_id)
            .cloned()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> Result<bool> {
        let mut config = self.config.write().await;
        let before = config.peers.len();
        config.peers.retain(|p| p.peer_id != peer_id);
        let removed = before != config.peers.len();
        if removed {
            save_config_at(&self.config_path, &config)?;
            let mut router = self.input_router.write().await;
            router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            self.input_sequence_by_peer.write().await.remove(peer_id);
            self.discovered_endpoints.write().await.remove(peer_id);
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
            self.reconnect_generation_by_peer
                .write()
                .await
                .remove(peer_id);
            self.abort_transport_sessions_for_peer(peer_id).await;
            let mut capture_target = self.input_capture_target_peer_id.write().await;
            if capture_target.as_deref() == Some(peer_id) {
                *capture_target = None;
            }
        }
        Ok(removed)
    }

    pub async fn set_peer_connected(&self, peer_id: &str, connected: bool) -> Result<()> {
        let mut config = self.config.write().await;
        let mut changed = false;

        if let Some(peer) = config.peers.iter_mut().find(|p| p.peer_id == peer_id) {
            peer.connected = connected;
            peer.last_seen = Utc::now();
            changed = true;
        }

        if changed {
            save_config_at(&self.config_path, &config)?;
        }

        if !connected {
            let mut router = self.input_router.write().await;
            router.release_owner(peer_id);
            router.clear_peer_state(peer_id);
            drop(router);
            self.clear_pending_inject_input_frames_for_peer(peer_id)
                .await;
        }

        Ok(())
    }

    pub async fn touch_peer(&self, peer_id: &str) -> Result<()> {
        let mut config = self.config.write().await;
        if let Some(peer) = config.peers.iter_mut().find(|p| p.peer_id == peer_id) {
            peer.last_seen = Utc::now();
        }
        Ok(())
    }

    pub async fn layout(&self) -> String {
        self.config.read().await.layout_matrix.clone()
    }

    pub async fn set_layout(&self, matrix: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.layout_matrix = matrix;
        save_config_at(&self.config_path, &config)
    }

    pub async fn edge_switch_policy(&self) -> (EasyMouseMode, bool) {
        let config = self.config.read().await;
        let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        let easy_mouse_enabled = config.features.get("easy_mouse").copied().unwrap_or(true);
        let wrap_mouse = config.features.get("wrap_mouse").copied().unwrap_or(true);

        let mode = if share_input_enabled && easy_mouse_enabled {
            EasyMouseMode::Enable
        } else {
            EasyMouseMode::Disable
        };

        (mode, wrap_mouse)
    }

    pub async fn capture_handoff_target_for_direction(
        &self,
        direction: SwitchDirection,
    ) -> Option<String> {
        let config = self.config.read().await;
        resolve_capture_handoff_target(&config, direction)
    }

    pub async fn apply_switch_all_capture_target(&self) -> Option<String> {
        let next = self.next_switch_all_capture_target().await;
        match next.as_deref() {
            Some(peer_id) => {
                let _ = self.set_input_capture_target(Some(peer_id)).await;
            }
            None => {
                self.clear_input_capture_target().await;
            }
        }
        next
    }

    pub async fn next_switch_all_capture_target(&self) -> Option<String> {
        let order = {
            let config = self.config.read().await;
            resolve_switch_all_target_order(&config)
        };
        let current_target = self.input_capture_target_peer_id.read().await.clone();
        if order.is_empty() {
            return None;
        }

        if let Some(current) = current_target
            && let Some(index) = order.iter().position(|peer_id| peer_id == &current)
        {
            return Some(order[(index + 1) % order.len()].clone());
        }

        Some(order[0].clone())
    }

    pub async fn set_feature(&self, name: String, enabled: bool) -> Result<()> {
        let mut config = self.config.write().await;
        config.features.insert(name.clone(), enabled);
        save_config_at(&self.config_path, &config)?;

        if name == "share_input" {
            self.input_router.write().await.set_enabled(enabled);
        } else if name == "share_clipboard" && !enabled {
            *self.clipboard_sync.write().await = ClipboardSyncState::default();
        }

        Ok(())
    }

    pub async fn feature_map(&self) -> std::collections::BTreeMap<String, bool> {
        self.config.read().await.features.clone()
    }

    pub async fn hotkey_map(&self) -> std::collections::BTreeMap<String, String> {
        self.config.read().await.hotkeys.clone()
    }

    pub async fn set_hotkey(&self, action: String, combo: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.hotkeys.insert(action, combo);
        save_config_at(&self.config_path, &config)
    }

    pub async fn request_peer_reconnect(&self, peer_id: &str) -> u64 {
        let mut generations = self.reconnect_generation_by_peer.write().await;
        let entry = generations.entry(peer_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    pub async fn peer_reconnect_generation(&self, peer_id: &str) -> u64 {
        *self
            .reconnect_generation_by_peer
            .read()
            .await
            .get(peer_id)
            .unwrap_or(&0)
    }

    fn next_transport_session_id(&self) -> u64 {
        self.next_transport_session_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn register_pending_transport_session(&self, abort_handle: AbortHandle) -> u64 {
        let session_id = self.next_transport_session_id();
        self.pending_transport_session_abort_handles
            .write()
            .await
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: AbortHandle,
    ) -> u64 {
        let session_id = self.next_transport_session_id();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn bind_pending_transport_session_to_peer(
        &self,
        session_id: u64,
        peer_id: &str,
    ) -> bool {
        let abort_handle = self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id);
        let Some(abort_handle) = abort_handle else {
            return false;
        };

        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        true
    }

    pub async fn clear_transport_session_registration(&self, session_id: u64) {
        if self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id)
            .is_some()
        {
            return;
        }

        let mut by_peer = self.transport_session_abort_handles_by_peer.write().await;
        let mut empty_peers = Vec::<String>::new();
        for (peer_id, sessions) in by_peer.iter_mut() {
            if sessions.remove(&session_id).is_some() && sessions.is_empty() {
                empty_peers.push(peer_id.clone());
            }
        }
        for peer_id in empty_peers {
            by_peer.remove(&peer_id);
        }
    }

    pub async fn abort_transport_sessions_for_peer(&self, peer_id: &str) -> usize {
        let sessions = self
            .transport_session_abort_handles_by_peer
            .write()
            .await
            .remove(peer_id)
            .unwrap_or_default();
        let aborted = sessions.len();
        for handle in sessions.into_values() {
            handle.abort();
        }
        aborted
    }

    pub async fn abort_transport_sessions_for_peers(&self, peer_ids: &[String]) -> usize {
        let mut total = 0usize;
        for peer_id in peer_ids {
            total += self.abort_transport_sessions_for_peer(peer_id).await;
        }
        total
    }

    pub async fn mark_all_peers_disconnected(&self) -> Result<usize> {
        let mut config = self.config.write().await;
        let mut disconnected_peer_ids = Vec::<String>::new();

        for peer in &mut config.peers {
            if !peer.connected {
                continue;
            }
            peer.connected = false;
            peer.last_seen = Utc::now();
            disconnected_peer_ids.push(peer.peer_id.clone());
        }

        if !disconnected_peer_ids.is_empty() {
            save_config_at(&self.config_path, &config)?;
        }
        drop(config);

        if !disconnected_peer_ids.is_empty() {
            let mut router = self.input_router.write().await;
            for peer_id in &disconnected_peer_ids {
                router.release_owner(peer_id);
                router.clear_peer_state(peer_id);
            }
            drop(router);

            for peer_id in &disconnected_peer_ids {
                self.clear_pending_inject_input_frames_for_peer(peer_id)
                    .await;
            }
        }

        Ok(disconnected_peer_ids.len())
    }

    pub async fn queue_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::ClipboardText { text });
        Ok(())
    }

    pub async fn queue_clipboard_image(&self, peer_id: &str, image_bmp: Vec<u8>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        validate_bmp_payload(&image_bmp).context("invalid clipboard BMP payload")?;

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::ClipboardImage { image_bmp });
        Ok(())
    }

    pub async fn queue_local_clipboard_text_for_connected_peers(
        &self,
        text: String,
    ) -> Result<bool> {
        self.queue_local_clipboard_payload_for_connected_peers(ClipboardPayload::Text(text))
            .await
    }

    pub async fn queue_local_clipboard_image_for_connected_peers(
        &self,
        image_bmp: Vec<u8>,
    ) -> Result<bool> {
        self.queue_local_clipboard_payload_for_connected_peers(ClipboardPayload::Image(image_bmp))
            .await
    }

    async fn queue_local_clipboard_payload_for_connected_peers(
        &self,
        payload: ClipboardPayload,
    ) -> Result<bool> {
        let connected_peer_ids = self.connected_peer_ids().await;
        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(false),
        };

        {
            let mut sync = self.clipboard_sync.write().await;
            if let Some(suppress_hash) = sync.suppress_echo_hash.as_deref() {
                if suppress_hash == hash.as_str() {
                    sync.suppress_echo_hash = None;
                    sync.last_observed_hash = Some(hash);
                    return Ok(false);
                }
                // Clipboard moved on before the echo value was observed; drop stale token.
                sync.suppress_echo_hash = None;
            }

            if connected_peer_ids.is_empty() {
                // Do not cache disconnected observations. On next peer connect we should
                // still broadcast current clipboard contents.
                sync.last_observed_hash = None;
                return Ok(false);
            }

            if sync.last_observed_hash.as_deref() == Some(hash.as_str()) {
                return Ok(false);
            }
            sync.last_observed_hash = Some(hash);
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        for peer_id in &connected_peer_ids {
            let outbound = match &payload {
                ClipboardPayload::Text(text) => {
                    OutboundPayload::ClipboardText { text: text.clone() }
                }
                ClipboardPayload::Image(bytes) => OutboundPayload::ClipboardImage {
                    image_bmp: bytes.clone(),
                },
            };

            queue_map
                .entry(peer_id.clone())
                .or_default()
                .push_back(outbound);
        }

        Ok(true)
    }

    pub async fn enqueue_remote_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        self.enqueue_remote_clipboard_payload(peer_id, ClipboardPayload::Text(text))
            .await
    }

    pub async fn enqueue_remote_clipboard_image(
        &self,
        peer_id: &str,
        image_bmp: Vec<u8>,
    ) -> Result<()> {
        self.enqueue_remote_clipboard_payload(peer_id, ClipboardPayload::Image(image_bmp))
            .await
    }

    async fn enqueue_remote_clipboard_payload(
        &self,
        peer_id: &str,
        payload: ClipboardPayload,
    ) -> Result<()> {
        match &payload {
            ClipboardPayload::Text(text) => {
                self.record_incoming_clipboard_text(peer_id, text).await;
            }
            ClipboardPayload::Image(image_bmp) => {
                self.record_incoming_clipboard_image(peer_id, image_bmp.len())
                    .await;
            }
        }

        let hash = match self.validated_clipboard_payload_hash(&payload).await? {
            Some(hash) => hash,
            None => return Ok(()),
        };

        let mut sync = self.clipboard_sync.write().await;
        if sync.pending_remote.back().map(|item| item.hash.as_str()) == Some(hash.as_str()) {
            return Ok(());
        }
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_front();
        }
        sync.pending_remote
            .push_back(PendingRemoteClipboardPayload {
                peer_id: peer_id.to_string(),
                payload,
                hash,
            });
        Ok(())
    }

    pub async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload> {
        self.clipboard_sync.write().await.pending_remote.pop_front()
    }

    pub async fn requeue_remote_clipboard_payload_front(
        &self,
        item: PendingRemoteClipboardPayload,
    ) {
        let mut sync = self.clipboard_sync.write().await;
        if sync.pending_remote.len() >= MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
            sync.pending_remote.pop_back();
        }
        sync.pending_remote.push_front(item);
    }

    pub async fn mark_remote_clipboard_applied(&self, hash: &str) {
        let mut sync = self.clipboard_sync.write().await;
        sync.suppress_echo_hash = Some(hash.to_string());
        sync.last_observed_hash = Some(hash.to_string());
    }

    pub async fn queue_file_from_path(&self, peer_id: &str, file_path: &Path) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let file_name = file_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .ok_or_else(|| anyhow::anyhow!("invalid file path"))?;

        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(anyhow::Error::from)?;
        validate_transfer_size(metadata.len())?;
        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(anyhow::Error::from)?;

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::File { file_name, bytes });
        Ok(())
    }

    pub async fn queue_input_move(&self, peer_id: &str, dx: i32, dy: i32) -> Result<()> {
        self.queue_input_events(peer_id, vec![InputEvent::MouseMove { dx, dy }])
            .await
    }

    pub async fn queue_input_key(
        &self,
        peer_id: &str,
        scan_code: u16,
        key_state: KeyState,
    ) -> Result<()> {
        self.queue_input_events(
            peer_id,
            vec![InputEvent::Key {
                scan_code,
                state: key_state,
            }],
        )
        .await
    }

    pub async fn queue_input_events(&self, peer_id: &str, events: Vec<InputEvent>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        if events.is_empty() {
            anyhow::bail!("input frame must include at least one event");
        }
        if events.len() > MAX_EVENTS_PER_FRAME {
            anyhow::bail!(
                "input frame event count exceeds limit: {} > {}",
                events.len(),
                MAX_EVENTS_PER_FRAME
            );
        }

        let sequence = {
            let mut sequences = self.input_sequence_by_peer.write().await;
            let entry = sequences.entry(peer_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };

        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .entry(peer_id.to_string())
            .or_default()
            .push_back(OutboundPayload::InputFrame {
                sequence,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events,
            });
        Ok(())
    }

    pub async fn drain_outgoing(&self, peer_id: &str) -> Vec<OutboundPayload> {
        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .remove(peer_id)
            .map(|queue| queue.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
    }

    pub async fn requeue_outgoing_front(&self, peer_id: &str, payloads: Vec<OutboundPayload>) {
        if payloads.is_empty() {
            return;
        }

        let mut queue_map = self.outgoing_payloads.write().await;
        let queue = queue_map.entry(peer_id.to_string()).or_default();
        for payload in payloads.into_iter().rev() {
            queue.push_front(payload);
        }
    }

    pub async fn record_transport_event(&self, event: TransportEventRecord) {
        let mut events = self.transport_events.write().await;
        events.push_back(event);
        while events.len() > MAX_TRANSPORT_EVENTS {
            events.pop_front();
        }
    }

    pub async fn transport_events(&self) -> Vec<TransportEventRecord> {
        self.transport_events.read().await.iter().cloned().collect()
    }

    pub async fn record_incoming_clipboard_text(&self, peer_id: &str, text: &str) {
        let preview = text.chars().take(80).collect::<String>();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: preview,
            size_bytes: text.len() as u64,
        })
        .await;
    }

    pub async fn record_outgoing_clipboard_text(&self, peer_id: &str, text: &str) {
        let preview = text.chars().take(80).collect::<String>();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: peer_id.to_string(),
            detail: preview,
            size_bytes: text.len() as u64,
        })
        .await;
    }

    pub async fn record_incoming_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
            size_bytes: size_bytes as u64,
        })
        .await;
    }

    pub async fn record_outgoing_clipboard_image(&self, peer_id: &str, size_bytes: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "clipboard_image".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("bmp image {} bytes", size_bytes),
            size_bytes: size_bytes as u64,
        })
        .await;
    }

    pub async fn store_incoming_file(
        &self,
        peer_id: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<PathBuf> {
        validate_transfer_size(bytes.len() as u64)?;
        let sanitized_name = sanitize_incoming_file_name(file_name)?;

        let peer_dir = self.inbox_root.join(peer_id);
        tokio::fs::create_dir_all(&peer_dir).await?;

        let final_path = resolve_conflict_path(&peer_dir, &sanitized_name);
        if !final_path.starts_with(&peer_dir) {
            anyhow::bail!("incoming file path escaped inbox root");
        }
        tokio::fs::write(&final_path, bytes).await?;

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: final_path.display().to_string(),
            size_bytes: tokio::fs::metadata(&final_path).await?.len(),
        })
        .await;

        Ok(final_path)
    }

    pub async fn record_outgoing_file(&self, peer_id: &str, file_name: &str, size_bytes: u64) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "file".to_string(),
            peer_id: peer_id.to_string(),
            detail: file_name.to_string(),
            size_bytes,
        })
        .await;
    }

    pub async fn record_outgoing_input_frame(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        capture_timestamp_unix_ms: i64,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} capture_to_send_ms={} captured_at_unix_ms={capture_timestamp_unix_ms}",
                elapsed_ms(capture_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    async fn enqueue_pending_inject_input_frame(
        &self,
        frame: PendingInjectInputFrame,
    ) -> (usize, Option<PendingInjectInputFrame>) {
        let mut queue = self.pending_inject_input_frames.write().await;
        let dropped = if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
            queue.pop_front()
        } else {
            None
        };
        queue.push_back(frame);
        (queue.len(), dropped)
    }

    async fn clear_pending_inject_input_frames_for_peer(&self, peer_id: &str) {
        let mut queue = self.pending_inject_input_frames.write().await;
        queue.retain(|frame| frame.peer_id != peer_id);
    }

    async fn record_input_inject_queued(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        depth: usize,
        timing: InputFrameTiming,
    ) {
        let capture_to_queue_ms = elapsed_ms(
            timing.capture_timestamp_unix_ms,
            timing.queued_timestamp_unix_ms,
        );
        let receive_to_queue_ms = elapsed_ms(
            timing.received_timestamp_unix_ms,
            timing.queued_timestamp_unix_ms,
        );
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_queued".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_depth={depth} capture_to_queue_ms={capture_to_queue_ms} receive_to_queue_ms={receive_to_queue_ms}"
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    async fn record_input_inject_dropped(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        capture_timestamp_unix_ms: i64,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_dropped".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} dropped_oldest capture_age_ms={}",
                elapsed_ms(capture_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    pub async fn record_input_inject_skipped(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
        reason: &str,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_skipped".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} reason={reason} queue_wait_ms={} capture_to_skip_ms={} receive_to_skip_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    pub async fn route_incoming_input_frame(
        &self,
        peer_id: &str,
        frame: InputFrame,
    ) -> Result<RouteDecision> {
        struct RecordingInputSink {
            events: Vec<InputEvent>,
        }
        impl InputSink for RecordingInputSink {
            fn apply(&mut self, event: &InputEvent) -> std::result::Result<(), String> {
                self.events.push(event.clone());
                Ok(())
            }
        }

        let mut sink = RecordingInputSink {
            events: Vec::with_capacity(frame.events.len()),
        };
        let received_timestamp_unix_ms = Utc::now().timestamp_millis();
        let decision = self
            .input_router
            .write()
            .await
            .route_frame(&frame, &mut sink)
            .map_err(anyhow::Error::from)?;

        if matches!(decision, RouteDecision::Applied { .. }) {
            let queued_timestamp_unix_ms = Utc::now().timestamp_millis();
            let timing = InputFrameTiming {
                capture_timestamp_unix_ms: frame.timestamp_unix_ms,
                received_timestamp_unix_ms,
                queued_timestamp_unix_ms,
            };
            let pending = PendingInjectInputFrame {
                peer_id: peer_id.to_string(),
                sequence: frame.sequence,
                capture_timestamp_unix_ms: timing.capture_timestamp_unix_ms,
                received_timestamp_unix_ms: timing.received_timestamp_unix_ms,
                queued_timestamp_unix_ms: timing.queued_timestamp_unix_ms,
                events: sink.events,
            };
            let (depth, dropped) = self.enqueue_pending_inject_input_frame(pending).await;
            if let Some(dropped) = dropped {
                self.record_input_inject_dropped(
                    &dropped.peer_id,
                    dropped.sequence,
                    dropped.events.len(),
                    dropped.capture_timestamp_unix_ms,
                )
                .await;
            }
            self.record_input_inject_queued(
                peer_id,
                frame.sequence,
                frame.events.len(),
                depth,
                timing,
            )
            .await;
        }

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: describe_input_frame_decision(
                &decision,
                frame.sequence,
                frame.timestamp_unix_ms,
                received_timestamp_unix_ms,
            ),
            size_bytes: frame.events.len() as u64,
        })
        .await;

        Ok(decision)
    }

    pub async fn dequeue_pending_inject_input_frame(&self) -> Option<PendingInjectInputFrame> {
        self.pending_inject_input_frames.write().await.pop_front()
    }

    pub async fn requeue_pending_inject_input_frame_front(&self, frame: PendingInjectInputFrame) {
        let mut queue = self.pending_inject_input_frames.write().await;
        if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
            queue.pop_back();
        }
        queue.push_front(frame);
    }

    pub async fn record_input_inject_applied(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_applied".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_wait_ms={} capture_to_apply_ms={} receive_to_apply_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    pub async fn record_input_inject_failed(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
        message: &str,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_failed".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_wait_ms={} capture_to_fail_ms={} receive_to_fail_ms={} {message}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        })
        .await;
    }

    pub async fn claim_input_owner(&self, peer_id: &str, force: bool) -> Result<bool> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        Ok(self.input_router.write().await.claim_owner(peer_id, force))
    }

    pub async fn input_injection_allowed_for_peer(&self, peer_id: &str) -> bool {
        let router = self.input_router.read().await;
        router.is_enabled() && router.owner() == Some(peer_id)
    }

    pub async fn release_input_owner(&self, peer_id: &str) -> bool {
        self.input_router.write().await.release_owner(peer_id)
    }

    pub async fn input_owner(&self) -> Option<String> {
        self.input_router
            .read()
            .await
            .owner()
            .map(|owner| owner.to_string())
    }

    pub async fn set_input_capture_target(&self, peer_id: Option<&str>) -> Result<Option<String>> {
        let next = match peer_id.map(str::trim) {
            Some("") | None => None,
            Some(peer_id) => {
                if self.get_peer(peer_id).await.is_none() {
                    anyhow::bail!("unknown peer {peer_id}");
                }
                Some(peer_id.to_string())
            }
        };

        let mut target = self.input_capture_target_peer_id.write().await;
        *target = next.clone();
        Ok(next)
    }

    pub async fn clear_input_capture_target(&self) {
        *self.input_capture_target_peer_id.write().await = None;
    }

    pub async fn input_capture_target(&self) -> Option<String> {
        self.input_capture_target_peer_id.read().await.clone()
    }

    pub async fn active_input_capture_target(&self) -> Option<String> {
        let target = self.input_capture_target().await?;
        let config = self.config.read().await;
        let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        if !share_input_enabled {
            return None;
        }
        if config
            .peers
            .iter()
            .any(|peer| peer.peer_id == target && peer.connected)
        {
            Some(target)
        } else {
            None
        }
    }

    pub async fn safe_reset(&self, network_only: bool, all: bool) -> Result<()> {
        let mut config = self.config.write().await;

        if all {
            let machine_id = config.machine_id.clone();
            let device_name = config.device_name.clone();
            *config = RuntimeConfig::default();
            config.machine_id = machine_id;
            config.device_name = device_name;
        } else if network_only {
            config.peers.clear();
        }

        self.outgoing_payloads.write().await.clear();
        self.transport_events.write().await.clear();
        *self.clipboard_sync.write().await = ClipboardSyncState::default();
        self.discovered_endpoints.write().await.clear();
        *self.input_router.write().await =
            InputRouter::new(config.features.get("share_input").copied().unwrap_or(true));
        self.input_sequence_by_peer.write().await.clear();
        self.pending_inject_input_frames.write().await.clear();
        *self.input_capture_target_peer_id.write().await = None;
        self.reconnect_generation_by_peer.write().await.clear();
        self.pairing_codes.write().await.clear();
        self.pending_nearby_pairing_requests.write().await.clear();
        self.nearby_pairing_decisions.write().await.clear();
        self.pending_transport_session_abort_handles
            .write()
            .await
            .clear();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .clear();

        save_config_at(&self.config_path, &config)
    }

    pub async fn diagnostics_dump(&self, output_path: Option<String>) -> Result<String> {
        let target = output_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("Boundless")
                    .join("diagnostics")
            });

        tokio::fs::create_dir_all(&target).await?;

        let file_path = target.join(format!("dump-{}.txt", Utc::now().format("%Y%m%d-%H%M%S")));

        let snapshot = self.snapshot().await;
        let trust_count = self
            .trusted_records()
            .await
            .map(|items| items.len())
            .unwrap_or(0);
        let event_count = self.transport_events.read().await.len();
        let input_owner = self
            .input_owner()
            .await
            .unwrap_or_else(|| "none".to_string());
        let input_capture_target = self
            .input_capture_target()
            .await
            .unwrap_or_else(|| "none".to_string());

        let report = format!(
            "Boundless Diagnostics\nMachine: {}\nFingerprint: {}\nPeers: {}\nTrusted CAs: {}\nTransport Events: {}\nInput Owner: {}\nInput Capture Target: {}\nAPI: {}\nTransport Port: {}\nProtocol: {}\n",
            snapshot.machine_id,
            self.fingerprint(),
            snapshot.peers.len(),
            trust_count,
            event_count,
            input_owner,
            input_capture_target,
            snapshot.api_bind,
            snapshot.network_port,
            snapshot.protocol_version
        );

        tokio::fs::write(&file_path, report).await?;
        Ok(file_path.display().to_string())
    }

    async fn connected_peer_ids(&self) -> Vec<String> {
        self.config
            .read()
            .await
            .peers
            .iter()
            .filter(|peer| peer.connected)
            .map(|peer| peer.peer_id.clone())
            .collect()
    }

    async fn validated_clipboard_payload_hash(
        &self,
        payload: &ClipboardPayload,
    ) -> Result<Option<String>> {
        if let ClipboardPayload::Image(image_bmp) = payload {
            validate_bmp_payload(image_bmp).context("invalid clipboard BMP payload")?;
        }

        let policy = ClipboardPolicy {
            enabled: self
                .config
                .read()
                .await
                .features
                .get("share_clipboard")
                .copied()
                .unwrap_or(true),
            ..ClipboardPolicy::default()
        };

        match validate_payload(policy, payload) {
            Ok(()) => Ok(Some(payload_hash_hex(payload))),
            Err(ClipboardPolicyError::Disabled) => Ok(None),
            Err(error) => Err(anyhow::anyhow!(error)),
        }
    }
}

fn validate_bind_address(bind: &str) -> Result<()> {
    bind.parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid bind address {bind}"))?;
    Ok(())
}

fn normalize_peer_address(address: &str, default_port: u16) -> Result<String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        anyhow::bail!("peer address must not be empty");
    }

    if let Some(parsed) = parse_manual_target(trimmed, default_port) {
        return Ok(parsed.to_string());
    }

    Ok(trimmed.to_string())
}

fn validate_pipe_name(pipe_name: &str) -> Result<()> {
    let trimmed = pipe_name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("pipe name must not be empty");
    }

    if trimmed.contains('/') || trimmed.contains('\\') {
        anyhow::bail!("pipe name must not contain path separators");
    }

    Ok(())
}

fn normalize_optional_alias(alias: String) -> Option<String> {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn validate_and_consume_pairing_code(
    pairing_codes: &mut HashMap<String, DateTime<Utc>>,
    code: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    if code.trim().is_empty() {
        anyhow::bail!("pairing code must not be empty");
    }

    let Some(expires_at) = pairing_codes.remove(code) else {
        anyhow::bail!("pairing code is invalid or was already used");
    };

    if expires_at < now {
        anyhow::bail!("pairing code has expired");
    }

    Ok(())
}

fn validate_ca_cert_pem(ca_cert_pem: &str) -> Result<()> {
    let certs = CertificateDer::pem_slice_iter(ca_cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse trust bundle CA certificate PEM")?;

    if certs.is_empty() {
        anyhow::bail!("trust bundle must include at least one CA certificate");
    }

    Ok(())
}

fn sanitize_incoming_file_name(file_name: &str) -> Result<String> {
    let mut components = Path::new(file_name).components();
    let Some(component) = components.next() else {
        anyhow::bail!("incoming file name must not be empty");
    };
    if components.next().is_some() {
        anyhow::bail!("incoming file name must not include path separators");
    }

    let Component::Normal(name) = component else {
        anyhow::bail!("incoming file name must be a plain file name");
    };

    let sanitized = name.to_string_lossy().trim().to_string();
    if sanitized.is_empty() {
        anyhow::bail!("incoming file name must not be empty");
    }

    Ok(sanitized)
}

fn describe_route_decision(decision: &RouteDecision) -> String {
    match decision {
        RouteDecision::Applied { event_count } => format!("applied events={event_count}"),
        RouteDecision::IgnoredFeatureDisabled => "ignored feature_disabled".to_string(),
        RouteDecision::IgnoredNoOwner => "ignored no_owner".to_string(),
        RouteDecision::IgnoredWrongOwner { owner_peer_id } => {
            format!("ignored wrong_owner={owner_peer_id}")
        }
    }
}

fn describe_input_frame_decision(
    decision: &RouteDecision,
    sequence: u64,
    capture_timestamp_unix_ms: i64,
    received_timestamp_unix_ms: i64,
) -> String {
    let capture_to_receive_ms = elapsed_ms(capture_timestamp_unix_ms, received_timestamp_unix_ms);
    format!(
        "sequence={sequence} capture_to_receive_ms={capture_to_receive_ms} {}",
        describe_route_decision(decision)
    )
}

fn elapsed_ms(start_unix_ms: i64, end_unix_ms: i64) -> i64 {
    (end_unix_ms - start_unix_ms).max(0)
}

fn resolve_capture_handoff_target(
    config: &RuntimeConfig,
    direction: SwitchDirection,
) -> Option<String> {
    let matrix = parse_layout_matrix(&config.layout_matrix);
    let mut local_cell: Option<(usize, usize)> = None;

    for (row_index, row) in matrix.iter().enumerate() {
        for (column_index, token) in row.iter().enumerate() {
            if !is_local_layout_token(token, config) {
                continue;
            }

            if local_cell.is_some() {
                return None;
            }
            local_cell = Some((row_index, column_index));
        }
    }

    let (row, column) = local_cell?;

    let token_at = |row_index: usize, column_index: usize| -> Option<String> {
        matrix
            .get(row_index)
            .and_then(|row_tokens| row_tokens.get(column_index))
            .cloned()
    };

    match direction {
        SwitchDirection::Left => {
            for next_column in (0..column).rev() {
                let Some(token) = token_at(row, next_column) else {
                    continue;
                };
                if is_local_layout_token(&token, config) {
                    continue;
                }
                if let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) {
                    return Some(peer_id);
                }
            }
            None
        }
        SwitchDirection::Right => {
            let row_width = matrix
                .get(row)
                .map(|row_tokens| row_tokens.len())
                .unwrap_or(0);
            for next_column in (column + 1)..row_width {
                let Some(token) = token_at(row, next_column) else {
                    continue;
                };
                if is_local_layout_token(&token, config) {
                    continue;
                }
                if let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) {
                    return Some(peer_id);
                }
            }
            None
        }
        SwitchDirection::Up => {
            for next_row in (0..row).rev() {
                let Some(token) = token_at(next_row, column) else {
                    continue;
                };
                if is_local_layout_token(&token, config) {
                    continue;
                }
                if let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) {
                    return Some(peer_id);
                }
            }
            None
        }
        SwitchDirection::Down => {
            for next_row in (row + 1)..matrix.len() {
                let Some(token) = token_at(next_row, column) else {
                    continue;
                };
                if is_local_layout_token(&token, config) {
                    continue;
                }
                if let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) {
                    return Some(peer_id);
                }
            }
            None
        }
    }
}

fn resolve_switch_all_target_order(config: &RuntimeConfig) -> Vec<String> {
    let mut ordered = Vec::<String>::new();
    let mut seen = HashSet::<String>::new();

    for row in parse_layout_matrix(&config.layout_matrix) {
        for token in row {
            if is_local_layout_token(&token, config) {
                continue;
            }
            let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) else {
                continue;
            };
            if seen.insert(peer_id.clone()) {
                ordered.push(peer_id);
            }
        }
    }

    let mut remainder = config
        .peers
        .iter()
        .filter(|peer| peer.connected)
        .map(|peer| (peer.display_name.to_ascii_lowercase(), peer.peer_id.clone()))
        .collect::<Vec<_>>();
    remainder
        .sort_by(|(name_a, id_a), (name_b, id_b)| name_a.cmp(name_b).then_with(|| id_a.cmp(id_b)));
    for (_, peer_id) in remainder {
        if seen.insert(peer_id.clone()) {
            ordered.push(peer_id);
        }
    }

    ordered
}

fn parse_layout_matrix(spec: &str) -> Vec<Vec<String>> {
    spec.split(';')
        .map(|row| {
            row.split(',')
                .map(|token| token.trim().to_string())
                .collect()
        })
        .collect()
}

fn is_local_layout_token(token: &str, config: &RuntimeConfig) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }

    matches!(
        token.to_ascii_lowercase().as_str(),
        "self" | "local" | "this" | "me"
    ) || token.eq_ignore_ascii_case(&config.machine_id)
        || token.eq_ignore_ascii_case(&config.device_name)
}

fn resolve_peer_layout_token(token: &str, peers: &[PeerConfig]) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    let token_lower = token.to_ascii_lowercase();
    let mut matched_peer_ids = Vec::<String>::new();

    for peer in peers.iter().filter(|peer| peer.connected) {
        let peer_id_match = peer.peer_id.eq_ignore_ascii_case(token);
        let display_name_match = peer.display_name.eq_ignore_ascii_case(token);
        let peer_id_prefix_match = peer.peer_id.to_ascii_lowercase().starts_with(&token_lower);
        if !(peer_id_match || display_name_match || peer_id_prefix_match) {
            continue;
        }

        if !matched_peer_ids
            .iter()
            .any(|peer_id| peer_id == &peer.peer_id)
        {
            matched_peer_ids.push(peer.peer_id.clone());
        }
    }

    if matched_peer_ids.len() == 1 {
        matched_peer_ids.pop()
    } else {
        None
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
            resolve_capture_handoff_target(&config, SwitchDirection::Left).as_deref(),
            Some("peer-left")
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, SwitchDirection::Right).as_deref(),
            Some("peer-right")
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, SwitchDirection::Up).as_deref(),
            Some("peer-up")
        );
        assert_eq!(
            resolve_capture_handoff_target(&config, SwitchDirection::Down).as_deref(),
            Some("peer-down")
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
            resolve_capture_handoff_target(&config, SwitchDirection::Right).is_none(),
            "disconnected neighbors should not be selected"
        );

        config.peers[0].connected = true;
        config.layout_matrix = "self,right;local,right".to_string();
        assert!(
            resolve_capture_handoff_target(&config, SwitchDirection::Right).is_none(),
            "multiple local cells should invalidate edge handoff resolution"
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
