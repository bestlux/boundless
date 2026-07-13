use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use chrono::Utc;
use core_clipboard::sanitize_clipboard_event_output_detail;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::queries::{ConsoleSnapshot, InputRuntimeSnapshot, TransportEventSnapshot};

const REDACTED_SECRET: &str = "[redacted-secret]";
const REDACTED_ID: &str = "[redacted-id]";
const REDACTED_FILE_NAME: &str = "[redacted-file-name]";
const REDACTED_PATH: &str = "[redacted-path]";
const BOUNDLESS_RELATED_TCP_PORTS: &[u16] = &[15100, 15101, 15200];

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortListenerDiagnosticSnapshot {
    pub platform: String,
    pub ports: Vec<u16>,
    pub read_only: bool,
    pub listeners: Vec<PortListenerDiagnostic>,
    pub summary: Vec<String>,
    pub mitigation: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortListenerDiagnostic {
    pub protocol: String,
    pub address_family: String,
    pub bind_scope: String,
    pub port: u16,
    pub process_id: Option<u32>,
    pub process_name: Option<String>,
    pub process_path: Option<String>,
    pub owner_kind: String,
    pub suggested_mitigation: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPortListenerRow {
    #[serde(rename = "LocalAddress", default)]
    local_address: String,
    #[serde(rename = "LocalPort", default)]
    local_port: u16,
    #[serde(rename = "OwningProcess", default)]
    owning_process: Option<u32>,
    #[serde(rename = "ProcessName", default)]
    process_name: Option<String>,
    #[serde(rename = "ProcessPath", default)]
    process_path: Option<String>,
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

    let layout_matrix = sanitize_layout_matrix(
        &snapshot.layout_matrix,
        &snapshot.status.machine_id,
        &snapshot.local_display_name,
        &snapshot.peers,
        &mut redaction,
    );

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
        "port_listeners": port_listener_diagnostics_snapshot(),
        "component_health": {
            "peer_count": snapshot.status.peer_count,
            "mdns_active": snapshot.mdns_active,
            "input_locked": snapshot.status.input_locked,
            "input_lock_supported": snapshot.status.input_lock_supported,
            "anti_idle": snapshot.anti_idle_status,
            "input_runtime": input_runtime_diagnostics(snapshot.input_runtime, &mut redaction),
            "clipboard_runtime": {
                "backend_mode": snapshot.clipboard_runtime.backend_mode,
            },
        },
        "configuration": {
            "layout_matrix": layout_matrix,
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

fn input_runtime_diagnostics(
    snapshot: InputRuntimeSnapshot,
    redaction: &mut RedactionContext,
) -> Value {
    json!({
        "owner_peer_id": snapshot.owner_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
        "configured_capture_target_peer_id": snapshot.configured_capture_target_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
        "active_capture_target_peer_id": snapshot.active_capture_target_peer_id.map(|id| redaction.pseudonymize_identifier(&id)),
        "lock_active": snapshot.lock_active,
        "lock_supported": snapshot.lock_supported,
        "capture_backend_mode": snapshot.capture_backend_mode,
        "pending_inject_frames": snapshot.pending_inject_frames,
        "pending_inject_high_water": snapshot.pending_inject_high_water,
        "elevated_injector_state": snapshot.elevated_injector_state,
        "elevated_injector_reason": snapshot.elevated_injector_reason,
        "elevated_injector_signature_trust": snapshot.elevated_injector_signature_trust,
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
        "port_listeners": port_listener_diagnostics_snapshot(),
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

pub fn port_listener_diagnostics_snapshot() -> PortListenerDiagnosticSnapshot {
    let ports = BOUNDLESS_RELATED_TCP_PORTS.to_vec();
    if !cfg!(windows) {
        return PortListenerDiagnosticSnapshot {
            platform: std::env::consts::OS.to_string(),
            ports,
            read_only: true,
            listeners: Vec::new(),
            summary: vec![
                "Local TCP listener ownership diagnostics are only available on Windows."
                    .to_string(),
            ],
            mitigation: Vec::new(),
            error: None,
        };
    }

    match collect_windows_port_listener_rows() {
        Ok(rows) => build_port_listener_diagnostics(rows),
        Err(error) => PortListenerDiagnosticSnapshot {
            platform: "windows".to_string(),
            ports,
            read_only: true,
            listeners: Vec::new(),
            summary: vec![
                "Could not collect local TCP listener ownership diagnostics.".to_string(),
            ],
            mitigation: vec![
                "Run the packaged Boundless-ConnectivityDiagnostics.ps1 helper locally for a manual read-only check.".to_string(),
            ],
            error: Some(error.to_string()),
        },
    }
}

fn collect_windows_port_listener_rows() -> Result<Vec<RawPortListenerRow>> {
    let ports = BOUNDLESS_RELATED_TCP_PORTS
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$ports = @({ports})
$rows = @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object {{ $ports -contains [int]$_.LocalPort }} |
    ForEach-Object {{
        $proc = Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue
        [pscustomobject]@{{
            LocalAddress = [string]$_.LocalAddress
            LocalPort = [int]$_.LocalPort
            OwningProcess = if ($null -ne $_.OwningProcess) {{ [int]$_.OwningProcess }} else {{ $null }}
            ProcessName = if ($null -ne $proc) {{ [string]$proc.ProcessName }} else {{ $null }}
            ProcessPath = if ($null -ne $proc) {{ try {{ [string]$proc.Path }} catch {{ $null }} }} else {{ $null }}
        }}
    }})
$rows | ConvertTo-Json -Depth 4 -Compress
"#
    );
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .output()
        .context("run read-only Windows TCP listener ownership query")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "Windows TCP listener ownership query failed with status {}: {}",
            output.status,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_port_listener_rows(&stdout).context("parse Windows TCP listener ownership query")
}

fn parse_port_listener_rows(stdout: &str) -> Result<Vec<RawPortListenerRow>> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    match serde_json::from_str::<Vec<RawPortListenerRow>>(trimmed) {
        Ok(rows) => Ok(rows),
        Err(array_error) => serde_json::from_str::<RawPortListenerRow>(trimmed)
            .map(|row| vec![row])
            .with_context(|| format!("parse listener rows as array or object: {array_error}")),
    }
}

fn build_port_listener_diagnostics(
    rows: Vec<RawPortListenerRow>,
) -> PortListenerDiagnosticSnapshot {
    let mut listeners = rows
        .into_iter()
        .filter(|row| BOUNDLESS_RELATED_TCP_PORTS.contains(&row.local_port))
        .map(port_listener_from_row)
        .collect::<Vec<_>>();
    listeners.sort_by(|left, right| {
        (
            left.port,
            left.address_family.as_str(),
            left.bind_scope.as_str(),
            left.process_id,
            left.process_name.as_deref().unwrap_or_default(),
        )
            .cmp(&(
                right.port,
                right.address_family.as_str(),
                right.bind_scope.as_str(),
                right.process_id,
                right.process_name.as_deref().unwrap_or_default(),
            ))
    });

    let summary = port_listener_summary(&listeners);
    let mitigation = port_listener_mitigation_summary(&listeners);
    PortListenerDiagnosticSnapshot {
        platform: "windows".to_string(),
        ports: BOUNDLESS_RELATED_TCP_PORTS.to_vec(),
        read_only: true,
        listeners,
        summary,
        mitigation,
        error: None,
    }
}

fn port_listener_from_row(row: RawPortListenerRow) -> PortListenerDiagnostic {
    let owner_kind =
        classify_listener_owner(row.process_name.as_deref(), row.process_path.as_deref());
    PortListenerDiagnostic {
        protocol: "tcp".to_string(),
        address_family: listener_address_family(&row.local_address).to_string(),
        bind_scope: listener_bind_scope(&row.local_address).to_string(),
        port: row.local_port,
        process_id: row.owning_process,
        process_name: clean_optional_string(row.process_name),
        process_path: clean_optional_string(row.process_path),
        suggested_mitigation: suggested_listener_mitigation(owner_kind, row.local_port),
        owner_kind: owner_kind.to_string(),
    }
}

fn clean_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn listener_address_family(address: &str) -> &'static str {
    if address.contains(':') {
        "ipv6"
    } else {
        "ipv4"
    }
}

fn listener_bind_scope(address: &str) -> &'static str {
    match address {
        "0.0.0.0" | "::" | "[::]" => "any",
        "127.0.0.1" | "::1" | "[::1]" => "loopback",
        "" => "unknown",
        _ => "specific",
    }
}

fn classify_listener_owner(process_name: Option<&str>, process_path: Option<&str>) -> &'static str {
    let process_name = process_name.unwrap_or_default().to_ascii_lowercase();
    let process_path = process_path.unwrap_or_default().to_ascii_lowercase();
    let combined = format!("{process_name} {process_path}");

    if combined.contains("mousewithoutborders")
        || combined.contains("mouse without borders")
        || combined.contains("powertoys.mousewithoutborders")
    {
        return "mouse-without-borders";
    }

    if process_name == "boundlessd"
        || process_name == "boundless-service"
        || process_name == "boundless"
        || combined.contains("boundless-service.exe")
        || combined.contains("boundlessd.exe")
    {
        return "boundless";
    }

    if process_name.is_empty() && process_path.is_empty() {
        return "unknown";
    }

    "other"
}

fn suggested_listener_mitigation(owner_kind: &str, port: u16) -> String {
    match owner_kind {
        "boundless" => format!(
            "TCP {port} is owned by Boundless; this is expected when the daemon is running."
        ),
        "mouse-without-borders" => format!(
            "Mouse Without Borders or PowerToys is listening on TCP {port}; stop MWB during Boundless dogfood or move Boundless to an alternate network_port before pairing."
        ),
        "other" => format!(
            "Another local process is listening on TCP {port}; identify the owner, stop it if appropriate, or move Boundless to an alternate network_port for side-by-side testing."
        ),
        _ => format!(
            "TCP {port} has a listener but the owning process could not be resolved; rerun diagnostics locally or inspect the port owner before changing trust or firewall state."
        ),
    }
}

fn port_listener_summary(listeners: &[PortListenerDiagnostic]) -> Vec<String> {
    if listeners.is_empty() {
        return vec![
            "No local listeners were found on Boundless-related TCP ports 15100, 15101, or 15200."
                .to_string(),
        ];
    }

    let mut summary = Vec::new();
    for port in BOUNDLESS_RELATED_TCP_PORTS {
        let owners = listeners
            .iter()
            .filter(|listener| listener.port == *port)
            .map(|listener| listener.owner_kind.as_str())
            .collect::<Vec<_>>();
        if owners.is_empty() {
            continue;
        }
        if owners.contains(&"boundless") && owners.contains(&"mouse-without-borders") {
            summary.push(format!(
                "Boundless and Mouse Without Borders/PowerToys both appear on TCP {port}; side-by-side listener ownership can confuse reachability diagnosis."
            ));
        } else if owners.contains(&"mouse-without-borders") {
            summary.push(format!(
                "Mouse Without Borders/PowerToys is listening on Boundless-related TCP {port}."
            ));
        } else if owners.contains(&"other") {
            summary.push(format!(
                "A non-Boundless process is listening on Boundless-related TCP {port}."
            ));
        } else if owners.contains(&"unknown") {
            summary.push(format!(
                "TCP {port} has a listener whose owning process could not be resolved."
            ));
        }
    }

    if summary.is_empty() {
        summary.push(
            "Only Boundless-owned listeners were found on Boundless-related TCP ports.".to_string(),
        );
    }
    summary
}

fn port_listener_mitigation_summary(listeners: &[PortListenerDiagnostic]) -> Vec<String> {
    let mut mitigation = Vec::new();
    if listeners
        .iter()
        .any(|listener| listener.owner_kind == "mouse-without-borders")
    {
        mitigation.push("For side-by-side dogfood, stop Mouse Without Borders/PowerToys before pairing or configure Boundless with an alternate network_port on every participating machine.".to_string());
    }
    if listeners
        .iter()
        .any(|listener| matches!(listener.owner_kind.as_str(), "other" | "unknown"))
    {
        mitigation.push("Resolve local port ownership before resetting trust; a local port collision can look like a remote firewall or VLAN failure.".to_string());
    }
    if !mitigation.is_empty() {
        mitigation.push(
            "Do not create firewall rules or elevate diagnostics automatically for this check."
                .to_string(),
        );
    }
    mitigation
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
    let peer_id = context.pseudonymize_identifier(&event.peer_id);
    let detail =
        redact_event_detail_with_context(&event.kind, &event.detail, include_filenames, context);
    json!({
        "timestamp": event.timestamp,
        "direction": event.direction,
        "kind": event.kind,
        "peer_id": peer_id,
        "detail": detail,
        "size_bytes": event.size_bytes,
    })
}

pub fn redact_event_detail(kind: &str, detail: &str, include_filenames: bool) -> String {
    redact_event_detail_with_context(
        kind,
        detail,
        include_filenames,
        &mut RedactionContext::default(),
    )
}

fn redact_event_detail_with_context(
    kind: &str,
    detail: &str,
    include_filenames: bool,
    context: &mut RedactionContext,
) -> String {
    if kind.starts_with("clipboard") {
        return sanitize_clipboard_event_output_detail(kind, detail);
    }

    let mut redacted = detail
        .split_whitespace()
        .map(|token| redact_detail_token(kind, token, include_filenames, context))
        .collect::<Vec<_>>()
        .join(" ");

    if redacted == detail && (kind == "file" || kind.contains("file_transfer")) {
        redacted = redact_filename(detail, include_filenames);
    }

    redacted
}

pub fn sanitize_layout_matrix(
    layout_matrix: &str,
    local_machine_id: &str,
    local_display_name: &str,
    peers: &[crate::queries::UiPairedPeer],
    context: &mut RedactionContext,
) -> String {
    layout_matrix
        .split(';')
        .map(|row| {
            row.split(',')
                .map(|token| {
                    sanitize_layout_token(
                        token.trim(),
                        local_machine_id,
                        local_display_name,
                        peers,
                        context,
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join(";")
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
        .and_then(|contents| parse_package_manifest_version(&contents));

    match version {
        Some(version) if !version.trim().is_empty() => (version, "package_manifest"),
        _ => ("unknown".to_string(), "invalid_package_manifest"),
    }
}

fn parse_package_manifest_version(contents: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(contents.trim_start_matches('\u{feff}'))
        .ok()
        .and_then(|manifest| {
            manifest
                .get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
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
        Value::String(text)
            if key_hint
                .map(|key| key.eq_ignore_ascii_case("detail"))
                .unwrap_or(false)
                && looks_like_clipboard_secret(text) =>
        {
            *text = "metadata_only=true".to_string();
        }
        Value::String(_) => {}
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
            "raw_endpoints",
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
        "default_redaction=true\nfilenames_included={include_filenames}\nredacted=clipboard_plaintext,private_keys,trust_secrets,cert_key_material,tokens,auth_material,peer_ids,machine_ids,request_ids,transfer_ids,raw_endpoints,local_paths\nfilename_policy={filename_policy}\n"
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

fn sanitize_layout_token(
    token: &str,
    local_machine_id: &str,
    local_display_name: &str,
    peers: &[crate::queries::UiPairedPeer],
    context: &mut RedactionContext,
) -> String {
    if token.is_empty() {
        return String::new();
    }

    let token_lower = token.to_ascii_lowercase();
    if matches!(token_lower.as_str(), "self" | "local" | "this" | "me")
        || token.eq_ignore_ascii_case(local_machine_id)
        || token.eq_ignore_ascii_case(local_display_name)
    {
        return "self".to_string();
    }

    if let Some(peer_id) = resolve_layout_peer_token(token, peers) {
        return context.pseudonymize_identifier(peer_id);
    }

    "[redacted-layout-token]".to_string()
}

fn resolve_layout_peer_token<'a>(
    token: &str,
    peers: &'a [crate::queries::UiPairedPeer],
) -> Option<&'a str> {
    let token_lower = token.to_ascii_lowercase();
    let mut matched_peer_ids = Vec::<&str>::new();

    for peer in peers {
        let peer_id_match = peer.peer_id.eq_ignore_ascii_case(token);
        let display_name_match = peer.display_name.eq_ignore_ascii_case(token);
        let peer_id_prefix_match = peer.peer_id.to_ascii_lowercase().starts_with(&token_lower);
        if !(peer_id_match || display_name_match || peer_id_prefix_match) {
            continue;
        }

        if !matched_peer_ids
            .iter()
            .any(|peer_id| *peer_id == peer.peer_id)
        {
            matched_peer_ids.push(peer.peer_id.as_str());
        }
    }

    if matched_peer_ids.len() == 1 {
        matched_peer_ids.pop()
    } else {
        None
    }
}

fn redact_detail_token(
    kind: &str,
    token: &str,
    include_filenames: bool,
    context: &mut RedactionContext,
) -> String {
    let Some((key, value)) = token.split_once('=') else {
        return if kind == "file" || kind.contains("file_transfer") {
            redact_filename(token, include_filenames)
        } else {
            token.to_string()
        };
    };

    match key {
        "file_name" => format!("{key}={}", redact_filename(value, include_filenames)),
        key if is_transfer_or_request_key(&key.to_ascii_lowercase()) => {
            format!("{key}={REDACTED_ID}")
        }
        key if is_peer_or_machine_key(&key.to_ascii_lowercase()) => {
            format!("{key}={}", context.pseudonymize_identifier(value))
        }
        key if key.contains("path") => format!("{key}={}", redact_path(value, include_filenames)),
        _ => token.to_string(),
    }
}

fn is_transfer_or_request_key(key: &str) -> bool {
    key == "request_id"
        || key == "transfer_id"
        || key.ends_with("_request_id")
        || key.ends_with("_transfer_id")
}

fn is_peer_or_machine_key(key: &str) -> bool {
    key == "peer_id"
        || key == "machine_id"
        || key.ends_with("_peer_id")
        || key.ends_with("_machine_id")
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
    fn input_runtime_diagnostics_include_content_free_elevated_injector_status() {
        let diagnostics = input_runtime_diagnostics(
            InputRuntimeSnapshot {
                owner_peer_id: Some("peer-secret".to_string()),
                configured_capture_target_peer_id: None,
                active_capture_target_peer_id: None,
                lock_active: false,
                lock_supported: true,
                capture_backend_mode: "user_session_broker".to_string(),
                pending_inject_frames: 0,
                pending_inject_high_water: 1,
                elevated_injector_state: "unavailable".to_string(),
                elevated_injector_reason: "identity_rejected".to_string(),
                elevated_injector_signature_trust: "invalid".to_string(),
            },
            &mut RedactionContext::default(),
        );

        assert_eq!(diagnostics["elevated_injector_state"], "unavailable");
        assert_eq!(diagnostics["elevated_injector_reason"], "identity_rejected");
        assert_eq!(diagnostics["elevated_injector_signature_trust"], "invalid");
        assert_eq!(diagnostics["owner_peer_id"], "peer-1");
    }

    #[test]
    fn port_listener_rows_parse_ipv4_ipv6_and_classify_owners() {
        let rows = parse_port_listener_rows(
            r#"[
                {
                    "LocalAddress": "0.0.0.0",
                    "LocalPort": 15100,
                    "OwningProcess": 42,
                    "ProcessName": "boundless-service",
                    "ProcessPath": "C:\\Program Files\\Boundless\\boundless-service.exe"
                },
                {
                    "LocalAddress": "::",
                    "LocalPort": 15200,
                    "OwningProcess": 43,
                    "ProcessName": "PowerToys.MouseWithoutBorders",
                    "ProcessPath": "C:\\Program Files\\PowerToys\\MouseWithoutBorders.exe"
                },
                {
                    "LocalAddress": "127.0.0.1",
                    "LocalPort": 15101,
                    "OwningProcess": 44,
                    "ProcessName": "SomeTool",
                    "ProcessPath": "C:\\Tools\\SomeTool.exe"
                }
            ]"#,
        )
        .expect("parse listener rows");

        let snapshot = build_port_listener_diagnostics(rows);

        assert_eq!(snapshot.platform, "windows");
        assert!(snapshot.read_only);
        assert_eq!(snapshot.ports, vec![15100, 15101, 15200]);
        assert_eq!(snapshot.listeners.len(), 3);
        assert_eq!(snapshot.listeners[0].address_family, "ipv4");
        assert_eq!(snapshot.listeners[0].bind_scope, "any");
        assert_eq!(snapshot.listeners[0].owner_kind, "boundless");
        assert_eq!(snapshot.listeners[1].port, 15101);
        assert_eq!(snapshot.listeners[1].bind_scope, "loopback");
        assert_eq!(snapshot.listeners[1].owner_kind, "other");
        assert_eq!(snapshot.listeners[2].address_family, "ipv6");
        assert_eq!(snapshot.listeners[2].bind_scope, "any");
        assert_eq!(snapshot.listeners[2].owner_kind, "mouse-without-borders");
        assert!(
            snapshot
                .summary
                .iter()
                .any(|item| item.contains("Mouse Without Borders/PowerToys"))
        );
        assert!(
            snapshot
                .mitigation
                .iter()
                .any(|item| item.contains("alternate network_port"))
        );
    }

    #[test]
    fn port_listener_bundle_redacts_process_paths_without_raw_endpoints() {
        let snapshot = build_port_listener_diagnostics(vec![RawPortListenerRow {
            local_address: "10.10.0.5".to_string(),
            local_port: 15200,
            owning_process: Some(43),
            process_name: Some("PowerToys.MouseWithoutBorders".to_string()),
            process_path: Some("C:\\Users\\Alice\\PowerToys\\MouseWithoutBorders.exe".to_string()),
        }]);
        let redacted = redact_sensitive_json(json!({ "port_listeners": snapshot }), false);
        let rendered = serde_json::to_string(&redacted).expect("serialize redacted listeners");

        assert!(rendered.contains("mouse-without-borders"));
        assert!(rendered.contains("ipv4"));
        assert!(rendered.contains("specific"));
        assert!(rendered.contains("15200"));
        assert!(rendered.contains(REDACTED_PATH));
        assert!(!rendered.contains("10.10.0.5"));
        assert!(!rendered.contains("Alice"));
        assert!(!rendered.contains("MouseWithoutBorders.exe"));
    }

    #[test]
    fn port_listener_summary_reports_side_by_side_collision() {
        let snapshot = build_port_listener_diagnostics(vec![
            RawPortListenerRow {
                local_address: "0.0.0.0".to_string(),
                local_port: 15100,
                owning_process: Some(42),
                process_name: Some("boundless-service".to_string()),
                process_path: Some(
                    "C:\\Program Files\\Boundless\\boundless-service.exe".to_string(),
                ),
            },
            RawPortListenerRow {
                local_address: "::".to_string(),
                local_port: 15100,
                owning_process: Some(43),
                process_name: Some("PowerToys.MouseWithoutBorders".to_string()),
                process_path: Some(
                    "C:\\Program Files\\PowerToys\\MouseWithoutBorders.exe".to_string(),
                ),
            },
        ]);

        assert!(
            snapshot
                .summary
                .iter()
                .any(|item| item.contains("both appear on TCP 15100"))
        );
    }

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

        assert!(rendered.contains("metadata_only=true"));
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
    fn layout_matrix_preserves_shape_with_redacted_aliases() {
        let peers = vec![
            crate::queries::UiPairedPeer {
                peer_id: "peer-alpha-full".to_string(),
                display_name: "Office Display".to_string(),
                address: "10.0.0.8:15100".to_string(),
                connected: true,
                health_state: "connected".to_string(),
                health_reason: "peer is connected".to_string(),
                trust_state: "trusted".to_string(),
                trusted_since: "2026-03-03T18:00:00Z".to_string(),
                trust_fingerprint: "abcdef1234567890".to_string(),
                device_identity: "peer-alpha-full".to_string(),
            },
            crate::queries::UiPairedPeer {
                peer_id: "peer-beta-full".to_string(),
                display_name: "Kitchen Display".to_string(),
                address: "10.0.0.9:15100".to_string(),
                connected: false,
                health_state: "disconnected".to_string(),
                health_reason: "not connected".to_string(),
                trust_state: "trusted".to_string(),
                trusted_since: "2026-03-03T18:00:00Z".to_string(),
                trust_fingerprint: "1234567890abcdef".to_string(),
                device_identity: "peer-beta-full".to_string(),
            },
        ];
        let mut context = RedactionContext::default();

        let sanitized = sanitize_layout_matrix(
            "machine-local-raw,peer-alpha;Office Display,Local Laptop;peer-beta-full,,unknown-room",
            "machine-local-raw",
            "Local Laptop",
            &peers,
            &mut context,
        );

        assert_eq!(
            sanitized,
            "self,peer-1;peer-1,self;peer-2,,[redacted-layout-token]"
        );
        assert!(!sanitized.contains("machine-local-raw"));
        assert!(!sanitized.contains("Local Laptop"));
        assert!(!sanitized.contains("peer-alpha"));
        assert!(!sanitized.contains("peer-beta-full"));
        assert!(!sanitized.contains("Office Display"));
        assert!(!sanitized.contains("Kitchen Display"));
        assert!(!sanitized.contains("unknown-room"));
    }

    #[test]
    fn event_detail_redacts_freeform_identifier_keys() {
        let mut context = RedactionContext::default();
        let event = TransportEventSnapshot {
            timestamp: "2026-06-20T00:00:00Z".to_string(),
            direction: "local".to_string(),
            kind: "transport_service_issue".to_string(),
            peer_id: "peer-alpha-full".to_string(),
            detail: "peer_id=peer-alpha-full machine_id=machine-local-raw request_id=req-123 transfer_id=file-456 source=diagnostic".to_string(),
            size_bytes: 0,
        };

        let redacted = redact_transport_event(&event, false, &mut context);
        let rendered = serde_json::to_string(&redacted).expect("serialize redacted event");

        assert!(rendered.contains("peer_id=peer-1"));
        assert!(rendered.contains("machine_id=peer-2"));
        assert!(rendered.contains("request_id=[redacted-id]"));
        assert!(rendered.contains("transfer_id=[redacted-id]"));
        assert!(!rendered.contains("peer-alpha-full"));
        assert!(!rendered.contains("machine-local-raw"));
        assert!(!rendered.contains("req-123"));
        assert!(!rendered.contains("file-456"));
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

    #[test]
    fn package_manifest_version_accepts_utf8_bom() {
        assert_eq!(
            parse_package_manifest_version("\u{feff}{\"version\":\"5.0.4-dogfood-e89e5d0\"}")
                .as_deref(),
            Some("5.0.4-dogfood-e89e5d0")
        );
    }
}
