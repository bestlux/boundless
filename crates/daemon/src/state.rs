use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::info;

use core_security::{
    SecurityPaths, default_security_root, ensure_trust_store, fingerprint, generate_pairing_code,
    load_or_create_device_secret,
};

use crate::config::{PeerConfig, RuntimeConfig, load_or_create_config, save_config};

#[derive(Clone)]
pub struct AppState {
    config: Arc<RwLock<RuntimeConfig>>,
    pairing_codes: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    device_fingerprint: Arc<String>,
}

impl AppState {
    pub fn load_or_create() -> Result<Self> {
        let config = load_or_create_config()?;

        let paths = SecurityPaths::for_root(default_security_root());
        let secret = load_or_create_device_secret(&paths)?;
        ensure_trust_store(&paths)?;
        let fingerprint = fingerprint(&secret);

        info!(machine_id = %config.machine_id, "state loaded");

        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            pairing_codes: Arc::new(RwLock::new(HashMap::new())),
            device_fingerprint: Arc::new(fingerprint),
        })
    }

    pub fn fingerprint(&self) -> &str {
        self.device_fingerprint.as_ref().as_str()
    }

    pub async fn snapshot(&self) -> RuntimeConfig {
        self.config.read().await.clone()
    }

    pub async fn update_bind(&self, bind: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.api_bind = bind;
        save_config(&config)
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
        if code.trim().is_empty() {
            anyhow::bail!("pairing code must not be empty");
        }

        let mut config = self.config.write().await;
        let peer_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();

        let peer = PeerConfig {
            peer_id: peer_id.clone(),
            display_name: alias.unwrap_or_else(|| format!("peer-{}", &peer_id[..8])),
            address: host,
            connected: true,
            last_seen: now,
        };

        config.peers.push(peer);
        save_config(&config)?;
        Ok(peer_id)
    }

    pub async fn list_peers(&self) -> Vec<PeerConfig> {
        self.config.read().await.peers.clone()
    }

    pub async fn remove_peer(&self, peer_id: &str) -> Result<bool> {
        let mut config = self.config.write().await;
        let before = config.peers.len();
        config.peers.retain(|p| p.peer_id != peer_id);
        let removed = before != config.peers.len();
        if removed {
            save_config(&config)?;
        }
        Ok(removed)
    }

    pub async fn layout(&self) -> String {
        self.config.read().await.layout_matrix.clone()
    }

    pub async fn set_layout(&self, matrix: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.layout_matrix = matrix;
        save_config(&config)
    }

    pub async fn set_feature(&self, name: String, enabled: bool) -> Result<()> {
        let mut config = self.config.write().await;
        config.features.insert(name, enabled);
        save_config(&config)
    }

    pub async fn feature_map(&self) -> std::collections::BTreeMap<String, bool> {
        self.config.read().await.features.clone()
    }

    pub async fn set_hotkey(&self, action: String, combo: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.hotkeys.insert(action, combo);
        save_config(&config)
    }

    pub async fn safe_reset(&self, network_only: bool, all: bool) -> Result<()> {
        let mut config = self.config.write().await;

        if all {
            let machine_id = config.machine_id.clone();
            *config = RuntimeConfig::default();
            config.machine_id = machine_id;
        } else if network_only {
            config.peers.clear();
        }

        save_config(&config)
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
        let report = format!(
            "Boundless Diagnostics\nMachine: {}\nFingerprint: {}\nPeers: {}\nAPI: {}\nProtocol: {}\n",
            snapshot.machine_id,
            self.fingerprint(),
            snapshot.peers.len(),
            snapshot.api_bind,
            snapshot.protocol_version
        );

        tokio::fs::write(&file_path, report).await?;
        Ok(file_path.display().to_string())
    }
}
