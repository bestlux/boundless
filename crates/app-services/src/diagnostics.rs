use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::queries::{ConsoleSnapshot, TransportEventSnapshot};

const REDACTED_SECRET: &str = "[redacted-secret]";
const REDACTED_ID: &str = "[redacted-id]";
const REDACTED_CLIPBOARD_TEXT: &str = "[redacted-clipboard-text]";
const REDACTED_FILE_NAME: &str = "[redacted-file-name]";
const REDACTED_PATH: &str = "[redacted-path]";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticExportOptions {
    pub output_path: Option<String>,
    pub include_filenames: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticExportResult {
    pub bundle_path: String,
    pub manifest_path: String,
    pub filenames_included: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDiagnosticSnapshot {
    pub platform: String,
    pub service_name: String,
    pub installed: bool,
    pub state: String,
    pub process_id: Option<u32>,
    pub binary_path: Option<String>,
    pub service_version: String,
    pub service_version_source: String,
    pub current_version: String,
    pub version_parity: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RedactionContext {
    identifiers: BTreeMap<String, String>,
}

impl RedactionContext {
    pub fn pseudonymize_identifier(&mut self, value: &str) -> String {
        if value.is_empty() || value == "none" || value == "all" {
            return value.to_string();
        }

        if let Some(existing) = self.identifiers.get(value) {
            return existing.clone();
        }

        let alias = format!("peer-{}", self.identifiers.len() + 1);
        self.identifiers.insert(value.to_string(), alias.clone());
        alias
    }
}

pub fn build_online_bundle(
    snapshot: ConsoleSnapshot,
    service: ServiceDiagnosticSnapshot,
    include_filenames: bool,
) -> Value {
    let mut redaction = RedactionContext::default();
    let recent_events = snapshot
        .transport_events
        .iter()
        .rev()
        .take(50)
        .map(|event| redact_transport_event(event, include_filenames, &mut redaction))
        .collect::<Vec<_>>();
    let recent_transfer_states = snapshot
        .transport_events
        .iter()
        .rev()
        .filter(|event| {
            event.kind.contains("transfer")
                || event.kind.contains("clipboard")
                || event.kind == "file"
        })
        .take(25)
        .map(|event| redact_transport_event(event, include_filenames, &mut redaction))
        .collect::<Vec<_>>();

    json!({
        "schema_version": 1,
        "generated_at": Utc::now().to_rfc3339(),
        "privacy": privacy_section(include_filenames),
        "runtime": {
            "mode": "online",
            "daemon_online": true,
            "daemon_version": snapshot.status.daemon_version,
            "protocol_version": snapshot.status.protocol_version,
            "api_transport": snapshot.status.api_transport,
            "api_bind": snapshot.status.api_bind,
            "api_pipe_name": snapshot.status.api_pipe_name,
        },
        "service": redact_service(service, include_filenames),
        "component_health": {
            "peer_count": snapshot.status.peer_count,
            "mdns_active": snapshot.mdns_active,
            "input_locked": snapshot.status.input_locked,
            "input_lock_supported": snapshot.status.input_lock_supported,
            "anti_idle": snapshot.anti_idle_status,
            "input_runtime": {
                "owner_peer_id": snapshot.input_runtime.owner_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
                "configured_capture_target_peer_id": snapshot.input_runtime.configured_capture_target_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
                "active_capture_target_peer_id": snapshot.input_runtime.active_capture_target_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
                "lock_active": snapshot.input_runtime.lock_active,
                "lock_supported": snapshot.input_runtime.lock_supported,
                "capture_backend_mode": snapshot.input_runtime.capture_backend_mode,
                "pending_inject_frames": snapshot.input_runtime.pending_inject_frames,
                "pending_inject_high_water": snapshot.input_runtime.pending_inject_high_water,
            },
        },
        "configuration": {
            "layout_matrix": snapshot.layout_matrix,
            "features": snapshot.features,
            "file_transfer": {
                "receive_dir": redact_path(&snapshot.file_transfer_config.receive_dir, false),
                "organize_by_peer": snapshot.file_transfer_config.organize_by_peer,
                "auto_accept_trusted_peers": snapshot.file_transfer_config.auto_accept_trusted_peers,
                "max_file_bytes": snapshot.file_transfer_config.max_file_bytes,
            },
            "input_handoff": snapshot.input_handoff_config,
        },
        "peers": snapshot.peers.into_iter().map(|peer| {
            let peer_alias = redaction.pseudonymize_identifier(&peer.peer_id);
            json!({
                "peer_id": peer_alias,
                "display_name": "[redacted-peer-name]",
                "address": "[redacted-endpoint]",
                "connected": peer.connected,
                "health_state": peer.health_state,
                "health_reason": peer.health_reason,
            })
        }).collect::<Vec<_>>(),
        "recent_transfer_states": recent_transfer_states,
        "recent_events": recent_events,
        "offline_notes": [],
    })
}

pub fn build_offline_bundle(
    current_version: &str,
    endpoint: &str,
    service: ServiceDiagnosticSnapshot,
    include_filenames: bool,
    reason: &str,
) -> Value {
    json!({
        "schema_version": 1,
        "generated_at": Utc::now().to_rfc3339(),
        "privacy": privacy_section(include_filenames),
        "runtime": {
            "mode": "offline",
            "daemon_online": false,
            "cli_version": current_version,
            "control_endpoint": endpoint,
        },
        "service": redact_service(service, include_filenames),
        "component_health": {
            "daemon": "unavailable",
            "reason": reason,
        },
        "recent_transfer_states": [],
        "recent_events": [],
        "offline_notes": [
            "Daemon IPC was not used, so in-memory peer health and transfer event history are unavailable."
        ],
    })
}

pub async fn write_diagnostic_bundle(
    bundle: Value,
    options: DiagnosticExportOptions,
) -> Result<DiagnosticExportResult> {
    let target = options
        .output_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(default_diagnostics_dir);
    tokio::fs::create_dir_all(&target)
        .await
        .with_context(|| format!("create diagnostics directory {}", target.display()))?;

    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let file_path = target.join(format!("bundle-{stamp}.json"));
    let manifest_path = target.join(format!("bundle-{stamp}.redaction.txt"));
    let redacted = redact_sensitive_json(bundle, options.include_filenames);
    let bundle_json = serde_json::to_string_pretty(&redacted).context("serialize diagnostics")?;
    tokio::fs::write(&file_path, bundle_json)
        .await
        .with_context(|| format!("write diagnostic bundle {}", file_path.display()))?;
    tokio::fs::write(
        &manifest_path,
        redaction_manifest(options.include_filenames),
    )
    .await
    .with_context(|| format!("write redaction manifest {}", manifest_path.display()))?;

    Ok(DiagnosticExportResult {
        bundle_path: file_path.display().to_string(),
        manifest_path: manifest_path.display().to_string(),
        filenames_included: options.include_filenames,
    })
}

pub fn redact_sensitive_json(mut value: Value, include_filenames: bool) -> Value {
    let mut context = RedactionContext::default();
    redact_json_value(&mut value, include_filenames, &mut context, None);
    value
}

pub fn redact_transport_event(
    event: &TransportEventSnapshot,
    include_filenames: bool,
    context: &mut RedactionContext,
) -> Value {
    json!({
        "timestamp": event.timestamp,
        "direction": event.direction,
        "kind": event.kind,
        "peer_id": context.pseudonymize_identifier(&event.peer_id),
        "detail": redact_event_detail(&event.kind, &event.detail, include_filenames),
        "size_bytes": event.size_bytes,
    })
}

pub fn redact_event_detail(kind: &str, detail: &str, include_filenames: bool) -> String {
    if kind == "clipboard_text" {
        return REDACTED_CLIPBOARD_TEXT.to_string();
    }

    let mut redacted = detail
        .split_whitespace()
        .map(|token| redact_detail_token(kind, token, include_filenames))
        .collect::<Vec<_>>()
        .join(" ");

    if redacted == detail && (kind == "file" || kind.contains("file_transfer")) {
        redacted = redact_filename(detail, include_filenames);
    }

    redacted
}

pub fn service_version_parity(
    service_version: Option<&str>,
    expected_version: &str,
) -> &'static str {
    let Some(service_version) = service_version else {
        return "unknown";
    };
    let normalized_service = service_version.trim().trim_start_matches('v');
    let normalized_expected = expected_version.trim().trim_start_matches('v');
    if normalized_service == normalized_expected {
        "matched"
    } else {
        "mismatched"
    }
}

pub fn service_binary_manifest_version(binary_path: &Path) -> (String, &'static str) {
    let Some(parent) = binary_path.parent() else {
        return ("unknown".to_string(), "missing_binary_parent");
    };
    let manifest_path = parent.join("package-manifest.json");
    if !manifest_path.is_file() {
        return ("unknown".to_string(), "missing_package_manifest");
    }

    let version = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|manifest| {
            manifest
                .get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        });

    match version {
        Some(version) if !version.trim().is_empty() => (version, "package_manifest"),
        _ => ("unknown".to_string(), "invalid_package_manifest"),
    }
}

pub fn extract_service_executable_path(raw_binary_path: &str) -> PathBuf {
    let trimmed = raw_binary_path.trim();
    if let Some(rest) = trimmed.strip_prefix('"')
        && let Some(end_quote) = rest.find('"')
    {
        return PathBuf::from(&rest[..end_quote]);
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(index) = lower.find(".exe") {
        return PathBuf::from(&trimmed[..index + 4]);
    }

    PathBuf::from(trimmed)
}

fn redact_json_value(
    value: &mut Value,
    include_filenames: bool,
    context: &mut RedactionContext,
    key_hint: Option<&str>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if is_secret_key(&lower) {
                    *child = Value::String(REDACTED_SECRET.to_string());
                    continue;
                }
                if is_identifier_key(&lower) {
                    if let Some(identifier) = child.as_str() {
                        *child = Value::String(context.pseudonymize_identifier(identifier));
                    } else {
                        redact_json_value(child, include_filenames, context, Some(key));
                    }
                    continue;
                }
                if is_path_key(&lower) {
                    if let Some(path) = child.as_str() {
                        *child = Value::String(redact_path(path, include_filenames));
                    } else {
                        redact_json_value(child, include_filenames, context, Some(key));
                    }
                    continue;
                }
                if is_endpoint_key(&lower) {
                    if child.as_str().is_some() {
                        *child = Value::String("[redacted-endpoint]".to_string());
                    } else {
                        redact_json_value(child, include_filenames, context, Some(key));
                    }
                    continue;
                }

                redact_json_value(child, include_filenames, context, Some(key));
            }
        }
        Value::Array(items) => {
            for child in items {
                redact_json_value(child, include_filenames, context, key_hint);
            }
        }
        Value::String(text) => {
            if key_hint
                .map(|key| key.eq_ignore_ascii_case("detail"))
                .unwrap_or(false)
                && looks_like_clipboard_secret(text)
            {
                *text = REDACTED_CLIPBOARD_TEXT.to_string();
            }
        }
        _ => {}
    }
}

fn redact_service(service: ServiceDiagnosticSnapshot, include_filenames: bool) -> Value {
    json!({
        "platform": service.platform,
        "service_name": service.service_name,
        "installed": service.installed,
        "state": service.state,
        "process_id": service.process_id,
        "binary_path": service.binary_path.map(|path| redact_path(&path, include_filenames)),
        "service_version": service.service_version,
        "service_version_source": service.service_version_source,
        "current_version": service.current_version,
        "version_parity": service.version_parity,
        "error": service.error,
    })
}

fn privacy_section(include_filenames: bool) -> Value {
    json!({
        "default_redaction": true,
        "filenames_included": include_filenames,
        "redacted": [
            "clipboard_plaintext",
            "private_keys",
            "trust_secrets",
            "cert_key_material",
            "tokens",
            "auth_material",
            "peer_ids",
            "machine_ids",
            "request_ids",
            "transfer_ids",
            "local_paths"
        ],
        "filename_policy": if include_filenames {
            "explicit_opt_in_basename_only"
        } else {
            "redacted"
        },
    })
}

fn redaction_manifest(include_filenames: bool) -> String {
    let filename_policy = if include_filenames {
        "explicit_opt_in_basename_only"
    } else {
        "redacted"
    };
    format!(
        "default_redaction=true\nfilenames_included={include_filenames}\nredacted=clipboard_plaintext,private_keys,trust_secrets,cert_key_material,tokens,auth_material,peer_ids,machine_ids,request_ids,transfer_ids,local_paths\nfilename_policy={filename_policy}\n"
    )
}

fn default_diagnostics_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("diagnostics")
}

fn is_secret_key(key: &str) -> bool {
    key.contains("secret")
        || key.contains("token")
        || key.contains("auth")
        || key.contains("private")
        || key.contains("cert")
        || key.contains("fingerprint")
        || key.contains("ca_cert_pem")
        || key.contains("key_material")
}

fn is_identifier_key(key: &str) -> bool {
    key == "peer_id"
        || key == "machine_id"
        || key == "request_id"
        || key == "transfer_id"
        || key.ends_with("_peer_id")
        || key.ends_with("_machine_id")
        || key.ends_with("_request_id")
        || key.ends_with("_transfer_id")
}

fn is_path_key(key: &str) -> bool {
    key.contains("path") || key.ends_with("_dir") || key.contains("receive_dir")
}

fn is_endpoint_key(key: &str) -> bool {
    key == "api_bind"
        || key == "control_endpoint"
        || key == "endpoint"
        || key.ends_with("_endpoint")
        || key == "address"
}

fn redact_detail_token(kind: &str, token: &str, include_filenames: bool) -> String {
    let Some((key, value)) = token.split_once('=') else {
        return if kind == "file" || kind.contains("file_transfer") {
            redact_filename(token, include_filenames)
        } else {
            token.to_string()
        };
    };

    match key {
        "file_name" => format!("{key}={}", redact_filename(value, include_filenames)),
        "transfer_id" | "request_id" => format!("{key}={REDACTED_ID}"),
        key if key.contains("path") => format!("{key}={}", redact_path(value, include_filenames)),
        _ => token.to_string(),
    }
}

fn redact_filename(value: &str, include_filenames: bool) -> String {
    if include_filenames {
        basename(value).unwrap_or(REDACTED_FILE_NAME).to_string()
    } else {
        REDACTED_FILE_NAME.to_string()
    }
}

fn redact_path(value: &str, include_filename: bool) -> String {
    if include_filename && let Some(file_name) = basename(value) {
        return format!("{REDACTED_PATH}/{file_name}");
    }
    REDACTED_PATH.to_string()
}

fn basename(value: &str) -> Option<&str> {
    value
        .rsplit(['/', '\\'])
        .next()
        .filter(|part| !part.trim().is_empty())
}

fn looks_like_clipboard_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("private key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_clipboard_secret_event_detail() {
        let mut context = RedactionContext::default();
        let event = TransportEventSnapshot {
            timestamp: "2026-06-20T00:00:00Z".to_string(),
            direction: "outgoing".to_string(),
            kind: "clipboard_text".to_string(),
            peer_id: "peer-machine-secret".to_string(),
            detail: "password=hunter2 token=abc123".to_string(),
            size_bytes: 29,
        };

        let redacted = redact_transport_event(&event, false, &mut context);
        let rendered = serde_json::to_string(&redacted).expect("serialize redacted event");

        assert!(rendered.contains(REDACTED_CLIPBOARD_TEXT));
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("abc123"));
        assert!(!rendered.contains("peer-machine-secret"));
    }

    #[test]
    fn redacts_trust_private_key_and_auth_material() {
        let input = json!({
            "ca_cert_pem": "-----BEGIN CERTIFICATE-----",
            "private_key": "-----BEGIN PRIVATE KEY-----",
            "trust_secret": "trust-me",
            "auth_token": "bearer-token",
            "safe_count": 4
        });

        let redacted = redact_sensitive_json(input, false);
        let rendered = serde_json::to_string(&redacted).expect("serialize redacted json");

        assert_eq!(redacted["safe_count"], json!(4));
        assert!(!rendered.contains("BEGIN CERTIFICATE"));
        assert!(!rendered.contains("BEGIN PRIVATE KEY"));
        assert!(!rendered.contains("trust-me"));
        assert!(!rendered.contains("bearer-token"));
        assert_eq!(redacted["private_key"], json!(REDACTED_SECRET));
    }

    #[test]
    fn pseudonymizes_peer_ids_consistently() {
        let input = json!({
            "peer_id": "machine-alpha",
            "active_capture_target_peer_id": "machine-alpha",
            "peers": [
                {"peer_id": "machine-alpha"},
                {"peer_id": "machine-beta"}
            ]
        });

        let redacted = redact_sensitive_json(input, false);
        let rendered = serde_json::to_string(&redacted).expect("serialize redacted json");

        assert!(!rendered.contains("machine-alpha"));
        assert!(!rendered.contains("machine-beta"));
        assert_eq!(
            redacted["peer_id"],
            redacted["active_capture_target_peer_id"]
        );
        assert_eq!(redacted["peers"][0]["peer_id"], redacted["peer_id"]);
        assert_ne!(redacted["peers"][1]["peer_id"], redacted["peer_id"]);
    }

    #[test]
    fn filename_inclusion_is_explicit_and_basename_only() {
        let hidden = redact_event_detail(
            "file_transfer_started",
            "transfer_id=abc file_name=C:\\Users\\A\\taxes.pdf total_bytes=9",
            false,
        );
        assert!(hidden.contains("file_name=[redacted-file-name]"));
        assert!(!hidden.contains("taxes.pdf"));

        let included = redact_event_detail(
            "file_transfer_started",
            "transfer_id=abc file_name=C:\\Users\\A\\taxes.pdf total_bytes=9",
            true,
        );
        assert!(included.contains("file_name=taxes.pdf"));
        assert!(!included.contains("Users"));
        assert!(included.contains("transfer_id=[redacted-id]"));
    }

    #[test]
    fn service_version_parity_uses_exact_stable_versions() {
        assert_eq!(service_version_parity(Some("v5.0.0"), "5.0.0"), "matched");
        assert_eq!(
            service_version_parity(Some("15.0.0"), "5.0.0"),
            "mismatched"
        );
        assert_eq!(service_version_parity(None, "5.0.0"), "unknown");
    }
}
