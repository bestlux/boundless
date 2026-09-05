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
use core_security::atomic_write_file;
use core_transfer::MAX_TRANSFER_BYTES;

const DEFAULT_LAYOUT_MATRIX: &str = "self";
const RUNTIME_CONFIG_VERSION: &str = "7";
const LEGACY_DEFAULT_NETWORK_PORT: u16 = 15100;
mod migration_backup;
const MIGRATABLE_PROTOCOL_VERSIONS: &[&str] = &["4.1.0", "4.2.0", "4.3.0", "4.4.0"];
const DEFAULT_ANTI_IDLE_RECENT_ACTIVITY_WINDOW_SECS: u32 = 300;
const DEFAULT_ANTI_IDLE_PULSE_INTERVAL_SECS: u32 = 30;
const DEFAULT_INPUT_CORNER_BLOCK_PX: u32 = 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub peer_id: String,
    pub display_name: String,
    pub address: String,
    // Runtime observations are neither persisted nor restored from old files.
    #[serde(skip)]
    pub connected: bool,
    #[serde(skip)]
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
    #[serde(default)]
    pub anti_idle: AntiIdleConfig,
    #[serde(default)]
    pub file_transfer: FileTransferConfig,
    #[serde(default)]
    pub input_handoff: InputHandoffConfig,
    pub hotkeys: BTreeMap<String, String>,
    pub peers: Vec<PeerConfig>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AntiIdleConfig {
    #[serde(default = "default_anti_idle_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recent_activity_window_secs")]
    pub recent_activity_window_secs: u32,
    #[serde(default = "default_allow_on_battery")]
    pub allow_on_battery: bool,
    #[serde(default = "default_keep_display_on")]
    pub keep_display_on: bool,
    #[serde(default = "default_pulse_interval_secs")]
    pub pulse_interval_secs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileTransferConfig {
    #[serde(default = "default_file_receive_dir")]
    pub receive_dir: String,
    #[serde(default)]
    pub organize_by_peer: bool,
    #[serde(default = "default_file_auto_accept_trusted_peers")]
    pub auto_accept_trusted_peers: bool,
    #[serde(default = "default_file_max_bytes")]
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputHandoffConfig {
    #[serde(default = "default_input_block_screen_corners")]
    pub block_screen_corners: bool,
    #[serde(default = "default_input_corner_block_px")]
    pub corner_block_px: u32,
    #[serde(default = "default_input_relative_mouse")]
    pub relative_mouse: bool,
    #[serde(default = "default_input_hide_cursor_at_edge")]
    pub hide_cursor_at_edge: bool,
    #[serde(default = "default_input_draw_cursor_marker")]
    pub draw_cursor_marker: bool,
}

impl Default for AntiIdleConfig {
    fn default() -> Self {
        Self {
            enabled: default_anti_idle_enabled(),
            recent_activity_window_secs: default_recent_activity_window_secs(),
            allow_on_battery: default_allow_on_battery(),
            keep_display_on: default_keep_display_on(),
            pulse_interval_secs: default_pulse_interval_secs(),
        }
    }
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            receive_dir: default_file_receive_dir(),
            organize_by_peer: false,
            auto_accept_trusted_peers: default_file_auto_accept_trusted_peers(),
            max_file_bytes: default_file_max_bytes(),
        }
    }
}

impl Default for InputHandoffConfig {
    fn default() -> Self {
        Self {
            block_screen_corners: default_input_block_screen_corners(),
            corner_block_px: default_input_corner_block_px(),
            relative_mouse: default_input_relative_mouse(),
            hide_cursor_at_edge: default_input_hide_cursor_at_edge(),
            draw_cursor_marker: default_input_draw_cursor_marker(),
        }
    }
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
            network_port: app_services::desktop::DEFAULT_NETWORK_PORT,
            features,
            anti_idle: AntiIdleConfig::default(),
            hotkeys,
            peers: Vec::new(),
            file_transfer: FileTransferConfig::default(),
            input_handoff: InputHandoffConfig::default(),
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

fn default_anti_idle_enabled() -> bool {
    true
}

fn default_recent_activity_window_secs() -> u32 {
    DEFAULT_ANTI_IDLE_RECENT_ACTIVITY_WINDOW_SECS
}

fn default_allow_on_battery() -> bool {
    false
}

fn default_keep_display_on() -> bool {
    false
}

fn default_pulse_interval_secs() -> u32 {
    DEFAULT_ANTI_IDLE_PULSE_INTERVAL_SECS
}

fn default_file_auto_accept_trusted_peers() -> bool {
    false
}

fn default_file_max_bytes() -> u64 {
    MAX_TRANSFER_BYTES
}

pub(crate) fn default_file_receive_dir() -> String {
    dirs::download_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .display()
        .to_string()
}

fn default_input_block_screen_corners() -> bool {
    true
}

fn default_input_corner_block_px() -> u32 {
    DEFAULT_INPUT_CORNER_BLOCK_PX
}

fn default_input_relative_mouse() -> bool {
    true
}

fn default_input_hide_cursor_at_edge() -> bool {
    false
}

fn default_input_draw_cursor_marker() -> bool {
    false
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
    migration_backup::validate_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    if !path.exists() {
        let config = RuntimeConfig::default();
        save_config_at(path, &config)?;
        return Ok(config);
    }

    let data = migration_backup::read_bounded_config(path)?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))?;

    let changed = migrate_config_value(path, &mut value)?;

    let config: RuntimeConfig =
        serde_json::from_value(value).with_context(|| format!("parse {}", path.display()))?;

    if config.config_version != RUNTIME_CONFIG_VERSION {
        bail!(
            "unsupported config version `{}`; expected `{}`. use a compatible build or restore a pre-upgrade configuration backup for `{}`",
            config.config_version,
            RUNTIME_CONFIG_VERSION,
            path.display()
        );
    }

    if config.protocol_version != PROTOCOL_CURRENT.to_string() {
        bail!(
            "unsupported protocol version `{}` in config; expected `{}`. use a compatible build or restore a pre-upgrade configuration backup for `{}`",
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

    if config.anti_idle.recent_activity_window_secs == 0 {
        bail!(
            "invalid config: anti_idle.recent_activity_window_secs must be greater than zero in `{}`",
            path.display()
        );
    }

    if config.anti_idle.pulse_interval_secs == 0 {
        bail!(
            "invalid config: anti_idle.pulse_interval_secs must be greater than zero in `{}`",
            path.display()
        );
    }

    if config.file_transfer.receive_dir.trim().is_empty() {
        bail!(
            "invalid config: file_transfer.receive_dir must not be empty in `{}`",
            path.display()
        );
    }

    if config.file_transfer.max_file_bytes == 0 {
        bail!(
            "invalid config: file_transfer.max_file_bytes must be greater than zero in `{}`",
            path.display()
        );
    }

    if config.input_handoff.corner_block_px > 256 {
        bail!(
            "invalid config: input_handoff.corner_block_px must be <= 256 in `{}`",
            path.display()
        );
    }

    if changed {
        // Validate the entire migrated configuration before touching the original.
        migration_backup::create_once(path, &data)?;
        save_config_at(path, &config)?;
    }
    Ok(config)
}

pub fn save_config_at(path: &Path, config: &RuntimeConfig) -> Result<()> {
    migration_backup::validate_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut cloned = config.clone();
    cloned.updated_at = Utc::now();

    let payload = serde_json::to_string_pretty(&cloned).context("serialize config")?;
    if payload.len() as u64 > migration_backup::MAX_CONFIG_BYTES {
        bail!("configuration exceeds the 4 MiB size limit");
    }
    atomic_write_file(path, payload).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "boundless-host".to_string())
}

fn migrate_config_value(path: &Path, value: &mut serde_json::Value) -> Result<bool> {
    let original = value.clone();
    let Some(object) = value.as_object_mut() else {
        bail!(
            "invalid config: root must be an object in `{}`",
            path.display()
        );
    };
    let config_version = object
        .get("config_version")
        .and_then(serde_json::Value::as_str)
        .context("invalid config: config_version is required")?
        .to_string();
    if !matches!(
        config_version.as_str(),
        "2" | "3" | "4" | "5" | "6" | RUNTIME_CONFIG_VERSION
    ) {
        bail!(
            "unsupported config version `{config_version}`; expected `{RUNTIME_CONFIG_VERSION}`. use a compatible build or restore a pre-upgrade configuration backup for `{}`",
            path.display()
        );
    }

    // Older schemas did not record default-vs-explicit intent. Treat exactly the
    // former product default as migratable once; explicit schema-7 ports survive.
    let migrate_default_ports = config_version != RUNTIME_CONFIG_VERSION;
    if migrate_default_ports {
        object.insert("config_version".into(), RUNTIME_CONFIG_VERSION.into());
        if object
            .get("network_port")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(LEGACY_DEFAULT_NETWORK_PORT))
        {
            object.insert(
                "network_port".into(),
                app_services::desktop::DEFAULT_NETWORK_PORT.into(),
            );
        }
    }
    if let Some(peers) = object
        .get_mut("peers")
        .and_then(serde_json::Value::as_array_mut)
    {
        for peer in peers
            .iter_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            peer.remove("connected");
            peer.remove("last_seen");
            if migrate_default_ports
                && let Some(address) = peer.get_mut("address")
                && let Some(migrated) = address.as_str().and_then(migrate_legacy_peer_address)
            {
                *address = migrated.into();
            }
        }
    }
    for (key, default) in [
        (
            "anti_idle",
            serde_json::to_value(AntiIdleConfig::default())?,
        ),
        (
            "file_transfer",
            serde_json::to_value(FileTransferConfig::default())?,
        ),
        (
            "input_handoff",
            serde_json::to_value(InputHandoffConfig::default())?,
        ),
    ] {
        object.entry(key).or_insert(default);
    }
    let protocol_version = object
        .get("protocol_version")
        .and_then(serde_json::Value::as_str);
    // Schemas 2-4 already used unconditional protocol migration. Later schemas
    // only accept the explicitly known previous protocol versions.
    if matches!(config_version.as_str(), "2" | "3" | "4")
        || protocol_version.is_some_and(|version| MIGRATABLE_PROTOCOL_VERSIONS.contains(&version))
    {
        object.insert(
            "protocol_version".into(),
            PROTOCOL_CURRENT.to_string().into(),
        );
    }
    Ok(*value != original)
}

fn migrate_legacy_peer_address(address: &str) -> Option<String> {
    let (host, port) = address.trim().rsplit_once(':')?;
    if port.parse::<u16>().ok()? != LEGACY_DEFAULT_NETWORK_PORT || host.is_empty() {
        return None;
    }
    // Keep hostnames, IPv4, bracketed IPv6 and interface scope IDs byte-for-byte.
    // An unbracketed IPv6 suffix is not an unambiguous host:port endpoint.
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return None;
    }
    Some(format!(
        "{host}:{}",
        app_services::desktop::DEFAULT_NETWORK_PORT
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        AntiIdleConfig, ApiTransport, FileTransferConfig, InputHandoffConfig,
        MIGRATABLE_PROTOCOL_VERSIONS, RuntimeConfig, default_input_corner_block_px,
        default_pulse_interval_secs, default_recent_activity_window_secs, load_or_create_config_at,
        save_config_at,
    };
    use core_protocol::PROTOCOL_CURRENT;
    use core_transfer::MAX_TRANSFER_BYTES;

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
            error
                .to_string()
                .contains("pre-upgrade configuration backup"),
            "unexpected error: {error:#}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_config_migrates_previous_local_protocol_versions() {
        for previous in MIGRATABLE_PROTOCOL_VERSIONS {
            let root = std::env::temp_dir().join(format!(
                "boundless-config-protocol-migrate-{}-test-{}",
                previous.replace('.', "-"),
                uuid::Uuid::new_v4()
            ));
            let path = root.join("config.json");
            std::fs::create_dir_all(&root).expect("create temp root");

            let seed = RuntimeConfig {
                protocol_version: (*previous).to_string(),
                ..RuntimeConfig::default()
            };
            save_config_at(&path, &seed).expect("seed stale local protocol config");

            let config = load_or_create_config_at(&path).expect("migrate local protocol");
            assert_eq!(config.protocol_version, PROTOCOL_CURRENT.to_string());

            let saved = std::fs::read_to_string(&path).expect("read migrated config");
            assert!(saved.contains(&format!(r#""protocol_version": "{}""#, PROTOCOL_CURRENT)));
            assert!(!saved.contains(&format!(r#""protocol_version": "{previous}""#)));

            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn save_config_at_replaces_existing_config_file() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-replace-existing-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let first = RuntimeConfig {
            device_name: "first-device".to_string(),
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &first).expect("write first config");

        let second = RuntimeConfig {
            machine_id: first.machine_id,
            device_name: "second-device".to_string(),
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &second).expect("replace existing config");

        let saved = load_or_create_config_at(&path).expect("read saved config");
        assert_eq!(saved.device_name, "second-device");

        let leftovers = std::fs::read_dir(&root)
            .expect("read root")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("boundless-tmp")
            })
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "successful config replacement should not leave temp files"
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

    #[test]
    fn anti_idle_defaults_match_balanced_policy() {
        let config = RuntimeConfig::default();

        assert!(config.anti_idle.enabled);
        assert_eq!(
            config.anti_idle.recent_activity_window_secs,
            default_recent_activity_window_secs()
        );
        assert!(!config.anti_idle.allow_on_battery);
        assert!(!config.anti_idle.keep_display_on);
        assert_eq!(
            config.anti_idle.pulse_interval_secs,
            default_pulse_interval_secs()
        );
    }

    #[test]
    fn default_config_uses_visible_boundless_receive_folder() {
        let config = RuntimeConfig::default();

        assert!(config.file_transfer.receive_dir.ends_with("Boundless"));
        assert!(!config.file_transfer.receive_dir.trim().is_empty());
        assert!(!config.file_transfer.organize_by_peer);
        assert!(!config.file_transfer.auto_accept_trusted_peers);
        assert_eq!(config.file_transfer.max_file_bytes, MAX_TRANSFER_BYTES);
    }

    #[test]
    fn input_handoff_defaults_match_predictable_edge_policy() {
        let config = RuntimeConfig::default();

        assert!(config.input_handoff.block_screen_corners);
        assert_eq!(
            config.input_handoff.corner_block_px,
            default_input_corner_block_px()
        );
        assert!(config.input_handoff.relative_mouse);
        assert!(!config.input_handoff.hide_cursor_at_edge);
        assert!(!config.input_handoff.draw_cursor_marker);
    }

    #[test]
    fn current_config_without_file_transfer_gets_default_receive_folder() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-file-transfer-default-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let mut value =
            serde_json::to_value(RuntimeConfig::default()).expect("serialize default config");
        value
            .as_object_mut()
            .expect("config must be object")
            .remove("file_transfer");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&value).expect("serialize"),
        )
        .expect("write seeded config");

        let config = load_or_create_config_at(&path).expect("load config with defaulted transfer");
        assert!(config.file_transfer.receive_dir.ends_with("Boundless"));
        assert!(!config.file_transfer.organize_by_peer);
        assert!(!config.file_transfer.auto_accept_trusted_peers);
        assert_eq!(config.file_transfer.max_file_bytes, MAX_TRANSFER_BYTES);
        assert_eq!(config.input_handoff, InputHandoffConfig::default());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_migrates_v2_config_with_default_anti_idle() {
        let root =
            std::env::temp_dir().join(format!("boundless-config-migrate-{}", uuid::Uuid::new_v4()));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(
            &path,
            format!(
                r#"{{
  "config_version": "2",
  "machine_id": "m1",
  "device_name": "node",
  "api_bind": "127.0.0.1:50051",
  "api_transport": "tcp",
  "api_pipe_name": "boundlessd-api",
  "protocol_version": "{}",
  "layout_matrix": "self",
  "auto_start": true,
  "network_port": 15100,
  "features": {{
    "share_clipboard": true,
    "transfer_file": true,
    "share_input": true,
    "easy_mouse": true,
    "wrap_mouse": true
  }},
  "hotkeys": {{
    "toggle_easy_mouse": "Ctrl+Alt+Shift+E",
    "lock_machine": "Ctrl+Alt+Shift+L",
    "switch_all": "Disabled",
    "reconnect": "Ctrl+Alt+Shift+R"
  }},
  "peers": [],
  "updated_at": "2026-04-13T00:00:00Z"
}}"#,
                PROTOCOL_CURRENT
            ),
        )
        .expect("seed config");

        let config = load_or_create_config_at(&path).expect("migrate config");
        assert_eq!(config.config_version, "7");
        assert_eq!(config.anti_idle, AntiIdleConfig::default());
        assert_eq!(config.input_handoff, InputHandoffConfig::default());

        let saved = std::fs::read_to_string(&path).expect("read migrated");
        assert!(saved.contains("\"anti_idle\""));
        assert!(saved.contains("\"input_handoff\""));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_migrates_v4_config_without_resetting_existing_sections() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-migrate-v4-preserve-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create root");
        let receive_dir = root.join("custom-receive");
        let seed = RuntimeConfig {
            config_version: "4".to_string(),
            anti_idle: AntiIdleConfig {
                enabled: false,
                recent_activity_window_secs: 900,
                allow_on_battery: true,
                keep_display_on: true,
                pulse_interval_secs: 45,
            },
            file_transfer: FileTransferConfig {
                receive_dir: receive_dir.display().to_string(),
                organize_by_peer: true,
                auto_accept_trusted_peers: false,
                max_file_bytes: 123_456,
            },
            ..RuntimeConfig::default()
        };
        let mut value = serde_json::to_value(seed).expect("serialize v4 seed");
        value
            .as_object_mut()
            .expect("config must be object")
            .remove("input_handoff");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&value).expect("serialize"),
        )
        .expect("write seeded config");

        let config = load_or_create_config_at(&path).expect("migrate v4 config");
        assert_eq!(config.config_version, "7");
        assert!(!config.anti_idle.enabled);
        assert_eq!(config.anti_idle.recent_activity_window_secs, 900);
        assert!(config.anti_idle.allow_on_battery);
        assert!(config.anti_idle.keep_display_on);
        assert_eq!(config.anti_idle.pulse_interval_secs, 45);
        assert_eq!(
            config.file_transfer.receive_dir,
            receive_dir.display().to_string()
        );
        assert!(config.file_transfer.organize_by_peer);
        assert!(!config.file_transfer.auto_accept_trusted_peers);
        assert_eq!(config.file_transfer.max_file_bytes, 123_456);
        assert_eq!(config.input_handoff, InputHandoffConfig::default());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_rejects_unbounded_corner_block_size() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-input-handoff-invalid-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let seed = RuntimeConfig {
            input_handoff: InputHandoffConfig {
                corner_block_px: 257,
                ..InputHandoffConfig::default()
            },
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).expect("seed invalid config");

        let error = load_or_create_config_at(&path).expect_err("must reject invalid corner block");
        assert!(
            error
                .to_string()
                .contains("input_handoff.corner_block_px must be <= 256"),
            "unexpected error: {error:#}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_or_create_rejects_zero_file_transfer_limit() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-file-transfer-limit-test-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        std::fs::create_dir_all(&root).expect("create temp root");

        let seed = RuntimeConfig {
            file_transfer: FileTransferConfig {
                max_file_bytes: 0,
                ..FileTransferConfig::default()
            },
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).expect("seed invalid config");

        let error = load_or_create_config_at(&path).expect_err("must reject invalid max size");
        assert!(
            error
                .to_string()
                .contains("file_transfer.max_file_bytes must be greater than zero"),
            "unexpected error: {error:#}"
        );

        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn supported_old_configs_migrate_default_ports_once_with_exact_backup_and_preserved_settings() {
        for version in ["2", "3", "4", "5", "6"] {
            let root = std::env::temp_dir().join(format!(
                "boundless-config-port-migration-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("config.json");
            let mut seed = RuntimeConfig {
                config_version: version.into(),
                network_port: 15100,
                device_name: "Office PC".into(),
                layout_matrix: "self,peer-1".into(),
                ..RuntimeConfig::default()
            };
            seed.anti_idle.enabled = false;
            seed.file_transfer.receive_dir = root.join("keep-my-files").display().to_string();
            seed.input_handoff.corner_block_px = 72;
            seed.features.insert("share_clipboard".into(), false);
            seed.hotkeys
                .insert("reconnect".into(), "Ctrl+Alt+F8".into());
            let mut value = serde_json::to_value(&seed).unwrap();
            value["peers"] = serde_json::json!([
                {"peer_id":"peer-1", "display_name":"Office laptop", "address":"office.local:15100", "connected":true, "last_seen":"2026-01-01T00:00:00Z"},
                {"peer_id":"peer-2", "display_name":"Custom", "address":"10.0.0.7:25100"},
                {"peer_id":"peer-3", "display_name":"IPv6", "address":"[fe80::7%4]:15100"}
            ]);
            let original = format!(
                "  {}\r\n",
                serde_json::to_string_pretty(&value)
                    .unwrap()
                    .replace('\n', "\r\n")
            )
            .into_bytes();
            std::fs::write(&path, &original).unwrap();
            let migrated = load_or_create_config_at(&path).unwrap();
            assert_eq!(migrated.network_port, 16100);
            assert_eq!(migrated.config_version, "7");
            assert_eq!(migrated.machine_id, seed.machine_id);
            assert_eq!(migrated.device_name, seed.device_name);
            assert_eq!(migrated.layout_matrix, seed.layout_matrix);
            assert_eq!(migrated.anti_idle, seed.anti_idle);
            assert_eq!(migrated.file_transfer, seed.file_transfer);
            assert_eq!(migrated.input_handoff, seed.input_handoff);
            assert_eq!(migrated.features, seed.features);
            assert_eq!(migrated.hotkeys, seed.hotkeys);
            assert_eq!(migrated.peers[0].address, "office.local:16100");
            assert_eq!(migrated.peers[0].peer_id, "peer-1");
            assert_eq!(migrated.peers[0].display_name, "Office laptop");
            assert!(!migrated.peers[0].connected);
            assert_eq!(migrated.peers[1].address, "10.0.0.7:25100");
            assert_eq!(migrated.peers[2].address, "[fe80::7%4]:16100");
            let backup = super::migration_backup::backup_path(&path).unwrap();
            assert_eq!(std::fs::read(&backup).unwrap(), original);
            let once = std::fs::read(&path).unwrap();
            load_or_create_config_at(&path).unwrap();
            assert_eq!(
                std::fs::read(&path).unwrap(),
                once,
                "second launch must not rewrite config"
            );
            assert_eq!(std::fs::read(&backup).unwrap(), original);
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                2,
                "no growing set of backups/staging files"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn migration_preserves_custom_listener_ports_and_current_explicit_legacy_ports() {
        for (version, port, peer_port) in [("6", 25100, 27100), ("7", 15100, 15100)] {
            let root = std::env::temp_dir().join(format!(
                "boundless-config-custom-ports-{}",
                uuid::Uuid::new_v4()
            ));
            let path = root.join("config.json");
            let seed = RuntimeConfig {
                config_version: version.into(),
                network_port: port,
                peers: vec![super::PeerConfig {
                    peer_id: "trusted".into(),
                    display_name: "Custom port PC".into(),
                    address: format!("manual.local:{peer_port}"),
                    connected: false,
                    last_seen: chrono::Utc::now(),
                }],
                ..RuntimeConfig::default()
            };
            save_config_at(&path, &seed).unwrap();
            let migrated = load_or_create_config_at(&path).unwrap();
            assert_eq!(migrated.network_port, port);
            assert_eq!(migrated.peers[0].address, seed.peers[0].address);
            assert_eq!(migrated.machine_id, seed.machine_id);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn migration_validation_or_backup_failure_does_not_rewrite_original() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-invalid-migration-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        let seed = RuntimeConfig {
            config_version: "6".into(),
            network_port: 15100,
            layout_matrix: "".into(),
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(load_or_create_config_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let backup = super::migration_backup::backup_path(&path).unwrap();
        assert!(!backup.exists());
        let valid = RuntimeConfig {
            layout_matrix: "self".into(),
            ..seed
        };
        save_config_at(&path, &valid).unwrap();
        std::fs::create_dir(&backup).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(load_or_create_config_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(backup.is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_config_is_rejected_without_backup_or_rewrite() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-oversized-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        let length = super::migration_backup::MAX_CONFIG_BYTES + 1;
        std::fs::File::create(&path)
            .unwrap()
            .set_len(length)
            .unwrap();
        let error = load_or_create_config_at(&path).unwrap_err();
        assert!(error.to_string().contains("4 MiB"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), length);
        assert!(
            !super::migration_backup::backup_path(&path)
                .unwrap()
                .exists()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_complete_migration_backup_is_not_overwritten() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-backup-once-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        let seed = RuntimeConfig {
            config_version: "6".into(),
            network_port: 15100,
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).unwrap();
        let original = std::fs::read(&path).unwrap();
        load_or_create_config_at(&path).unwrap();
        let changed = RuntimeConfig {
            device_name: "renamed before retry".into(),
            ..seed
        };
        save_config_at(&path, &changed).unwrap();
        let loaded = load_or_create_config_at(&path).unwrap();
        assert_eq!(loaded.device_name, changed.device_name);
        assert_eq!(
            std::fs::read(super::migration_backup::backup_path(&path).unwrap()).unwrap(),
            original
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn interrupted_backup_is_preserved_and_retries_do_not_accumulate_copies() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-backup-interrupted-{}",
            uuid::Uuid::new_v4()
        ));
        let path = root.join("config.json");
        let seed = RuntimeConfig {
            config_version: "6".into(),
            network_port: 15100,
            ..RuntimeConfig::default()
        };
        save_config_at(&path, &seed).unwrap();
        let original = std::fs::read(&path).unwrap();
        let pending = root.join("config.json.pre-v7.bak.pending");
        std::fs::write(&pending, b"interrupted backup").unwrap();
        for _ in 0..3 {
            assert!(load_or_create_config_at(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert_eq!(std::fs::read(&pending).unwrap(), b"interrupted backup");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        }
        std::fs::remove_file(&pending).unwrap();
        assert_eq!(load_or_create_config_at(&path).unwrap().network_port, 16100);
        assert_eq!(
            std::fs::read(super::migration_backup::backup_path(&path).unwrap()).unwrap(),
            original
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
