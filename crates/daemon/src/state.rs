use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::sync::RwLock;
use tracing::info;

use core_security::{
    DeviceIdentity, SecurityPaths, TrustBundle, TrustRecord, default_security_root,
    ensure_device_identity, ensure_trust_store, fingerprint, generate_pairing_code,
    load_or_create_device_secret, load_trust_records, upsert_trust_record,
};
use core_transfer::{resolve_conflict_path, validate_transfer_size};

use crate::config::{
    PeerConfig, RuntimeConfig, config_path, load_or_create_config_at, save_config_at,
};

const MAX_TRANSPORT_EVENTS: usize = 512;

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    ClipboardText { text: String },
    File { file_name: String, bytes: Vec<u8> },
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
    inbox_root: Arc<PathBuf>,
    input_owner_peer_id: Arc<RwLock<Option<String>>>,
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

        Ok(Self {
            config_path: Arc::new(config_path),
            config: Arc::new(RwLock::new(config)),
            pairing_codes: Arc::new(RwLock::new(HashMap::new())),
            security_paths: Arc::new(paths),
            identity: Arc::new(identity),
            device_fingerprint: Arc::new(fingerprint),
            outgoing_payloads: Arc::new(RwLock::new(HashMap::new())),
            transport_events: Arc::new(RwLock::new(VecDeque::new())),
            inbox_root: Arc::new(inbox_root),
            input_owner_peer_id: Arc::new(RwLock::new(None)),
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
            peer.address = bundle.network_address;
            peer.display_name = alias.unwrap_or(bundle.display_name);
            peer.connected = false;
            peer.last_seen = Utc::now();
        } else {
            config.peers.push(PeerConfig {
                peer_id: bundle.machine_id,
                display_name: alias.unwrap_or(bundle.display_name),
                address: bundle.network_address,
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

    pub async fn update_network_port(&self, port: u16) -> Result<()> {
        let mut config = self.config.write().await;
        config.network_port = port;
        save_config_at(&self.config_path, &config)
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
        let peer_id = uuid::Uuid::new_v4().to_string();

        let peer = PeerConfig {
            peer_id: peer_id.clone(),
            display_name: alias.unwrap_or_else(|| format!("peer-{}", &peer_id[..8])),
            address: host,
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
            let mut owner = self.input_owner_peer_id.write().await;
            if owner.as_deref() == Some(peer_id) {
                *owner = None;
            }
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
        config.features.insert(name, enabled);
        save_config_at(&self.config_path, &config)
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

    pub async fn drain_outgoing(&self, peer_id: &str) -> Vec<OutboundPayload> {
        let mut queue_map = self.outgoing_payloads.write().await;
        queue_map
            .remove(peer_id)
            .map(|queue| queue.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
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

        let peer_dir = self.inbox_root.join(peer_id);
        tokio::fs::create_dir_all(&peer_dir).await?;

        let final_path = resolve_conflict_path(&peer_dir, file_name);
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

    pub async fn claim_input_owner(&self, peer_id: &str, force: bool) -> Result<bool> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let mut owner = self.input_owner_peer_id.write().await;
        let acquired = match owner.as_deref() {
            None => {
                *owner = Some(peer_id.to_string());
                true
            }
            Some(current) if current == peer_id => true,
            Some(_) if force => {
                *owner = Some(peer_id.to_string());
                true
            }
            Some(_) => false,
        };

        Ok(acquired)
    }

    pub async fn release_input_owner(&self, peer_id: &str) -> bool {
        let mut owner = self.input_owner_peer_id.write().await;
        if owner.as_deref() == Some(peer_id) {
            *owner = None;
            return true;
        }

        false
    }

    pub async fn input_owner(&self) -> Option<String> {
        self.input_owner_peer_id.read().await.clone()
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
        *self.input_owner_peer_id.write().await = None;

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
}

fn validate_bind_address(bind: &str) -> Result<()> {
    bind.parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid bind address {bind}"))?;
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
}
