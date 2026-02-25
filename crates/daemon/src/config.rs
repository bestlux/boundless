use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use core_protocol::PROTOCOL_CURRENT;

const DEFAULT_LAYOUT_MATRIX: &str = "self";
const RUNTIME_CONFIG_VERSION: &str = "2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    pub connected: bool,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiTransport {
    Tcp,
    NamedPipe,
}

impl ApiTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::NamedPipe => "named_pipe",
        }
    }

    pub fn effective(self) -> Self {
        #[cfg(windows)]
        {
            self
        }

        #[cfg(not(windows))]
        {
            match self {
                Self::Tcp => Self::Tcp,
                Self::NamedPipe => Self::Tcp,
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub config_version: String,
    pub machine_id: String,
    pub device_name: String,
    pub api_bind: String,
    #[serde(default = "default_api_transport")]
    pub api_transport: ApiTransport,
    #[serde(default = "default_api_pipe_name")]
    pub api_pipe_name: String,
    pub protocol_version: String,
    pub layout_matrix: String,
    pub auto_start: bool,
    pub network_port: u16,
    pub features: BTreeMap<String, bool>,
    pub hotkeys: BTreeMap<String, String>,
    pub peers: Vec<PeerConfig>,
    pub updated_at: DateTime<Utc>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let now = Utc::now();

        let features = [
            ("share_clipboard".to_string(), true),
            ("transfer_file".to_string(), true),
            ("share_input".to_string(), true),
            ("easy_mouse".to_string(), true),
            ("wrap_mouse".to_string(), true),
        ]
        .into_iter()
        .collect();

        let hotkeys = [
            (
                "toggle_easy_mouse".to_string(),
                "Ctrl+Alt+Shift+E".to_string(),
            ),
            ("lock_machine".to_string(), "Ctrl+Alt+Shift+L".to_string()),
            ("switch_all".to_string(), "Disabled".to_string()),
            ("reconnect".to_string(), "Ctrl+Alt+Shift+R".to_string()),
        ]
        .into_iter()
        .collect();

        Self {
            config_version: default_config_version(),
            machine_id: Uuid::new_v4().to_string(),
            device_name: hostname(),
            api_bind: "127.0.0.1:50051".to_string(),
            api_transport: default_api_transport(),
            api_pipe_name: default_api_pipe_name(),
            protocol_version: PROTOCOL_CURRENT.to_string(),
            layout_matrix: DEFAULT_LAYOUT_MATRIX.to_string(),
            auto_start: true,
            network_port: 15100,
            features,
            hotkeys,
            peers: Vec::new(),
            updated_at: now,
        }
    }
}

fn default_config_version() -> String {
    RUNTIME_CONFIG_VERSION.to_string()
}

fn default_api_transport() -> ApiTransport {
    if cfg!(windows) {
        ApiTransport::NamedPipe
    } else {
        ApiTransport::Tcp
    }
}

fn default_api_pipe_name() -> String {
    "boundlessd-api".to_string()
}

pub fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("BOUNDLESS_CONFIG_PATH") {
        return PathBuf::from(path);
    }

    dirs::config_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("config.json")
}

pub fn load_or_create_config_at(path: &Path) -> Result<RuntimeConfig> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    if !path.exists() {
        let config = RuntimeConfig::default();
        save_config_at(path, &config)?;
        return Ok(config);
    }

    let data = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let config: RuntimeConfig =
        serde_json::from_str(&data).with_context(|| format!("parse {}", path.display()))?;

    if config.config_version != RUNTIME_CONFIG_VERSION {
        bail!(
            "unsupported config version `{}`; expected `{}`. remove `{}` to regenerate config for this build",
            config.config_version,
            RUNTIME_CONFIG_VERSION,
            path.display()
        );
    }

    if config.protocol_version != PROTOCOL_CURRENT.to_string() {
        bail!(
            "unsupported protocol version `{}` in config; expected `{}`. remove `{}` to regenerate config for this build",
            config.protocol_version,
            PROTOCOL_CURRENT,
            path.display()
        );
    }

    if config.layout_matrix.trim().is_empty() {
        bail!(
            "invalid config: layout_matrix must not be empty in `{}`",
            path.display()
        );
    }

    Ok(config)
}

pub fn save_config_at(path: &Path, config: &RuntimeConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut cloned = config.clone();
    cloned.updated_at = Utc::now();

    let payload = serde_json::to_string_pretty(&cloned).context("serialize config")?;
    fs::write(path, payload).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "boundless-host".to_string())
}

#[cfg(test)]
mod tests {
    use super::{ApiTransport, RuntimeConfig, load_or_create_config_at, save_config_at};
    use core_protocol::PROTOCOL_CURRENT;

    #[test]
    fn tcp_effective_transport_is_tcp() {
        assert!(matches!(ApiTransport::Tcp.effective(), ApiTransport::Tcp));
    }

    #[cfg(not(windows))]
    #[test]
    fn named_pipe_effective_transport_falls_back_to_tcp_on_non_windows() {
        assert!(matches!(
            ApiTransport::NamedPipe.effective(),
            ApiTransport::Tcp
        ));
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_effective_transport_stays_named_pipe_on_windows() {
        assert!(matches!(
            ApiTransport::NamedPipe.effective(),
            ApiTransport::NamedPipe
        ));
    }

    #[test]
    fn load_or_create_config_rejects_missing_config_version() {
        let json = r#"{
  "machine_id": "m1",
  "device_name": "node",
  "api_bind": "127.0.0.1:50051",
  "api_transport": "tcp",
  "api_pipe_name": "boundlessd-api",
  "protocol_version": "1.1.0",
  "layout_matrix": "self",
  "auto_start": true,
  "network_port": 15100,
  "features": {
    "share_clipboard": true,
    "transfer_file": true,
    "share_input": true,
    "easy_mouse": true,
    "wrap_mouse": true
  },
  "hotkeys": {
    "toggle_easy_mouse": "Ctrl+Alt+Shift+E",
    "lock_machine": "Ctrl+Alt+Shift+L",
    "switch_all": "Disabled",
    "reconnect": "Ctrl+Alt+Shift+R"
  },
  "peers": [],
  "updated_at": "2026-01-01T00:00:00Z"
}"#;

        let root = std::env::temp_dir().join(format!(
            "boundless-config-missing-version-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");
        std::fs::write(&path, json).expect("write seeded config");

        let _error = load_or_create_config_at(&path).expect_err("must reject missing version");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_config_rejects_protocol_version_mismatch() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-protocol-mismatch-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let seed = RuntimeConfig {
            protocol_version: "2.0.0".to_string(),
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).expect("seed stale config");

        let error = load_or_create_config_at(&path).expect_err("must reject stale protocol");
        assert!(
            error
                .to_string()
                .contains(&format!("expected `{}`", PROTOCOL_CURRENT)),
            "unexpected error: {error:#}"
        );
        assert!(
            error.to_string().contains("regenerate config"),
            "unexpected error: {error:#}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_config_rejects_empty_layout_matrix() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-empty-layout-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let seed = RuntimeConfig {
            layout_matrix: "   ".to_string(),
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).expect("seed invalid config");

        let error = load_or_create_config_at(&path).expect_err("must reject empty layout");
        assert!(
            error
                .to_string()
                .contains("layout_matrix must not be empty"),
            "unexpected error: {error:#}"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
