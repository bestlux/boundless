use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::sync::RwLock;
use tracing::info;

use core_clipboard::{
    ClipboardPayload, ClipboardPolicy, ClipboardPolicyError, payload_hash_hex, validate_payload,
};
use core_discovery::parse_manual_target;
use core_input::{InputEvent, InputFrame, InputRouter, InputSink, RouteDecision};
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

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    ClipboardText {
        text: String,
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
pub struct PendingRemoteClipboardText {
    pub peer_id: String,
    pub text: String,
    pub hash: String,
}

#[derive(Debug, Default)]
struct ClipboardSyncState {
    last_observed_hash: Option<String>,
    suppress_echo_hash: Option<String>,
    pending_remote: VecDeque<PendingRemoteClipboardText>,
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

    pub async fn join_peer(
        &self,
        code: String,
        host: String,
        alias: Option<String>,
    ) -> Result<String> {
        let now = Utc::now();
        {
            let mut pairing_codes = self.pairing_codes.write().await;
            validate_and_consume_pairing_code(&mut pairing_codes, &code, now)?;
            pairing_codes.retain(|_, expires_at| *expires_at >= now);
        }

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

    pub async fn set_hotkey(&self, action: String, combo: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.hotkeys.insert(action, combo);
        save_config_at(&self.config_path, &config)
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

    pub async fn queue_local_clipboard_text_for_connected_peers(
        &self,
        text: String,
    ) -> Result<bool> {
        let connected_peer_ids = self.connected_peer_ids().await;
        let hash = match self.validated_clipboard_text_hash(&text).await? {
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
            queue_map
                .entry(peer_id.clone())
                .or_default()
                .push_back(OutboundPayload::ClipboardText { text: text.clone() });
        }

        Ok(true)
    }

    pub async fn enqueue_remote_clipboard_text(&self, peer_id: &str, text: String) -> Result<()> {
        self.record_incoming_clipboard_text(peer_id, &text).await;

        let hash = match self.validated_clipboard_text_hash(&text).await? {
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
        sync.pending_remote.push_back(PendingRemoteClipboardText {
            peer_id: peer_id.to_string(),
            text,
            hash,
        });
        Ok(())
    }

    pub async fn dequeue_remote_clipboard_text(&self) -> Option<PendingRemoteClipboardText> {
        self.clipboard_sync.write().await.pending_remote.pop_front()
    }

    pub async fn requeue_remote_clipboard_text_front(&self, item: PendingRemoteClipboardText) {
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
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
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
                events: vec![InputEvent::MouseMove { dx, dy }],
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

    pub async fn record_outgoing_input_frame(&self, peer_id: &str, event_count: usize) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "outgoing".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: "queued_input_frame_sent".to_string(),
            size_bytes: event_count as u64,
        })
        .await;
    }

    pub async fn route_incoming_input_frame(
        &self,
        peer_id: &str,
        frame: InputFrame,
    ) -> Result<RouteDecision> {
        struct NoopInputSink;
        impl InputSink for NoopInputSink {
            fn apply(&mut self, _event: &InputEvent) -> std::result::Result<(), String> {
                Ok(())
            }
        }

        let mut sink = NoopInputSink;
        let decision = self
            .input_router
            .write()
            .await
            .route_frame(&frame, &mut sink)
            .map_err(anyhow::Error::from)?;

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: describe_route_decision(&decision),
            size_bytes: frame.events.len() as u64,
        })
        .await;

        Ok(decision)
    }

    pub async fn claim_input_owner(&self, peer_id: &str, force: bool) -> Result<bool> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        Ok(self.input_router.write().await.claim_owner(peer_id, force))
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

        let report = format!(
            "Boundless Diagnostics\nMachine: {}\nFingerprint: {}\nPeers: {}\nTrusted CAs: {}\nTransport Events: {}\nInput Owner: {}\nAPI: {}\nTransport Port: {}\nProtocol: {}\n",
            snapshot.machine_id,
            self.fingerprint(),
            snapshot.peers.len(),
            trust_count,
            event_count,
            input_owner,
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

    async fn validated_clipboard_text_hash(&self, text: &str) -> Result<Option<String>> {
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
        let payload = ClipboardPayload::Text(text.to_string());

        match validate_payload(policy, &payload) {
            Ok(()) => Ok(Some(payload_hash_hex(&payload))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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
            .dequeue_remote_clipboard_text()
            .await
            .expect("remote item");
        assert_eq!(remote.text, "remote");
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
            .dequeue_remote_clipboard_text()
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
}
