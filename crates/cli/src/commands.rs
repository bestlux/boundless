use super::*;
use app_services::desktop::{
    LayoutPeerToken, TcpEndpointCandidate, TcpEndpointSource, build_orientation_matrix,
    host_and_pairing_port_from_endpoint, is_local_layout_token as is_local_layout_token_shared,
    parse_layout_matrix, resolve_boundlessd_candidates, spawn_boundlessd_process,
    tcp_endpoint_candidate, terminate_boundlessd_processes, validate_layout_matrix_spec,
};
use app_services::diagnostics::{
    DiagnosticExportOptions, ServiceDiagnosticSnapshot, build_offline_bundle,
    write_diagnostic_bundle,
};
use app_services::install_doctor::{
    InstallDoctorReport, InstallEvidence, evaluate_install_evidence,
};
#[cfg(windows)]
use app_services::install_doctor::{REQUIRED_PAYLOADS, VERSIONED_EXECUTABLES};
use core_clipboard::sanitize_clipboard_event_output_detail;
#[cfg(any(windows, test))]
use std::path::PathBuf as StdPathBuf;
#[cfg(windows)]
use std::path::{Path, PathBuf};

const BOUNDLESS_SERVICE_NAME: &str = "BoundlessService";
#[cfg(windows)]
const BOUNDLESS_SERVICE_DISPLAY_NAME: &str = "Boundless Service";
#[cfg(windows)]
const USER_WRITABLE_SERVICE_SOURCE_DIRS: [&str; 3] = ["LOCALAPPDATA", "APPDATA", "TEMP"];

const OUTPUT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutputFormat {
    Human,
    Json,
}

impl OutputFormat {
    pub(super) fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

#[derive(Serialize)]
struct DaemonStatusJson<'a> {
    schema_version: u32,
    daemon_version: &'a str,
    running: bool,
    machine_id: &'a str,
    peer_count: u32,
    protocol_version: &'a str,
    api_bind: &'a str,
    api_transport: &'a str,
    api_pipe_name: &'a str,
    input_locked: bool,
    input_lock_supported: bool,
    capture_target_peer_id: &'a str,
    anti_idle_supported: bool,
    anti_idle_enabled: bool,
    anti_idle_active: bool,
    anti_idle_display_required: bool,
}

pub(super) async fn ensure_daemon_available(endpoint: &str, start_daemon: bool) -> Result<()> {
    let initial_error = match channel(endpoint).await {
        Ok(_) => return Ok(()),
        Err(error) => error,
    };

    if !start_daemon {
        bail!("daemon is not reachable at {endpoint}; run boundlessd or pass --start-daemon");
    }

    if is_named_pipe_endpoint(endpoint) && has_access_denied_io_error(&initial_error) {
        let launched = recover_stale_named_pipe_owner(endpoint).await?;
        println!("daemon_start=spawned path={launched}");
        return Ok(());
    }

    let launched = spawn_daemon_process()?;
    println!("daemon_start=spawned path={launched}");

    wait_for_daemon_ready(endpoint, "start attempt").await
}

async fn recover_stale_named_pipe_owner(endpoint: &str) -> Result<String> {
    let terminated = terminate_boundlessd_processes()?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    if channel(endpoint).await.is_ok() {
        return Ok("existing daemon became reachable after stale-process cleanup".to_string());
    }

    let launched = spawn_daemon_process()?;
    let context = if terminated {
        "stale-daemon recovery"
    } else {
        "named-pipe recovery"
    };
    wait_for_daemon_ready(endpoint, context).await?;
    Ok(format!(
        "{launched} (after clearing stale boundlessd.exe named-pipe owner)"
    ))
}

async fn wait_for_daemon_ready(endpoint: &str, context: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match channel(endpoint).await {
            Ok(_) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    bail!("daemon did not become reachable at {endpoint} after {context}: {error}");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn spawn_daemon_process() -> Result<String> {
    let candidates = resolve_boundlessd_candidates(std::env::current_exe().ok());
    spawn_boundlessd_process(&candidates)
}

#[cfg(windows)]
fn resolve_boundless_service_binary() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("resolve current executable")?;
    let Some(parent) = current_exe.parent() else {
        bail!("current executable has no parent directory");
    };
    Ok(parent.join("boundless-service.exe"))
}

pub(super) async fn daemon_status(endpoint: &str, output: OutputFormat) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let status = client.get_status(StatusRequest {}).await?.into_inner();
    match output {
        OutputFormat::Human => println!("{}", format_daemon_status_line(&status)),
        OutputFormat::Json => println!("{}", daemon_status_json(&status)?),
    }
    Ok(())
}

fn daemon_status_json(status: &StatusReply) -> Result<String> {
    serde_json::to_string_pretty(&DaemonStatusJson {
        schema_version: OUTPUT_SCHEMA_VERSION,
        daemon_version: &status.daemon_version,
        running: status.running,
        machine_id: &status.machine_id,
        peer_count: status.peer_count,
        protocol_version: &status.protocol_version,
        api_bind: &status.api_bind,
        api_transport: &status.api_transport,
        api_pipe_name: &status.api_pipe_name,
        input_locked: status.input_locked,
        input_lock_supported: status.input_lock_supported,
        capture_target_peer_id: &status.capture_target_peer_id,
        anti_idle_supported: status.anti_idle_supported,
        anti_idle_enabled: status.anti_idle_enabled,
        anti_idle_active: status.anti_idle_active,
        anti_idle_display_required: status.anti_idle_display_required,
    })
    .context("serialize daemon status")
}

pub(super) async fn doctor_install(endpoint: &str, output: OutputFormat) -> Result<()> {
    let evidence = collect_install_evidence(endpoint).await;
    let report = evaluate_install_evidence(evidence);
    print_install_doctor_report(&report, output)?;
    if report.ok {
        Ok(())
    } else {
        bail!("installed Boundless failed one or more verification checks")
    }
}

fn print_install_doctor_report(report: &InstallDoctorReport, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).context("serialize install doctor report")?
        ),
        OutputFormat::Human => {
            for check in &report.checks {
                println!(
                    "{} {} expected={} actual={} message={}",
                    if check.ok { "PASS" } else { "FAIL" },
                    check.id,
                    check.expected,
                    check.actual,
                    check.message
                );
            }
            println!("doctor_install={}", if report.ok { "pass" } else { "fail" });
        }
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Deserialize, Default)]
struct InstalledPackageManifest {
    version: String,
    executables: std::collections::BTreeMap<String, String>,
}

#[cfg(windows)]
async fn collect_install_evidence(endpoint: &str) -> InstallEvidence {
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let mut evidence = InstallEvidence {
        platform_supported: true,
        ..Default::default()
    };
    match platform_windows::install_verification::collect_windows_install_snapshot() {
        Ok(snapshot) => {
            evidence.product_codes = snapshot.product_codes;
            evidence.display_version = snapshot.display_version;
            evidence.install_root = snapshot.install_root.display().to_string();
            evidence.tray_count = snapshot.tray_count;
            evidence.tray_path_matches = snapshot.tray_path_matches;
            evidence.tray_responding = snapshot.tray_responding;
        }
        Err(error) => evidence
            .collection_errors
            .push(format!("windows install snapshot: {error:#}")),
    }

    let install_root = if evidence.install_root.is_empty() {
        std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
            .join("Boundless")
    } else {
        PathBuf::from(&evidence.install_root)
    };
    evidence.install_root = install_root.display().to_string();
    let manifest_path = install_root.join("package-manifest.json");
    evidence.manifest_present = manifest_path.is_file();
    match std::fs::read_to_string(&manifest_path)
        .context("read installed package manifest")
        .and_then(|contents| {
            serde_json::from_str::<InstalledPackageManifest>(
                contents.trim_start_matches('\u{feff}'),
            )
            .context("parse installed package manifest")
        }) {
        Ok(manifest) => {
            evidence.manifest_version = manifest.version;
            evidence.manifest_executables = manifest.executables;
        }
        Err(error) => evidence
            .collection_errors
            .push(format!("manifest: {error:#}")),
    }
    for payload in REQUIRED_PAYLOADS {
        evidence
            .payloads_present
            .insert(payload.to_string(), install_root.join(payload).is_file());
    }

    match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).and_then(
        |manager| {
            manager.open_service(
                BOUNDLESS_SERVICE_NAME,
                ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
            )
        },
    ) {
        Ok(service) => {
            match service.query_config() {
                Ok(config) => {
                    evidence.service_account = config
                        .account_name
                        .map(|value| value.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let binary =
                        extract_service_executable_path(&config.executable_path.to_string_lossy());
                    evidence.service_binary_path = binary.display().to_string();
                    evidence.service_binary_path_matches = paths_equal_case_insensitive(
                        &binary,
                        &install_root.join("boundless-service.exe"),
                    );
                }
                Err(error) => evidence
                    .collection_errors
                    .push(format!("service config: {error}")),
            }
            match service.query_status() {
                Ok(status) => {
                    evidence.service_running = status.current_state == ServiceState::Running
                }
                Err(error) => evidence
                    .collection_errors
                    .push(format!("service status: {error}")),
            }
        }
        Err(error) => evidence
            .collection_errors
            .push(format!("open BoundlessService: {error}")),
    }

    for executable in VERSIONED_EXECUTABLES {
        let file_name = format!("{executable}.exe");
        let version = probe_executable_version(&install_root.join(file_name), executable)
            .unwrap_or_else(|error| {
                evidence
                    .collection_errors
                    .push(format!("{executable} version: {error:#}"));
                String::new()
            });
        evidence
            .executable_versions
            .insert(executable.to_string(), version);
    }

    match connect_control_plane(endpoint).await {
        Ok(mut client) => match client.get_status(StatusRequest {}).await {
            Ok(status) => {
                let status = status.into_inner();
                evidence.daemon_api_healthy = true;
                evidence.daemon_running = status.running;
                evidence.daemon_runtime_version = status.daemon_version;
            }
            Err(error) => evidence
                .collection_errors
                .push(format!("daemon status: {error}")),
        },
        Err(error) => evidence
            .collection_errors
            .push(format!("daemon API: {error}")),
    }
    evidence
}

#[cfg(not(windows))]
async fn collect_install_evidence(_endpoint: &str) -> InstallEvidence {
    InstallEvidence {
        collection_errors: vec!["install verification is supported on Windows only".to_string()],
        ..Default::default()
    }
}

#[cfg(windows)]
fn probe_executable_version(path: &Path, expected_name: &str) -> Result<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {} --version", path.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if !output.status.success() {
                bail!("{} --version exited {}", path.display(), output.status);
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut fields = stdout.split_whitespace();
            let name = fields.next().unwrap_or_default();
            let version = fields.next().unwrap_or_default();
            if name != expected_name || version.is_empty() || fields.next().is_some() {
                bail!("{} returned malformed version output", path.display());
            }
            return Ok(version.to_string());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} --version timed out", path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn paths_equal_case_insensitive(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn format_daemon_status_line(status: &StatusReply) -> String {
    format!(
        "running={} daemon_version={} machine_id={} peers={} protocol={} api_transport={} api_bind={} api_pipe_name={} input_locked={} input_lock_supported={} active_capture_target={} anti_idle_supported={} anti_idle_enabled={} anti_idle_active={} anti_idle_display_required={}",
        status.running,
        status.daemon_version,
        status.machine_id,
        status.peer_count,
        status.protocol_version,
        status.api_transport,
        status.api_bind,
        status.api_pipe_name,
        status.input_locked,
        status.input_lock_supported,
        if status.capture_target_peer_id.is_empty() {
            "none"
        } else {
            status.capture_target_peer_id.as_str()
        },
        status.anti_idle_supported,
        status.anti_idle_enabled,
        status.anti_idle_active,
        status.anti_idle_display_required
    )
}

pub(super) async fn pair_create_code(endpoint: &str, ttl: u32) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .create_pairing_code(PairCreateCodeRequest { ttl_seconds: ttl })
        .await?
        .into_inner();

    println!("code={} expires_at={}", response.code, response.expires_at);
    Ok(())
}

pub(super) async fn pair_discover(endpoint: &str) -> Result<()> {
    let discovered = list_discovered_peer_records(endpoint).await?;
    if discovered.is_empty() {
        println!("no discovered peers (mDNS may still be warming up)");
        return Ok(());
    }

    for (index, peer) in discovered.iter().enumerate() {
        let pairing_port = host_and_pairing_port_from_endpoint(&peer.endpoint)
            .map(|(_, port)| port)
            .unwrap_or(app_services::desktop::DEFAULT_PAIRING_PORT);
        println!(
            "[{}] name={} endpoint={} machine_id={} pairing_port={} mdns=discovered pairing_reachability=unchecked transport_reachability=unchecked transport_candidates=[{}] pairing_candidates=[{}]",
            index + 1,
            peer.display_name,
            peer.endpoint,
            short_machine_id(&peer.machine_id),
            pairing_port,
            redacted_transport_candidates(peer),
            redacted_pairing_candidates(peer)
        );
    }
    Ok(())
}

fn redacted_transport_candidates(peer: &DiscoveredPeerRecord) -> String {
    redacted_candidate_labels(&discovery_transport_candidates(peer))
}

fn redacted_pairing_candidates(peer: &DiscoveredPeerRecord) -> String {
    let endpoints = if peer.endpoint_candidates.is_empty() {
        vec![peer.endpoint.clone()]
    } else {
        peer.endpoint_candidates.clone()
    };
    let pairing_candidates = endpoints
        .iter()
        .filter_map(|endpoint| {
            host_and_pairing_port_from_endpoint(endpoint)
                .ok()
                .map(|(host, port)| format_host_port(&host, port))
        })
        .collect::<Vec<_>>();
    let candidates = pairing_candidates
        .iter()
        .enumerate()
        .map(|(ordinal, endpoint)| {
            tcp_endpoint_candidate(endpoint, TcpEndpointSource::Discovery, ordinal)
        })
        .collect::<Vec<_>>();
    redacted_candidate_labels(&candidates)
}

fn discovery_transport_candidates(peer: &DiscoveredPeerRecord) -> Vec<TcpEndpointCandidate> {
    let endpoints = if peer.endpoint_candidates.is_empty() {
        vec![peer.endpoint.clone()]
    } else {
        peer.endpoint_candidates.clone()
    };
    endpoints
        .iter()
        .enumerate()
        .map(|(ordinal, endpoint)| {
            tcp_endpoint_candidate(endpoint, TcpEndpointSource::Discovery, ordinal)
        })
        .collect()
}

fn redacted_candidate_labels(candidates: &[TcpEndpointCandidate]) -> String {
    if candidates.is_empty() {
        return "none".to_string();
    }
    candidates
        .iter()
        .map(TcpEndpointCandidate::redacted_provenance_label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn filter_connectable_discovered_peer_records(
    peers: Vec<DiscoveredPeerRecord>,
    local_machine_id: &str,
    paired_peers: &[PeerRecord],
) -> Vec<DiscoveredPeerRecord> {
    let paired_peer_ids = paired_peers
        .iter()
        .map(|peer| peer.peer_id.clone())
        .collect::<Vec<_>>();
    let mut peers =
        filter_connectable_discovery_records(peers, local_machine_id, &paired_peer_ids, |peer| {
            peer.machine_id.clone()
        });
    peers.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.machine_id.cmp(&b.machine_id))
    });
    peers
}

pub(super) struct PairRequestArgs {
    pub(super) selector: String,
    pub(super) request_id: Option<String>,
    pub(super) verification_nonce: Option<String>,
    pub(super) host_override: Option<String>,
    pub(super) port_override: Option<u16>,
    pub(super) code: Option<String>,
    pub(super) alias: Option<String>,
    pub(super) timeout_seconds: u64,
}

pub(super) struct NearbyJoinCliRequest {
    pub(super) code: String,
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) timeout_seconds: u64,
    pub(super) alias: Option<String>,
    pub(super) endpoint_candidates: Vec<String>,
    pub(super) role_reversal: bool,
}

pub(super) async fn pair_request(endpoint: &str, args: PairRequestArgs) -> Result<()> {
    let PairRequestArgs {
        selector,
        request_id,
        verification_nonce,
        host_override,
        port_override,
        code,
        alias,
        timeout_seconds,
    } = args;
    let (
        host,
        pairing_port,
        default_alias,
        selector_hint,
        target_label,
        target_endpoint,
        endpoint_candidates,
    ) = if let Some(host_override) = host_override {
        let host = host_override.trim().to_string();
        if host.is_empty() {
            bail!("--host must not be empty");
        }
        let pairing_port = port_override.unwrap_or(app_services::desktop::DEFAULT_PAIRING_PORT);
        (
            host.clone(),
            pairing_port,
            None,
            selector.clone(),
            host.clone(),
            format_host_port(&host, pairing_port),
            Vec::new(),
        )
    } else {
        let discovered = list_discovered_peer_records(endpoint).await?;
        if discovered.is_empty() {
            if request_id.is_some() {
                bail!(
                    "no discovered peers available for selector `{selector}`; retry with `--host <target-host-or-ip> --port <pairing-port>`"
                );
            }
            bail!(
                "no discovered peers available; try `pair nearby-join <code> --host <host> --port <port>`"
            );
        }
        let selected = resolve_discovered_peer_record(&discovered, &selector)?;
        let (host, pairing_port) = host_and_pairing_port_from_endpoint(&selected.endpoint)
            .with_context(|| format!("invalid discovered endpoint {}", selected.endpoint))?;
        (
            host,
            pairing_port,
            Some(selected.display_name.clone()),
            selected.machine_id.clone(),
            selected.display_name.clone(),
            selected.endpoint.clone(),
            selected.endpoint_candidates.clone(),
        )
    };

    let alias = alias.or(default_alias);
    println!(
        "pair_request target={} endpoint={} pairing_port={} machine_id={}",
        target_label,
        target_endpoint,
        pairing_port,
        short_machine_id(&selector_hint),
    );

    if let Some(request_id) = request_id {
        let code = if let Some(value) = code {
            value
        } else {
            prompt_pairing_code()?
        };
        if code.trim().is_empty() {
            bail!("pairing code must not be empty");
        }
        let verification_nonce = if let Some(value) = verification_nonce {
            value
        } else {
            prompt_pairing_nonce()?
        };
        if verification_nonce.trim().is_empty() {
            bail!("pairing nonce must not be empty");
        }
        return pair_nearby_submit_code(
            endpoint,
            NearbySubmitCodeRequest {
                host,
                port: u32::from(pairing_port),
                request_id,
                code,
                verification_nonce,
                alias: alias.unwrap_or_default(),
                endpoint_candidates,
            },
        )
        .await;
    }

    if let Some(code) = code {
        if code.trim().is_empty() {
            bail!("pairing code must not be empty");
        }
        return pair_nearby_join(
            endpoint,
            NearbyJoinCliRequest {
                code,
                host,
                port: pairing_port,
                timeout_seconds,
                alias,
                endpoint_candidates,
                role_reversal: false,
            },
        )
        .await;
    }

    match pair_nearby_request_code(
        endpoint,
        host.clone(),
        pairing_port,
        alias,
        endpoint_candidates,
    )
    .await?
    {
        NearbyRequestCodeStart::CodeRequired {
            request_id,
            verification_nonce,
            expires_at,
        } => {
            println!(
                "pair_request_code_started=true request_id={} verification_nonce={} expires_at={}",
                request_id, verification_nonce, expires_at
            );
            println!("enter code shown on target machine, then submit:");
            println!(
                "  boundlessctl pair request {} --request-id {} --nonce {} --code <6-digit-code> --host {} --port {}",
                selector_hint, request_id, verification_nonce, host, pairing_port
            );
            Ok(())
        }
        NearbyRequestCodeStart::Unsupported { reason } => {
            bail!("target does not support the canonical guided pairing request flow ({reason})");
        }
    }
}

pub(super) async fn setup_wizard(endpoint: &str, start_daemon: bool) -> Result<()> {
    ensure_daemon_available(endpoint, start_daemon).await?;

    println!("Boundless setup wizard");
    println!("This flow pairs this PC with one peer and optionally sets orientation.");

    let existing_peers = list_peer_records(endpoint).await?;
    if !existing_peers.is_empty() {
        println!(
            "note: {} peer(s) already configured; setup will add/update one peer only",
            existing_peers.len()
        );
    }

    let discovered = list_discovered_peer_records(endpoint).await?;
    let (host, pairing_port, default_alias, endpoint_candidates) = if discovered.is_empty() {
        println!("No discovered peers yet. Falling back to manual host entry.");
        let host = prompt_required("Peer host/IP")?;
        let port = prompt_u16_with_default(
            "Peer nearby pairing port",
            app_services::desktop::DEFAULT_PAIRING_PORT,
        )?;
        (host, port, None, Vec::new())
    } else {
        println!("Discovered peers:");
        for (index, peer) in discovered.iter().enumerate() {
            println!(
                "  [{}] {} endpoint={} machine_id={}",
                index + 1,
                peer.display_name,
                peer.endpoint,
                short_machine_id(&peer.machine_id)
            );
        }
        println!("Type an index/machine_id/display-name prefix or `manual`.");
        let selector = prompt_required("Peer selector")?;
        if selector.eq_ignore_ascii_case("manual") {
            let host = prompt_required("Peer host/IP")?;
            let port = prompt_u16_with_default(
                "Peer nearby pairing port",
                app_services::desktop::DEFAULT_PAIRING_PORT,
            )?;
            (host, port, None, Vec::new())
        } else {
            let selected = resolve_discovered_peer_record(&discovered, &selector)?;
            let (host, pairing_port) = host_and_pairing_port_from_endpoint(&selected.endpoint)
                .with_context(|| format!("invalid discovered endpoint {}", selected.endpoint))?;
            (
                host,
                pairing_port,
                Some(selected.display_name.clone()),
                selected.endpoint_candidates.clone(),
            )
        }
    };

    println!("On the peer PC, run `boundlessctl pair create-code --ttl 120` and copy the code.");
    let code = prompt_pairing_code()?;
    if code.trim().is_empty() {
        bail!("pairing code must not be empty");
    }

    let alias = prompt_optional_with_default("Alias for this peer", default_alias.as_deref())?;
    pair_nearby_join(
        endpoint,
        NearbyJoinCliRequest {
            code,
            host,
            port: pairing_port,
            timeout_seconds: 120,
            alias: alias.clone(),
            endpoint_candidates,
            role_reversal: false,
        },
    )
    .await?;

    let updated_peers = list_peer_records(endpoint).await?;
    let new_peer = find_new_peer_record(&existing_peers, &updated_peers).or_else(|| {
        alias.as_deref().and_then(|candidate_alias| {
            updated_peers
                .iter()
                .find(|peer| peer.display_name.eq_ignore_ascii_case(candidate_alias))
                .cloned()
        })
    });

    if existing_peers.is_empty() {
        if let Some(peer) = new_peer {
            println!(
                "Where is `{}` relative to this PC? [left/right/up/down/skip]",
                peer.display_name
            );
            let side = prompt_optional("Orientation")?;
            if let Some(side) = side {
                let normalized = side.to_ascii_lowercase();
                match normalized.as_str() {
                    "left" | "l" => {
                        layout_orient(endpoint, Some(peer.peer_id), None, None, None).await?
                    }
                    "right" | "r" => {
                        layout_orient(endpoint, None, Some(peer.peer_id), None, None).await?
                    }
                    "up" | "u" | "top" => {
                        layout_orient(endpoint, None, None, Some(peer.peer_id), None).await?
                    }
                    "down" | "d" | "bottom" => {
                        layout_orient(endpoint, None, None, None, Some(peer.peer_id)).await?
                    }
                    "skip" | "s" | "" => {
                        println!("layout unchanged; run `boundlessctl layout wizard` later");
                    }
                    _ => {
                        println!(
                            "unrecognized orientation `{side}`; layout unchanged (run `boundlessctl layout wizard`)"
                        );
                    }
                }
            }
        }
    } else {
        println!("layout was not auto-updated because peers already existed");
        println!("run `boundlessctl layout wizard` to adjust orientation");
    }

    println!("Setup complete.");
    println!("Next steps:");
    println!(
        "  - run `boundlessctl pair pending` on remote PC to approve pending requests, if needed"
    );
    println!("  - run `boundlessctl layout preview` to verify orientation");
    Ok(())
}

pub(super) async fn pair_join(
    endpoint: &str,
    code: String,
    host: String,
    alias: Option<String>,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .join_with_pairing_code(PairJoinRequest {
            code,
            host,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();
    println!(
        "accepted={} peer_id={} message={}",
        response.accepted, response.peer_id, response.message
    );
    Ok(())
}

pub(super) async fn pair_nearby_join(endpoint: &str, request: NearbyJoinCliRequest) -> Result<()> {
    let NearbyJoinCliRequest {
        code,
        host,
        port,
        timeout_seconds,
        alias,
        endpoint_candidates,
        role_reversal,
    } = request;
    let role = if role_reversal {
        "role-reversal-request".to_string()
    } else {
        String::new()
    };
    let alias_value = alias.unwrap_or_default();
    let mut control_plane = connect_control_plane(endpoint).await?;
    let initial_response = control_plane
        .start_nearby_pairing_join(NearbyJoinStartRequest {
            host: host.clone(),
            port: u32::from(port),
            code,
            alias: alias_value.clone(),
            endpoint_candidates: endpoint_candidates.clone(),
            role: role.clone(),
            attempt_id: String::new(),
        })
        .await?
        .into_inner();
    let peer_machine_id = wait_for_nearby_pairing_approval(
        endpoint,
        NearbyApprovalPoll {
            host,
            port: u32::from(port),
            initial_response,
            timeout_seconds,
            expected_request_id: String::new(),
            alias: alias_value,
            endpoint_candidates,
            role,
        },
    )
    .await?;
    println!("accepted=true peer_machine_id={peer_machine_id} message=nearby pairing complete");
    Ok(())
}

enum NearbyRequestCodeStart {
    CodeRequired {
        request_id: String,
        verification_nonce: String,
        expires_at: String,
    },
    Unsupported {
        reason: String,
    },
}

struct NearbyApprovalPoll {
    host: String,
    port: u32,
    initial_response: ipc_api::boundless::v1::NearbyJoinStatusReply,
    timeout_seconds: u64,
    expected_request_id: String,
    alias: String,
    endpoint_candidates: Vec<String>,
    role: String,
}

async fn pair_nearby_request_code(
    endpoint: &str,
    host: String,
    port: u16,
    alias: Option<String>,
    endpoint_candidates: Vec<String>,
) -> Result<NearbyRequestCodeStart> {
    let mut control_plane = connect_control_plane(endpoint).await?;
    let response = control_plane
        .request_nearby_pairing_code(NearbyRequestCodeStartRequest {
            host,
            port: u32::from(port),
            alias: alias.unwrap_or_default(),
            endpoint_candidates,
        })
        .await?
        .into_inner();

    if response.code_required {
        return Ok(NearbyRequestCodeStart::CodeRequired {
            request_id: response.request_id,
            verification_nonce: response.verification_nonce,
            expires_at: response.verification_expires_at,
        });
    }

    if response.unsupported {
        return Ok(NearbyRequestCodeStart::Unsupported {
            reason: response.message,
        });
    }

    let message = response.message.trim();
    if message.is_empty() {
        bail!("nearby pairing request failed");
    }
    bail!("nearby pairing request failed: {message}");
}

async fn wait_for_nearby_pairing_approval(
    endpoint: &str,
    poll: NearbyApprovalPoll,
) -> Result<String> {
    let NearbyApprovalPoll {
        host,
        port,
        initial_response,
        timeout_seconds,
        expected_request_id,
        alias,
        endpoint_candidates,
        role,
    } = poll;
    let mut response = initial_response;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds.max(5));
    let mut poll_count = 0_u64;

    loop {
        let status = response.status.trim().to_ascii_lowercase();
        match status.as_str() {
            "approved" => {
                if !expected_request_id.is_empty()
                    && response.request_id != expected_request_id.as_str()
                {
                    bail!("nearby pairing request id mismatch");
                }
                if response.peer_machine_id.trim().is_empty() {
                    bail!("nearby pairing failed: approved status missing peer machine id");
                }
                return Ok(response.peer_machine_id);
            }
            "pending" => {
                let request_id = response.request_id;
                println!(
                    "pending=true request_id={} message={}",
                    request_id, response.message
                );
                if !expected_request_id.is_empty() && request_id != expected_request_id.as_str() {
                    bail!("nearby pairing request id mismatch");
                }

                loop {
                    if std::time::Instant::now() >= deadline {
                        bail!(
                            "timed out waiting for nearby pairing approval request_id={request_id}"
                        );
                    }

                    tokio::time::sleep(Duration::from_secs(1)).await;
                    poll_count += 1;
                    if poll_count.is_multiple_of(5) {
                        println!(
                            "pending=true request_id={} waited={}s",
                            request_id, poll_count
                        );
                    }

                    let mut control_plane = connect_control_plane(endpoint).await?;
                    response = control_plane
                        .check_nearby_pairing_join(NearbyJoinStatusRequest {
                            host: host.clone(),
                            port,
                            request_id: request_id.clone(),
                            alias: alias.clone(),
                            endpoint_candidates: endpoint_candidates.clone(),
                            role: role.clone(),
                            attempt_id: String::new(),
                        })
                        .await?
                        .into_inner();
                    let next_status = response.status.trim().to_ascii_lowercase();
                    if next_status == "pending" {
                        continue;
                    }
                    break;
                }
            }
            "rejected" => bail!("nearby pairing rejected: {}", response.message),
            "error" | "code_required" => bail!("nearby pairing failed: {}", response.message),
            _ => {
                let message = response.message.trim();
                if message.is_empty() {
                    bail!(
                        "nearby pairing failed: unknown status `{}`",
                        response.status
                    );
                }
                bail!("nearby pairing failed: {message}");
            }
        }
    }
}

async fn pair_nearby_submit_code(endpoint: &str, request: NearbySubmitCodeRequest) -> Result<()> {
    let expected_request_id = request.request_id.clone();
    let mut control_plane = connect_control_plane(endpoint).await?;
    let response = control_plane
        .submit_nearby_pairing_code(request)
        .await?
        .into_inner();
    if !response.ok {
        bail!("nearby pairing failed: {}", response.message);
    }
    if response.request_id != expected_request_id {
        bail!("nearby pairing request id mismatch");
    }
    let peer_machine_id = response.peer_machine_id;
    println!(
        "accepted=true peer_machine_id={} trust_committed={} already_committed={} reconnect_status={} message={}",
        peer_machine_id,
        response.trust_committed,
        response.already_committed,
        response.reconnect_status,
        response.message
    );
    Ok(())
}

pub(super) async fn pair_pending(endpoint: &str) -> Result<()> {
    let snapshot = fetch_ui_snapshot(endpoint).await?;

    if snapshot.pending_requests.is_empty() {
        println!("no pending nearby pairing requests");
        return Ok(());
    }

    for request in snapshot.pending_requests {
        let requires_code = request.requires_verification_code;
        let has_visible_code = requires_code && !request.verification_code.trim().is_empty();
        println!(
            "request_id={} requester_machine_id={} requester_display_name={} created_at={} flow={} role={} attempt_id={} verification_code={} verification_expires_at={}",
            request.request_id,
            request.requester_machine_id,
            request.requester_display_name,
            request.created_at,
            if requires_code {
                "code_confirmation"
            } else {
                "manual_approval"
            },
            if request.role.trim().is_empty() {
                "initiator"
            } else {
                request.role.as_str()
            },
            if request.attempt_id.trim().is_empty() {
                "-"
            } else {
                request.attempt_id.as_str()
            },
            if has_visible_code {
                request.verification_code.as_str()
            } else if requires_code {
                "(hidden)"
            } else {
                "-"
            },
            if has_visible_code {
                request.verification_expires_at.as_str()
            } else if requires_code {
                "(hidden)"
            } else {
                "-"
            }
        );
    }
    Ok(())
}

pub(super) async fn pair_approve(
    endpoint: &str,
    request_id: String,
    alias: Option<String>,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .approve_nearby_pairing_request(NearbyPairingDecisionRequest {
            request_id,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn pair_reject(endpoint: &str, request_id: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .reject_nearby_pairing_request(NearbyPairingDecisionRequest {
            request_id,
            alias: String::new(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn pair_export_trust(endpoint: &str, output: Option<String>) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client.export_trust_bundle(Empty {}).await?.into_inner();

    let bundle = StoredTrustBundle {
        machine_id: response.machine_id,
        display_name: response.display_name,
        network_address: response.network_address,
        ca_cert_pem: response.ca_cert_pem,
    };

    let json = serde_json::to_string_pretty(&bundle).context("serialize trust bundle")?;

    if let Some(path) = output {
        std::fs::write(&path, &json).with_context(|| format!("write {path}"))?;
        println!("wrote trust bundle to {path}");
    } else {
        println!("{json}");
    }

    Ok(())
}

pub(super) async fn pair_import_trust(
    endpoint: &str,
    input: String,
    alias: Option<String>,
) -> Result<()> {
    let raw = std::fs::read_to_string(&input).with_context(|| format!("read {input}"))?;
    let bundle: StoredTrustBundle = serde_json::from_str(&raw).context("parse trust bundle")?;

    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .import_trust_bundle(ImportTrustBundleRequest {
            machine_id: bundle.machine_id,
            display_name: bundle.display_name,
            network_address: bundle.network_address,
            ca_cert_pem: bundle.ca_cert_pem,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn pair_rotate_trust(endpoint: &str, confirm: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .rotate_trust(RotateTrustRequest { confirm })
        .await?
        .into_inner();
    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

#[derive(Serialize)]
struct PeerListJson<'a> {
    schema_version: u32,
    peers: Vec<PeerJson<'a>>,
}

#[derive(Serialize)]
struct PeerJson<'a> {
    peer_id: &'a str,
    display_name: &'a str,
    address: &'a str,
    connected: bool,
    health_state: &'a str,
    health_reason: &'a str,
    trust_state: &'a str,
    trusted_since: &'a str,
    trust_fingerprint: &'a str,
    device_identity: &'a str,
}

pub(super) async fn peer_list(endpoint: &str, output: OutputFormat) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client.list_peers(Empty {}).await?.into_inner();

    if output == OutputFormat::Json {
        println!("{}", peer_list_json(&response.peers)?);
        return Ok(());
    }

    if response.peers.is_empty() {
        println!("no peers configured");
        return Ok(());
    }

    for peer in response.peers {
        println!(
            "peer_id={} name={} address={} connected={} health_state={} trust_state={} trusted_since={} trust_fingerprint={}",
            peer.peer_id,
            peer.display_name,
            peer.address,
            peer.connected,
            peer.health_state,
            peer.trust_state,
            peer.trusted_since,
            peer.trust_fingerprint
        );
    }

    Ok(())
}

pub(super) async fn peer_remove(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .remove_peer(RemovePeerRequest { peer_id })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn layout_show(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client.layout_show(Empty {}).await?.into_inner();
    println!("{}", response.matrix_spec);
    Ok(())
}

pub(super) async fn layout_set(endpoint: &str, matrix: String) -> Result<()> {
    validate_layout_for_endpoint(endpoint, &matrix).await?;
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .layout_set(LayoutSetRequest {
            matrix_spec: matrix,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

#[cfg(any(windows, test))]
fn extract_service_executable_path(raw_binary_path: &str) -> StdPathBuf {
    let trimmed = raw_binary_path.trim();
    if let Some(rest) = trimmed.strip_prefix('"')
        && let Some((path, _)) = rest.split_once('"')
    {
        return StdPathBuf::from(path);
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(index) = lower.find(".exe") {
        return StdPathBuf::from(&trimmed[..index + 4]);
    }

    StdPathBuf::from(trimmed.split_whitespace().next().unwrap_or(trimmed))
}

#[cfg(any(windows, test))]
fn service_version_parity(service_version: Option<&str>, expected_version: &str) -> &'static str {
    let Some(service_version) = service_version else {
        return "unknown";
    };
    if normalize_version(service_version) == normalize_version(expected_version) {
        "matched"
    } else {
        "mismatched"
    }
}

#[cfg(any(windows, test))]
fn normalize_version(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

#[cfg(windows)]
fn service_binary_manifest_version(binary_path: &Path) -> (String, &'static str) {
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

#[cfg(any(windows, test))]
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

#[cfg(windows)]
fn collect_service_diagnostics() -> ServiceDiagnosticSnapshot {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(error) => {
            return ServiceDiagnosticSnapshot {
                platform: "windows".to_string(),
                service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                installed: false,
                state: "unknown".to_string(),
                process_id: None,
                binary_path: None,
                service_version: "unknown".to_string(),
                service_version_source: "service_manager_unavailable".to_string(),
                current_version,
                version_parity: "unknown".to_string(),
                error: Some(error.to_string()),
            };
        }
    };

    match manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(service) => {
            let status = service.query_status();
            let config = service.query_config();
            match (status, config) {
                (Ok(status), Ok(config)) => {
                    let binary =
                        extract_service_executable_path(&config.executable_path.to_string_lossy());
                    let (service_version, version_source) =
                        service_binary_manifest_version(&binary);
                    let known_service_version =
                        (service_version != "unknown").then_some(service_version.as_str());
                    ServiceDiagnosticSnapshot {
                        platform: "windows".to_string(),
                        service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                        installed: true,
                        state: format!("{:?}", status.current_state),
                        process_id: status.process_id,
                        binary_path: Some(binary.display().to_string()),
                        version_parity: service_version_parity(
                            known_service_version,
                            &current_version,
                        )
                        .to_string(),
                        service_version,
                        service_version_source: version_source.to_string(),
                        current_version,
                        error: None,
                    }
                }
                (status, config) => ServiceDiagnosticSnapshot {
                    platform: "windows".to_string(),
                    service_name: BOUNDLESS_SERVICE_NAME.to_string(),
                    installed: true,
                    state: "unknown".to_string(),
                    process_id: None,
                    binary_path: None,
                    service_version: "unknown".to_string(),
                    service_version_source: "query_failed".to_string(),
                    current_version,
                    version_parity: "unknown".to_string(),
                    error: Some(format!(
                        "status_error={} config_error={}",
                        status
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_default(),
                        config
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_default()
                    )),
                },
            }
        }
        Err(error) => ServiceDiagnosticSnapshot {
            platform: "windows".to_string(),
            service_name: BOUNDLESS_SERVICE_NAME.to_string(),
            installed: false,
            state: "not_installed".to_string(),
            process_id: None,
            binary_path: None,
            service_version: "unknown".to_string(),
            service_version_source: "not_installed".to_string(),
            current_version,
            version_parity: "not_installed".to_string(),
            error: Some(error.to_string()),
        },
    }
}

#[cfg(not(windows))]
fn collect_service_diagnostics() -> ServiceDiagnosticSnapshot {
    ServiceDiagnosticSnapshot {
        platform: "non-windows".to_string(),
        service_name: BOUNDLESS_SERVICE_NAME.to_string(),
        installed: false,
        state: "unsupported".to_string(),
        process_id: None,
        binary_path: None,
        service_version: "unknown".to_string(),
        service_version_source: "unsupported".to_string(),
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        version_parity: "unsupported".to_string(),
        error: None,
    }
}

#[cfg(windows)]
pub(super) async fn service_status() -> Result<()> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(service) => {
            let status = service.query_status()?;
            let config = service.query_config()?;
            let binary = extract_service_executable_path(&config.executable_path.to_string_lossy());
            let (service_version, version_source) = service_binary_manifest_version(&binary);
            let known_service_version =
                (service_version != "unknown").then_some(service_version.as_str());
            let parity = service_version_parity(known_service_version, env!("CARGO_PKG_VERSION"));
            println!(
                "installed=true service={} state={:?} process_id={} binary={} service_version={} service_version_source={} cli_version={} version_parity={}",
                BOUNDLESS_SERVICE_NAME,
                status.current_state,
                status.process_id.unwrap_or_default(),
                binary.display(),
                service_version,
                version_source,
                env!("CARGO_PKG_VERSION"),
                parity
            );
        }
        Err(error) => {
            println!(
                "installed=false service={} state=not_installed cli_version={} version_parity=not_installed error={}",
                BOUNDLESS_SERVICE_NAME,
                env!("CARGO_PKG_VERSION"),
                error
            );
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) async fn service_status() -> Result<()> {
    println!(
        "installed=false service={BOUNDLESS_SERVICE_NAME} state=unsupported platform=non-windows cli_version={} version_parity=unsupported",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

#[cfg(windows)]
pub(super) async fn service_install(binary: Option<String>, auto_start: bool) -> Result<()> {
    use platform_windows::runtime::current_user_sid_string;
    use std::ffi::OsString;
    use windows_service::{
        service::{ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let service_binary_path = binary
        .map(PathBuf::from)
        .unwrap_or(resolve_boundless_service_binary()?);
    if !service_binary_path.is_file() {
        bail!(
            "service binary was not found: {}",
            service_binary_path.display()
        );
    }
    reject_user_writable_service_source(&service_binary_path)?;
    let allowed_user_sid = current_user_sid_string()
        .context("resolve installing user SID for service control pipe ACL")?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let service_info = ServiceInfo {
        name: OsString::from(BOUNDLESS_SERVICE_NAME),
        display_name: OsString::from(BOUNDLESS_SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: if auto_start {
            ServiceStartType::AutoStart
        } else {
            ServiceStartType::OnDemand
        },
        error_control: ServiceErrorControl::Normal,
        executable_path: service_binary_path.clone(),
        launch_arguments: vec![OsString::from(format!(
            "--allowed-user-sid={allowed_user_sid}"
        ))],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = manager.create_service(
        &service_info,
        ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG,
    )?;
    service.set_description("Boundless service-mode daemon host")?;
    println!(
        "installed=true service={} binary={} start_type={} control_pipe_acl=system,administrators,installing_user",
        BOUNDLESS_SERVICE_NAME,
        service_binary_path.display(),
        if auto_start { "auto" } else { "demand" }
    );
    Ok(())
}

#[cfg(not(windows))]
pub(super) async fn service_install(_binary: Option<String>, _auto_start: bool) -> Result<()> {
    bail!("service install is only supported on Windows")
}

#[cfg(windows)]
fn reject_user_writable_service_source(path: &std::path::Path) -> Result<()> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize service binary {}", path.display()))?;
    for var in USER_WRITABLE_SERVICE_SOURCE_DIRS {
        let Ok(root) = std::env::var(var) else {
            continue;
        };
        if root.trim().is_empty() {
            continue;
        }
        let root = PathBuf::from(root);
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if canonical.starts_with(&root) {
            bail!(
                "refusing to install LocalSystem service from user-writable path `{}`; copy boundless-service.exe to an admin-protected directory such as %ProgramFiles%\\Boundless and rerun with --binary",
                canonical.display()
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) async fn service_start() -> Result<()> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.start::<&str>(&[])?;
    let status = service.query_status()?;
    println!(
        "start_requested=true service={} state={:?}",
        BOUNDLESS_SERVICE_NAME, status.current_state
    );
    Ok(())
}

#[cfg(not(windows))]
pub(super) async fn service_start() -> Result<()> {
    bail!("service start is only supported on Windows")
}

#[cfg(windows)]
pub(super) async fn service_stop() -> Result<()> {
    use windows_service::{
        service::ServiceAccess,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    let status = service.stop()?;
    println!(
        "stop_requested=true service={} state={:?}",
        BOUNDLESS_SERVICE_NAME, status.current_state
    );
    Ok(())
}

#[cfg(not(windows))]
pub(super) async fn service_stop() -> Result<()> {
    bail!("service stop is only supported on Windows")
}

#[cfg(windows)]
pub(super) async fn service_uninstall() -> Result<()> {
    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
    }
    service.delete()?;
    println!(
        "uninstall_requested=true service={} state=delete_pending",
        BOUNDLESS_SERVICE_NAME
    );
    Ok(())
}

#[cfg(not(windows))]
pub(super) async fn service_uninstall() -> Result<()> {
    bail!("service uninstall is only supported on Windows")
}

pub(super) async fn layout_preview(endpoint: &str) -> Result<()> {
    let matrix = fetch_layout_spec(endpoint).await?;
    let peers = list_peer_records(endpoint).await?;
    let local_tokens = fetch_local_layout_tokens(endpoint).await?;
    let grid = parse_layout_matrix(&matrix);

    println!("layout_matrix={matrix}");
    if grid.is_empty() {
        println!("layout grid is empty");
        return Ok(());
    }

    for row in grid {
        let labels = row
            .into_iter()
            .map(|token| preview_label_for_token(&token, &peers, &local_tokens))
            .collect::<Vec<_>>();
        println!("  {}", labels.join(" | "));
    }

    println!("tip: run `boundlessctl layout orient --left <peer> --right <peer>` for quick edits");
    Ok(())
}

pub(super) async fn layout_orient(
    endpoint: &str,
    left: Option<String>,
    right: Option<String>,
    up: Option<String>,
    down: Option<String>,
) -> Result<()> {
    let peers = list_peer_records(endpoint).await?;
    if peers.is_empty() {
        bail!("no peers configured");
    }

    let existing_matrix = fetch_layout_spec(endpoint).await?;
    let local_tokens = fetch_local_layout_tokens(endpoint).await?;
    let existing = extract_orientation_slots(&existing_matrix, &peers, &local_tokens)?;
    ensure_orientation_safe_to_edit(&existing_matrix, &existing, &peers, &local_tokens)?;

    let left_peer = match left {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.left,
    };
    let right_peer = match right {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.right,
    };
    let up_peer = match up {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.up,
    };
    let down_peer = match down {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.down,
    };

    if left_peer.is_none() && right_peer.is_none() && up_peer.is_none() && down_peer.is_none() {
        bail!("no orientation peers selected; provide at least one side");
    }

    let mut unique = std::collections::HashSet::<String>::new();
    for peer_id in [
        left_peer.as_deref(),
        right_peer.as_deref(),
        up_peer.as_deref(),
        down_peer.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !unique.insert(peer_id.to_string()) {
            bail!("each peer can only appear once across left/right/up/down");
        }
    }

    let matrix = build_orientation_matrix(
        left_peer.as_deref(),
        right_peer.as_deref(),
        up_peer.as_deref(),
        down_peer.as_deref(),
    );
    layout_set(endpoint, matrix.clone()).await?;
    println!("layout_matrix={matrix}");
    layout_preview(endpoint).await
}

pub(super) async fn layout_wizard(endpoint: &str) -> Result<()> {
    let peers = list_peer_records(endpoint).await?;
    if peers.is_empty() {
        bail!("no peers configured");
    }

    println!("Layout wizard");
    println!("Peers:");
    for (index, peer) in peers.iter().enumerate() {
        println!(
            "  [{}] name={} peer_id={} connected={}",
            index + 1,
            peer.display_name,
            short_machine_id(&peer.peer_id),
            peer.connected
        );
    }
    println!("Enter index/peer_id/name prefix for each side, or leave blank.");

    let left = prompt_optional("Left peer")?;
    let right = prompt_optional("Right peer")?;
    let up = prompt_optional("Up peer")?;
    let down = prompt_optional("Down peer")?;

    layout_orient(endpoint, left, right, up, down).await
}

#[derive(Serialize)]
struct FeatureListJson {
    schema_version: u32,
    features: std::collections::BTreeMap<String, bool>,
}

fn peer_list_json(peers: &[PeerInfo]) -> Result<String> {
    let peers = peers
        .iter()
        .map(|peer| PeerJson {
            peer_id: &peer.peer_id,
            display_name: &peer.display_name,
            address: &peer.address,
            connected: peer.connected,
            health_state: &peer.health_state,
            health_reason: &peer.health_reason,
            trust_state: &peer.trust_state,
            trusted_since: &peer.trusted_since,
            trust_fingerprint: &peer.trust_fingerprint,
            device_identity: &peer.device_identity,
        })
        .collect();
    serde_json::to_string_pretty(&PeerListJson {
        schema_version: OUTPUT_SCHEMA_VERSION,
        peers,
    })
    .context("serialize peer list")
}

pub(super) async fn feature_list(endpoint: &str, output: OutputFormat) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client.list_features(Empty {}).await?.into_inner();

    let features = response
        .features
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();

    if output == OutputFormat::Json {
        println!("{}", feature_list_json(features)?);
        return Ok(());
    }

    for (name, enabled) in features {
        println!("{name}={enabled}");
    }

    Ok(())
}

pub(super) async fn feature_set(endpoint: &str, name: String, value: ToggleValue) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .set_feature(FeatureSetRequest {
            name,
            enabled: value.as_bool(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn anti_idle_show(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let config = client.get_anti_idle_config(Empty {}).await?.into_inner();
    let status = client.get_anti_idle_status(Empty {}).await?.into_inner();

    println!(
        "enabled={} recent_activity_window_secs={} allow_on_battery={} keep_display_on={} supported={} active={} display_required={} reason={}",
        config.enabled,
        config.recent_activity_window_secs,
        config.allow_on_battery,
        config.keep_display_on,
        status.supported,
        status.active,
        status.display_required,
        status.reason
    );
    Ok(())
}

pub(super) async fn anti_idle_set(
    endpoint: &str,
    enabled: bool,
    window_minutes: u32,
    allow_on_battery: bool,
    keep_display_on: bool,
) -> Result<()> {
    let recent_activity_window_secs = window_minutes.saturating_mul(60);
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .set_anti_idle_config(AntiIdleSetRequest {
            enabled,
            recent_activity_window_secs,
            allow_on_battery,
            keep_display_on,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn file_transfer_config(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let config = client
        .get_file_transfer_config(Empty {})
        .await?
        .into_inner();

    println!(
        "receive_dir={} organize_by_peer={} auto_accept_trusted_peers={} max_file_bytes={}",
        config.receive_dir,
        config.organize_by_peer,
        config.auto_accept_trusted_peers,
        config.max_file_bytes
    );
    Ok(())
}

pub(super) async fn file_transfer_set_receive_dir(
    endpoint: &str,
    path: String,
    organize_by_peer: bool,
    no_organize_by_peer: bool,
    auto_accept_trusted_peers: Option<bool>,
    max_file_bytes: Option<u64>,
) -> Result<()> {
    if organize_by_peer && no_organize_by_peer {
        bail!("--organize-by-peer and --no-organize-by-peer cannot both be set");
    }

    let mut client = connect_control_plane(endpoint).await?;
    let current = client
        .get_file_transfer_config(Empty {})
        .await?
        .into_inner();
    let response = client
        .set_file_transfer_config(FileTransferSetRequest {
            receive_dir: path,
            organize_by_peer: if organize_by_peer {
                true
            } else if no_organize_by_peer {
                false
            } else {
                current.organize_by_peer
            },
            auto_accept_trusted_peers: auto_accept_trusted_peers
                .unwrap_or(current.auto_accept_trusted_peers),
            max_file_bytes: max_file_bytes.unwrap_or(current.max_file_bytes),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn hotkey_set(endpoint: &str, action: String, combo: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .set_hotkey(HotkeySetRequest { action, combo })
        .await?
        .into_inner();
    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_text(
    endpoint: &str,
    peer_id: String,
    text: String,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .send_clipboard_text(SendClipboardTextRequest { peer_id, text })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_image(
    endpoint: &str,
    peer_id: String,
    path: String,
) -> Result<()> {
    let image_bmp = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    validate_bmp_payload(&image_bmp).with_context(|| format!("invalid BMP payload at {path}"))?;

    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .send_clipboard_image(SendClipboardImageRequest { peer_id, image_bmp })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_files(
    endpoint: &str,
    peer_id: String,
    paths: Vec<String>,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let total = paths.len();
    for path in paths {
        let response = client
            .send_file(SendFileRequest {
                peer_id: peer_id.clone(),
                file_path: path.clone(),
            })
            .await?
            .into_inner();

        println!(
            "path={} ok={} message={}",
            path, response.ok, response.message
        );
    }
    if total > 1 {
        println!("queued_files={total}");
    }
    Ok(())
}

pub(super) async fn transport_events(
    endpoint: &str,
    limit: usize,
    kind: Option<&str>,
    exclude_kind: Option<&str>,
    output: OutputFormat,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let mut events = client
        .list_transport_events(Empty {})
        .await?
        .into_inner()
        .events;
    events = select_transport_events(events, limit, kind, exclude_kind);

    if output == OutputFormat::Json {
        println!("{}", transport_events_json(&events)?);
        return Ok(());
    }

    if events.is_empty() {
        println!("no transport events");
        return Ok(());
    }

    for event in events {
        let detail = protected_transport_event_detail(&event);
        println!(
            "{} direction={} kind={} peer_id={} size_bytes={} detail={}",
            event.timestamp,
            escape_event_field(&event.direction),
            escape_event_field(&event.kind),
            escape_event_field(&event.peer_id),
            event.size_bytes,
            escape_event_field(&detail)
        );
    }

    Ok(())
}

fn feature_list_json(features: std::collections::BTreeMap<String, bool>) -> Result<String> {
    serde_json::to_string_pretty(&FeatureListJson {
        schema_version: OUTPUT_SCHEMA_VERSION,
        features,
    })
    .context("serialize feature list")
}

#[derive(Serialize)]
struct TransportEventsJson<'a> {
    schema_version: u32,
    events: Vec<TransportEventJson<'a>>,
}

#[derive(Serialize)]
struct TransportEventJson<'a> {
    timestamp: &'a str,
    direction: &'a str,
    kind: &'a str,
    peer_id: &'a str,
    detail: String,
    size_bytes: u64,
}

fn transport_events_json(events: &[TransportEvent]) -> Result<String> {
    let events = events
        .iter()
        .map(|event| TransportEventJson {
            timestamp: &event.timestamp,
            direction: &event.direction,
            kind: &event.kind,
            peer_id: &event.peer_id,
            detail: protected_transport_event_detail(event),
            size_bytes: event.size_bytes,
        })
        .collect();
    serde_json::to_string_pretty(&TransportEventsJson {
        schema_version: OUTPUT_SCHEMA_VERSION,
        events,
    })
    .context("serialize transport events")
}

fn protected_transport_event_detail(event: &TransportEvent) -> String {
    sanitize_clipboard_event_output_detail(&event.kind, &event.detail)
}

fn select_transport_events(
    events: Vec<TransportEvent>,
    limit: usize,
    kind: Option<&str>,
    exclude_kind: Option<&str>,
) -> Vec<TransportEvent> {
    let mut events = filter_transport_events(events, kind, exclude_kind);
    if limit > 0 && events.len() > limit {
        events = events.split_off(events.len() - limit);
    }
    events
}

fn filter_transport_events(
    events: Vec<TransportEvent>,
    kind: Option<&str>,
    exclude_kind: Option<&str>,
) -> Vec<TransportEvent> {
    let kind = kind.filter(|value| !value.is_empty());
    let exclude_kind = exclude_kind.filter(|value| !value.is_empty());
    events
        .into_iter()
        .filter(|event| {
            kind.is_none_or(|needle| event.kind.contains(needle))
                && !exclude_kind.is_some_and(|needle| event.kind.contains(needle))
        })
        .collect()
}

fn escape_event_field(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

pub(super) async fn input_owner(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client.get_input_owner(Empty {}).await?.into_inner();
    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

fn none_if_empty(value: &str) -> &str {
    value_or_default(value, "none")
}

fn value_or_default<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

fn format_input_status_line(
    runtime: &InputRuntimeStatusReply,
    handoff: &InputHandoffConfigReply,
) -> String {
    format!(
        "owner={} configured_capture_target={} active_capture_target={} lock_active={} lock_supported={} capture_backend_mode={} pending_inject_frames={} pending_inject_high_water={} elevated_injector_state={} elevated_injector_reason={} elevated_injector_signature_trust={} block_screen_corners={} corner_block_px={} relative_mouse={} hide_cursor_at_edge={} draw_cursor_marker={}",
        none_if_empty(&runtime.owner_peer_id),
        none_if_empty(&runtime.configured_capture_target_peer_id),
        none_if_empty(&runtime.active_capture_target_peer_id),
        runtime.lock_active,
        runtime.lock_supported,
        none_if_empty(&runtime.capture_backend_mode),
        runtime.pending_inject_frames,
        runtime.pending_inject_high_water,
        value_or_default(&runtime.elevated_injector_state, "off"),
        value_or_default(&runtime.elevated_injector_reason, "none"),
        value_or_default(&runtime.elevated_injector_signature_trust, "unknown"),
        handoff.block_screen_corners,
        handoff.corner_block_px,
        handoff.relative_mouse,
        handoff.hide_cursor_at_edge,
        handoff.draw_cursor_marker,
    )
}

pub(super) async fn input_status(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let snapshot = client.get_console_snapshot(Empty {}).await?.into_inner();
    let runtime = snapshot.input_runtime.unwrap_or_default();
    let handoff = snapshot.input_handoff_config.unwrap_or_default();
    println!("{}", format_input_status_line(&runtime, &handoff));
    Ok(())
}

pub(super) async fn input_config(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let snapshot = client.get_console_snapshot(Empty {}).await?.into_inner();
    let handoff = snapshot.input_handoff_config.unwrap_or_default();

    println!(
        "block_screen_corners={} corner_block_px={} relative_mouse={} hide_cursor_at_edge={} draw_cursor_marker={}",
        handoff.block_screen_corners,
        handoff.corner_block_px,
        handoff.relative_mouse,
        handoff.hide_cursor_at_edge,
        handoff.draw_cursor_marker,
    );
    Ok(())
}

pub(super) async fn input_set_config(
    endpoint: &str,
    block_screen_corners: Option<bool>,
    corner_block_px: Option<u32>,
    relative_mouse: Option<bool>,
    hide_cursor_at_edge: Option<bool>,
    draw_cursor_marker: Option<bool>,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let snapshot = client.get_console_snapshot(Empty {}).await?.into_inner();
    let current = snapshot.input_handoff_config.unwrap_or_default();
    let response = client
        .set_input_handoff_config(InputHandoffSetRequest {
            block_screen_corners: block_screen_corners.unwrap_or(current.block_screen_corners),
            corner_block_px: corner_block_px.unwrap_or(current.corner_block_px),
            relative_mouse: relative_mouse.unwrap_or(current.relative_mouse),
            hide_cursor_at_edge: hide_cursor_at_edge.unwrap_or(current.hide_cursor_at_edge),
            draw_cursor_marker: draw_cursor_marker.unwrap_or(current.draw_cursor_marker),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn input_capture_target(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .get_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_capture_start(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .set_input_capture_target(InputCaptureTargetRequest { peer_id })
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_capture_stop(endpoint: &str) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .clear_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_send_move(
    endpoint: &str,
    peer_id: String,
    dx: i32,
    dy: i32,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .send_input_move(SendInputMoveRequest { peer_id, dx, dy })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn input_send_key(
    endpoint: &str,
    peer_id: String,
    scan_code: u16,
    state: InputKeyState,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .send_input_key(SendInputKeyRequest {
            peer_id,
            scan_code: scan_code as u32,
            key_down: state.is_down(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn input_claim(endpoint: &str, peer_id: String, force: bool) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .claim_input_owner(InputOwnerRequest { peer_id, force })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

pub(super) async fn input_release(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .release_input_owner(InputOwnerRequest {
            peer_id,
            force: false,
        })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

pub(super) async fn diagnostics_dump(
    endpoint: &str,
    output: Option<String>,
    include_filenames: bool,
    offline: bool,
    open_folder: bool,
) -> Result<()> {
    let response = if offline {
        let bundle = build_offline_bundle(
            env!("CARGO_PKG_VERSION"),
            endpoint,
            collect_service_diagnostics(),
            include_filenames,
            "offline flag requested",
        );
        let export = write_diagnostic_bundle(
            bundle,
            DiagnosticExportOptions {
                output_path: output,
                include_filenames,
            },
        )
        .await?;
        ipc_api::boundless::v1::DiagnosticsDumpReply {
            bundle_path: export.bundle_path,
            manifest_path: export.manifest_path,
            filenames_included: export.filenames_included,
        }
    } else {
        let mut client = connect_control_plane(endpoint).await?;
        client
            .dump_diagnostics(DiagnosticsDumpRequest {
                output_path: output.unwrap_or_default(),
                include_filenames,
            })
            .await?
            .into_inner()
    };

    println!(
        "bundle_path={} manifest_path={} filenames_included={}",
        response.bundle_path, response.manifest_path, response.filenames_included
    );
    if open_folder {
        open_containing_folder(&response.bundle_path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn open_containing_folder(bundle_path: &str) -> Result<()> {
    let path = std::path::Path::new(bundle_path);
    let parent = path
        .parent()
        .context("diagnostic bundle path has no containing folder")?;
    std::process::Command::new("explorer")
        .arg(parent)
        .spawn()
        .context("open diagnostic bundle containing folder")?;
    Ok(())
}

#[cfg(not(windows))]
fn open_containing_folder(bundle_path: &str) -> Result<()> {
    let path = std::path::Path::new(bundle_path);
    let parent = path
        .parent()
        .context("diagnostic bundle path has no containing folder")?;
    println!("containing_folder={}", parent.display());
    Ok(())
}

pub(super) async fn diagnostics_run_action(endpoint: &str, action: String) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .trigger_hotkey_action(HotkeyTriggerRequest { action })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn safe_reset(
    endpoint: &str,
    network_only: bool,
    all: bool,
    confirm: String,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let response = client
        .safe_reset(SafeResetRequest {
            network_only,
            all,
            confirm,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct UiSnapshot {
    generated_at: String,
    daemon_online: bool,
    machine_id: String,
    layout_matrix: String,
    discovered_peers: Vec<UiDiscoveredPeer>,
    paired_peers: Vec<UiPairedPeer>,
    pending_requests: Vec<UiPendingRequest>,
    anti_idle_config: UiAntiIdleConfig,
    anti_idle_status: UiAntiIdleStatus,
}

#[derive(Debug, Clone, Serialize)]
struct UiDiscoveredPeer {
    machine_id: String,
    display_name: String,
    endpoint: String,
    endpoint_candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UiPairedPeer {
    peer_id: String,
    display_name: String,
    address: String,
    connected: bool,
    health_state: String,
    health_reason: String,
    trust_state: String,
    trusted_since: String,
    trust_fingerprint: String,
    device_identity: String,
}

#[derive(Debug, Clone, Serialize)]
struct UiPendingRequest {
    request_id: String,
    requester_machine_id: String,
    requester_display_name: String,
    created_at: String,
    verification_code: String,
    verification_expires_at: String,
    requires_verification_code: bool,
    role: String,
    attempt_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct UiAntiIdleConfig {
    enabled: bool,
    recent_activity_window_secs: u32,
    allow_on_battery: bool,
    keep_display_on: bool,
}

#[derive(Debug, Clone, Serialize)]
struct UiAntiIdleStatus {
    supported: bool,
    enabled: bool,
    active: bool,
    display_required: bool,
    reason: String,
}

pub(super) async fn ui_snapshot(endpoint: &str, start_daemon: bool) -> Result<()> {
    if start_daemon {
        ensure_daemon_available(endpoint, true).await?;
    }

    let mut control_plane = connect_control_plane(endpoint).await?;
    let snapshot = control_plane.get_ui_snapshot(Empty {}).await?.into_inner();
    let snapshot = UiSnapshot {
        generated_at: snapshot.generated_at,
        daemon_online: snapshot.daemon_online,
        machine_id: snapshot.machine_id,
        layout_matrix: snapshot.layout_matrix,
        discovered_peers: snapshot
            .discovered_peers
            .into_iter()
            .map(|peer| UiDiscoveredPeer {
                machine_id: peer.machine_id,
                display_name: peer.display_name,
                endpoint: peer.endpoint,
                endpoint_candidates: peer.endpoint_candidates,
            })
            .collect(),
        paired_peers: snapshot
            .paired_peers
            .into_iter()
            .map(|peer| UiPairedPeer {
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                address: peer.address,
                connected: peer.connected,
                health_state: peer.health_state,
                health_reason: peer.health_reason,
                trust_state: peer.trust_state,
                trusted_since: peer.trusted_since,
                trust_fingerprint: peer.trust_fingerprint,
                device_identity: peer.device_identity,
            })
            .collect(),
        pending_requests: snapshot
            .pending_requests
            .into_iter()
            .map(|request| UiPendingRequest {
                request_id: request.request_id,
                requester_machine_id: request.requester_machine_id,
                requester_display_name: request.requester_display_name,
                created_at: request.created_at,
                verification_code: request.verification_code,
                verification_expires_at: request.verification_expires_at,
                requires_verification_code: request.requires_verification_code,
                role: request.role,
                attempt_id: request.attempt_id,
            })
            .collect(),
        anti_idle_config: snapshot
            .anti_idle_config
            .map(|config| UiAntiIdleConfig {
                enabled: config.enabled,
                recent_activity_window_secs: config.recent_activity_window_secs,
                allow_on_battery: config.allow_on_battery,
                keep_display_on: config.keep_display_on,
            })
            .unwrap_or(UiAntiIdleConfig {
                enabled: false,
                recent_activity_window_secs: 0,
                allow_on_battery: false,
                keep_display_on: false,
            }),
        anti_idle_status: snapshot
            .anti_idle_status
            .map(|status| UiAntiIdleStatus {
                supported: status.supported,
                enabled: status.enabled,
                active: status.active,
                display_required: status.display_required,
                reason: status.reason,
            })
            .unwrap_or(UiAntiIdleStatus {
                supported: false,
                enabled: false,
                active: false,
                display_required: false,
                reason: "none".to_string(),
            }),
    };

    println!(
        "{}",
        serde_json::to_string(&snapshot).context("serialize ui snapshot")?
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct DiscoveredPeerRecord {
    machine_id: String,
    display_name: String,
    endpoint: String,
    endpoint_candidates: Vec<String>,
}

#[derive(Debug, Clone)]
struct PeerRecord {
    peer_id: String,
    display_name: String,
    connected: bool,
}

#[derive(Debug, Clone)]
struct LocalLayoutTokens {
    machine_id: String,
    display_name: String,
}

async fn list_discovered_peer_records(endpoint: &str) -> Result<Vec<DiscoveredPeerRecord>> {
    let snapshot = fetch_ui_snapshot(endpoint).await?;
    let paired_peers = map_peer_records(&snapshot.paired_peers);
    let peers = snapshot
        .discovered_peers
        .into_iter()
        .map(|peer| DiscoveredPeerRecord {
            machine_id: peer.machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint,
            endpoint_candidates: peer.endpoint_candidates,
        })
        .collect::<Vec<_>>();
    Ok(filter_connectable_discovered_peer_records(
        peers,
        &snapshot.machine_id,
        &paired_peers,
    ))
}

async fn list_peer_records(endpoint: &str) -> Result<Vec<PeerRecord>> {
    let snapshot = fetch_ui_snapshot(endpoint).await?;
    Ok(map_peer_records(&snapshot.paired_peers))
}

async fn fetch_layout_spec(endpoint: &str) -> Result<String> {
    let snapshot = fetch_ui_snapshot(endpoint).await?;
    Ok(snapshot.layout_matrix)
}

async fn fetch_local_layout_tokens(endpoint: &str) -> Result<LocalLayoutTokens> {
    let mut control_plane = connect_control_plane(endpoint).await?;
    let snapshot = control_plane
        .get_console_snapshot(Empty {})
        .await?
        .into_inner();
    let status = snapshot
        .status
        .ok_or_else(|| anyhow::anyhow!("console snapshot missing status payload"))?;

    Ok(LocalLayoutTokens {
        machine_id: status.machine_id,
        display_name: snapshot.local_display_name,
    })
}

async fn validate_layout_for_endpoint(endpoint: &str, matrix: &str) -> Result<()> {
    let peers = list_peer_records(endpoint).await?;
    let local_tokens = fetch_local_layout_tokens(endpoint).await?;
    let peer_tokens = peers
        .iter()
        .map(|peer| LayoutPeerToken {
            peer_id: peer.peer_id.clone(),
            display_name: peer.display_name.clone(),
        })
        .collect::<Vec<_>>();

    validate_layout_matrix_spec(
        matrix,
        &local_tokens.machine_id,
        Some(local_tokens.display_name.as_str()),
        &peer_tokens,
    )
}

async fn fetch_ui_snapshot(endpoint: &str) -> Result<UiSnapshotReply> {
    let mut control_plane = connect_control_plane(endpoint).await?;
    let snapshot = control_plane.get_ui_snapshot(Empty {}).await?.into_inner();
    Ok(snapshot)
}

fn map_peer_records(peers: &[ipc_api::boundless::v1::PeerInfo]) -> Vec<PeerRecord> {
    let mut peers = peers
        .iter()
        .map(|peer| PeerRecord {
            peer_id: peer.peer_id.clone(),
            display_name: peer.display_name.clone(),
            connected: peer.connected,
        })
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| {
                a.display_name
                    .to_ascii_lowercase()
                    .cmp(&b.display_name.to_ascii_lowercase())
            })
            .then_with(|| a.peer_id.cmp(&b.peer_id))
    });
    peers
}

fn resolve_discovered_peer_record<'a>(
    peers: &'a [DiscoveredPeerRecord],
    selector: &str,
) -> Result<&'a DiscoveredPeerRecord> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 {
            bail!("selector index must start at 1");
        }
        return peers
            .get(index - 1)
            .ok_or_else(|| anyhow::anyhow!("no discovered peer at index {index}"));
    }

    let normalized = selector.trim();
    if normalized.is_empty() {
        bail!("selector must not be empty");
    }
    let selector_lower = normalized.to_ascii_lowercase();

    let matches = peers
        .iter()
        .filter(|peer| {
            peer.machine_id.eq_ignore_ascii_case(normalized)
                || peer
                    .machine_id
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
                || peer.display_name.eq_ignore_ascii_case(normalized)
                || peer
                    .display_name
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        bail!("no discovered peer matching `{selector}`");
    }
    if matches.len() > 1 {
        bail!("multiple discovered peers match `{selector}`; use an index");
    }
    Ok(matches[0])
}

fn resolve_peer_selector_opt(
    peers: &[PeerRecord],
    selector: Option<&str>,
) -> Result<Option<String>> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let peer = resolve_peer_selector(peers, trimmed)?;
    Ok(Some(peer.peer_id.clone()))
}

fn resolve_peer_selector<'a>(peers: &'a [PeerRecord], selector: &str) -> Result<&'a PeerRecord> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 {
            bail!("peer index must start at 1");
        }
        return peers
            .get(index - 1)
            .ok_or_else(|| anyhow::anyhow!("no peer at index {index}"));
    }

    let selector_lower = selector.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(selector)
                || peer
                    .peer_id
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
                || peer.display_name.eq_ignore_ascii_case(selector)
                || peer
                    .display_name
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("no peer matching `{selector}`");
    }
    if matches.len() > 1 {
        bail!("multiple peers match `{selector}`; use index or full peer_id");
    }
    Ok(matches[0])
}

#[derive(Debug, Default)]
struct OrientationSlots {
    left: Option<String>,
    right: Option<String>,
    up: Option<String>,
    down: Option<String>,
}

fn extract_orientation_slots(
    matrix: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<OrientationSlots> {
    let grid = parse_layout_matrix(matrix);
    if grid.is_empty() {
        return Ok(OrientationSlots::default());
    }

    let mut local_cell: Option<(usize, usize)> = None;
    for (row_index, row) in grid.iter().enumerate() {
        for (column_index, token) in row.iter().enumerate() {
            if is_local_layout_token_shared(
                token,
                &local_tokens.machine_id,
                Some(local_tokens.display_name.as_str()),
            ) {
                if local_cell.is_some() {
                    bail!("layout has multiple local cells; cannot safely orient");
                }
                local_cell = Some((row_index, column_index));
            }
        }
    }
    let Some((row, column)) = local_cell else {
        bail!("layout has no local cell; cannot safely orient");
    };

    let token_at = |row_index: usize, column_index: usize| -> Option<&str> {
        grid.get(row_index)
            .and_then(|tokens| tokens.get(column_index))
            .map(String::as_str)
    };

    let mut slots = OrientationSlots::default();
    for next_column in (0..column).rev() {
        let Some(token) = token_at(row, next_column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.left = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    let width = grid.get(row).map(|tokens| tokens.len()).unwrap_or(0);
    for next_column in (column + 1)..width {
        let Some(token) = token_at(row, next_column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.right = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    for next_row in (0..row).rev() {
        let Some(token) = token_at(next_row, column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.up = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    for next_row in (row + 1)..grid.len() {
        let Some(token) = token_at(next_row, column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.down = Some(peer_id);
                break;
            }
            None => continue,
        }
    }

    Ok(slots)
}

fn ensure_orientation_safe_to_edit(
    matrix: &str,
    slots: &OrientationSlots,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<()> {
    let matrix_peer_ids = collect_matrix_peer_ids(matrix, peers, local_tokens)?;
    let slot_peer_ids = [
        slots.left.as_deref(),
        slots.right.as_deref(),
        slots.up.as_deref(),
        slots.down.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(ToString::to_string)
    .collect::<std::collections::HashSet<_>>();

    let hidden = matrix_peer_ids
        .difference(&slot_peer_ids)
        .cloned()
        .collect::<Vec<_>>();
    if !hidden.is_empty() {
        bail!(
            "current layout contains peers beyond immediate left/right/up/down; `layout orient` would drop them. Use `layout set` for complex topologies."
        );
    }
    Ok(())
}

fn collect_matrix_peer_ids(
    matrix: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::<String>::new();
    for row in parse_layout_matrix(matrix) {
        for token in row {
            if let Some(peer_id) = resolve_matrix_peer_token(&token, peers, local_tokens)? {
                ids.insert(peer_id);
            }
        }
    }
    Ok(ids)
}

fn resolve_matrix_peer_token(
    token: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<Option<String>> {
    let trimmed = token.trim();
    if trimmed.is_empty()
        || is_local_layout_token_shared(
            trimmed,
            &local_tokens.machine_id,
            Some(local_tokens.display_name.as_str()),
        )
    {
        return Ok(None);
    }

    let token_lower = trimmed.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(trimmed)
                || peer.peer_id.to_ascii_lowercase().starts_with(&token_lower)
                || peer.display_name.eq_ignore_ascii_case(trimmed)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("layout token `{trimmed}` does not resolve to a known peer");
    }
    if matches.len() > 1 {
        bail!("layout token `{trimmed}` is ambiguous across peers");
    }
    Ok(Some(matches[0].peer_id.clone()))
}

fn preview_label_for_token(
    token: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> String {
    if token.trim().is_empty() {
        return ".".to_string();
    }
    if is_local_layout_token_shared(
        token,
        &local_tokens.machine_id,
        Some(local_tokens.display_name.as_str()),
    ) {
        return "THIS-PC".to_string();
    }

    if let Some(peer) = peers.iter().find(|peer| {
        peer.peer_id.eq_ignore_ascii_case(token) || peer.display_name.eq_ignore_ascii_case(token)
    }) {
        return format!(
            "{}{}",
            peer.display_name,
            if peer.connected { "" } else { " (offline)" }
        );
    }

    token.to_string()
}

pub(super) fn host_and_pairing_port_from_discovery_endpoint(
    endpoint: &str,
) -> Result<(String, u16)> {
    host_and_pairing_port_from_endpoint(endpoint)
}

fn find_new_peer_record(before: &[PeerRecord], after: &[PeerRecord]) -> Option<PeerRecord> {
    let before_ids = before
        .iter()
        .map(|peer| peer.peer_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    after
        .iter()
        .find(|peer| !before_ids.contains(peer.peer_id.as_str()))
        .cloned()
}

fn prompt_required(label: &str) -> Result<String> {
    loop {
        print!("{label}: ");
        io::stdout().flush().context("flush stdout")?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label}"))?;
        let trimmed = line.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
        println!("{label} is required");
    }
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    print!("{label}: ");
    io::stdout().flush().context("flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .with_context(|| format!("read {label}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn prompt_optional_with_default(label: &str, default: Option<&str>) -> Result<Option<String>> {
    if let Some(default) = default {
        print!("{label} [{default}]: ");
    } else {
        print!("{label}: ");
    }
    io::stdout().flush().context("flush stdout")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .with_context(|| format!("read {label}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default.map(|value| value.to_string()));
    }
    Ok(Some(trimmed.to_string()))
}

fn prompt_u16_with_default(label: &str, default: u16) -> Result<u16> {
    loop {
        print!("{label} [{default}]: ");
        io::stdout().flush().context("flush stdout")?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        let value = trimmed
            .parse::<u16>()
            .with_context(|| format!("{label} must be a valid port in range 0..=65535"))?;
        if value == 0 {
            println!("{label} must be greater than 0");
            continue;
        }
        return Ok(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_status_output_includes_elevated_injector_status() {
        let runtime = InputRuntimeStatusReply {
            elevated_injector_state: "active".to_string(),
            elevated_injector_reason: "none".to_string(),
            elevated_injector_signature_trust: "unsigned_dogfood".to_string(),
            ..Default::default()
        };
        let line = format_input_status_line(&runtime, &InputHandoffConfigReply::default());

        assert!(line.contains("elevated_injector_state=active"));
        assert!(line.contains("elevated_injector_reason=none"));
        assert!(line.contains("elevated_injector_signature_trust=unsigned_dogfood"));

        let default_line = format_input_status_line(
            &InputRuntimeStatusReply::default(),
            &InputHandoffConfigReply::default(),
        );
        assert!(default_line.contains("elevated_injector_state=off"));
        assert!(default_line.contains("elevated_injector_reason=none"));
        assert!(default_line.contains("elevated_injector_signature_trust=unknown"));
    }

    fn local_tokens() -> LocalLayoutTokens {
        LocalLayoutTokens {
            machine_id: "local-machine".to_string(),
            display_name: "local-device".to_string(),
        }
    }

    #[test]
    fn daemon_status_output_matches_packaging_fixture() {
        let status = StatusReply {
            running: true,
            daemon_version: "5.0.0".to_string(),
            machine_id: "4f0c6bce-6c10-4df9-b8b5-3a9a3fbb5da1".to_string(),
            peer_count: 1,
            protocol_version: "4.2.0".to_string(),
            api_transport: "npipe".to_string(),
            api_bind: String::new(),
            api_pipe_name: "boundlessd-api".to_string(),
            input_locked: false,
            input_lock_supported: true,
            capture_target_peer_id: String::new(),
            anti_idle_supported: true,
            anti_idle_enabled: true,
            anti_idle_active: false,
            anti_idle_display_required: false,
        };
        let fixture =
            include_str!("../../../packaging/windows/fixtures/daemon-status-single-line.txt")
                .trim();

        assert_eq!(format_daemon_status_line(&status), fixture);
    }

    #[test]
    fn daemon_status_json_uses_stable_schema_fields() {
        let status = StatusReply {
            daemon_version: "5.0.16".to_string(),
            running: true,
            machine_id: "machine-a".to_string(),
            peer_count: 2,
            protocol_version: "4.4.0".to_string(),
            api_bind: "127.0.0.1:15100".to_string(),
            api_transport: "npipe".to_string(),
            api_pipe_name: "boundlessd-api".to_string(),
            input_locked: true,
            input_lock_supported: true,
            capture_target_peer_id: String::new(),
            anti_idle_supported: true,
            anti_idle_enabled: true,
            anti_idle_active: false,
            anti_idle_display_required: false,
        };
        let value: serde_json::Value =
            serde_json::from_str(&daemon_status_json(&status).expect("serialize status"))
                .expect("parse status JSON");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["machine_id"], "machine-a");
        assert_eq!(value["peer_count"], 2);
        assert_eq!(value["capture_target_peer_id"], "");
        assert_eq!(value.as_object().expect("object").len(), 16);
    }

    #[test]
    fn peer_and_feature_json_preserve_full_stable_shapes() {
        let peer = PeerInfo {
            peer_id: "peer-a".to_string(),
            display_name: "Desk".to_string(),
            address: "10.0.0.2:15100".to_string(),
            connected: true,
            health_state: "healthy".to_string(),
            health_reason: "connected".to_string(),
            trust_state: "trusted".to_string(),
            trusted_since: "2026-07-14T00:00:00Z".to_string(),
            trust_fingerprint: "sha256:abc".to_string(),
            device_identity: "device-a".to_string(),
        };
        let peers: serde_json::Value =
            serde_json::from_str(&peer_list_json(&[peer]).expect("serialize peers"))
                .expect("parse peers JSON");
        assert_eq!(peers["schema_version"], 1);
        assert_eq!(peers["peers"][0]["health_reason"], "connected");
        assert_eq!(peers["peers"][0]["device_identity"], "device-a");
        assert_eq!(
            peers["peers"][0].as_object().expect("peer object").len(),
            10
        );

        let features = std::collections::BTreeMap::from([
            ("file_transfer".to_string(), false),
            ("clipboard".to_string(), true),
        ]);
        let rendered = feature_list_json(features).expect("serialize features");
        let clipboard = rendered.find("clipboard").expect("clipboard field");
        let file_transfer = rendered.find("file_transfer").expect("file field");
        assert!(
            clipboard < file_transfer,
            "feature keys must be stable and sorted"
        );
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("parse features");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["features"]["clipboard"], true);
    }

    #[test]
    fn empty_json_collections_are_arrays_and_objects() {
        let peers: serde_json::Value =
            serde_json::from_str(&peer_list_json(&[]).expect("empty peers"))
                .expect("parse empty peers");
        assert_eq!(peers["peers"], serde_json::json!([]));

        let features: serde_json::Value = serde_json::from_str(
            &feature_list_json(std::collections::BTreeMap::new()).expect("empty features"),
        )
        .expect("parse empty features");
        assert_eq!(features["features"], serde_json::json!({}));

        let events: serde_json::Value =
            serde_json::from_str(&transport_events_json(&[]).expect("empty events"))
                .expect("parse empty events");
        assert_eq!(events["events"], serde_json::json!([]));
    }

    #[test]
    fn transport_event_json_keeps_clipboard_detail_protected() {
        let event = test_transport_event(
            "clipboard_text_sent",
            "secret clipboard text that must not be emitted",
        );
        let value: serde_json::Value =
            serde_json::from_str(&transport_events_json(&[event]).expect("serialize events"))
                .expect("parse events JSON");
        let detail = value["events"][0]["detail"].as_str().expect("detail");
        assert!(!detail.contains("secret"));
        assert_eq!(value["schema_version"], 1);
        assert_eq!(
            value["events"][0].as_object().expect("event object").len(),
            6
        );
    }

    fn test_transport_event(kind: &str, detail: &str) -> TransportEvent {
        TransportEvent {
            timestamp: "2026-07-08T00:00:00Z".to_string(),
            direction: "local".to_string(),
            kind: kind.to_string(),
            peer_id: "peer-a".to_string(),
            detail: detail.to_string(),
            size_bytes: 0,
        }
    }

    #[test]
    fn transport_event_filters_match_kind_substrings() {
        let events = vec![
            test_transport_event("input_runtime_wake", "source=retry_deadline"),
            test_transport_event("clipboard_text", "payload"),
            test_transport_event("anti_idle_local_state", "active=true"),
        ];

        let selected = select_transport_events(events, 0, Some("clipboard"), None);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].kind, "clipboard_text");
    }

    #[test]
    fn transport_event_filters_exclude_kind_substrings_before_limit() {
        let events = vec![
            test_transport_event("clipboard_text", "retained"),
            test_transport_event("input_runtime_wake", "source=retry_deadline"),
        ];

        let selected = select_transport_events(events, 1, None, Some("input_runtime"));

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].kind, "clipboard_text");
        assert_eq!(selected[0].detail, "retained");
    }

    #[test]
    fn clipboard_event_cli_detail_fails_closed() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_77c3c7ea";
        let event = test_transport_event(
            "clipboard_image_rejected",
            &format!(
                "payload_type=bmp disposition=rejected reason=hash_mismatch sample_count=4 first_seen=2026-07-10T00:00:00Z last_seen=2026-07-10T00:00:01Z expected={SECRET} actual={SECRET}"
            ),
        );

        let detail = protected_transport_event_detail(&event);

        assert_eq!(
            detail,
            "payload_type=bmp disposition=rejected reason=hash_mismatch sample_count=4 first_seen=2026-07-10T00:00:00Z last_seen=2026-07-10T00:00:01Z"
        );
        assert!(!detail.contains(SECRET));
    }

    #[test]
    fn transport_event_filters_aggregates_before_limit() {
        let events = vec![
            test_transport_event("input_handoff", "direction=Left"),
            test_transport_event(
                "input_frame",
                "sequence=4000 sample_count=4000 first_seen=one last_seen=two",
            ),
            test_transport_event("runtime_wake", "sample_count=4000"),
        ];

        let selected = select_transport_events(events, 1, Some("input_frame"), None);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].kind, "input_frame");
        assert!(selected[0].detail.contains("sample_count=4000"));
    }

    #[test]
    fn build_orientation_matrix_builds_cross_layout() {
        let matrix = build_orientation_matrix(
            Some("peer-left"),
            Some("peer-right"),
            Some("peer-up"),
            Some("peer-down"),
        );
        assert_eq!(matrix, ",peer-up,;peer-left,self,peer-right;,peer-down,");
    }

    #[test]
    fn resolve_discovered_peer_record_supports_display_name_prefix() {
        let peers = vec![
            DiscoveredPeerRecord {
                machine_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                display_name: "office-desktop".to_string(),
                endpoint: "10.0.0.10:15100".to_string(),
                endpoint_candidates: Vec::new(),
            },
            DiscoveredPeerRecord {
                machine_id: "11111111-2222-3333-4444-555555555555".to_string(),
                display_name: "laptop".to_string(),
                endpoint: "10.0.0.11:15100".to_string(),
                endpoint_candidates: Vec::new(),
            },
        ];

        let selected = resolve_discovered_peer_record(&peers, "office").expect("resolve prefix");
        assert_eq!(selected.display_name, "office-desktop");
    }

    #[test]
    fn discovered_peer_reachability_summaries_are_redacted_and_actionable() {
        let peer = DiscoveredPeerRecord {
            machine_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            display_name: "office-desktop".to_string(),
            endpoint: "10.0.0.10:15100".to_string(),
            endpoint_candidates: vec![
                "[fe80::1%4]:15100".to_string(),
                "10.0.0.10:15100".to_string(),
            ],
        };

        let transport = redacted_transport_candidates(&peer);
        let pairing = redacted_pairing_candidates(&peer);

        assert_eq!(
            transport,
            "source=mdns tcp ipv6 port 15100, source=mdns tcp ipv4 port 15100"
        );
        assert_eq!(
            pairing,
            "source=mdns tcp ipv6 port 15200, source=mdns tcp ipv4 port 15200"
        );
        assert!(!transport.contains("10.0.0.10"));
        assert!(!pairing.contains("fe80::1"));
    }

    #[test]
    fn filter_connectable_discovered_peer_records_hides_local_and_paired_peers() {
        let discovered = vec![
            DiscoveredPeerRecord {
                machine_id: "local-machine".to_string(),
                display_name: "This PC".to_string(),
                endpoint: "10.0.0.1:15100".to_string(),
                endpoint_candidates: Vec::new(),
            },
            DiscoveredPeerRecord {
                machine_id: "paired-machine".to_string(),
                display_name: "Different Alias".to_string(),
                endpoint: "10.0.0.2:15100".to_string(),
                endpoint_candidates: Vec::new(),
            },
            DiscoveredPeerRecord {
                machine_id: "brand-new-machine".to_string(),
                display_name: "Office Desktop".to_string(),
                endpoint: "10.0.0.3:15100".to_string(),
                endpoint_candidates: Vec::new(),
            },
        ];
        let paired = vec![PeerRecord {
            peer_id: "paired-machine".to_string(),
            display_name: "Stored Alias".to_string(),
            connected: true,
        }];

        let filtered =
            filter_connectable_discovered_peer_records(discovered, "LOCAL-MACHINE", &paired);

        assert_eq!(
            filtered.len(),
            1,
            "only the new peer should remain connectable"
        );
        assert_eq!(filtered[0].machine_id, "brand-new-machine");
    }

    #[test]
    fn host_and_pairing_port_parses_hostname_endpoint() {
        let (host, port) =
            host_and_pairing_port_from_endpoint("DESKTOP-ABC:15100").expect("parse endpoint");
        assert_eq!(host, "DESKTOP-ABC");
        assert_eq!(port, 15200);
    }

    #[test]
    fn extract_service_executable_path_handles_quoted_path_with_args() {
        let path = extract_service_executable_path(
            r#""C:\Program Files\Boundless\boundless-service.exe" --allowed-user-sid=S-1-5-21-1"#,
        );

        assert_eq!(
            path,
            StdPathBuf::from(r"C:\Program Files\Boundless\boundless-service.exe")
        );
    }

    #[test]
    fn extract_service_executable_path_handles_unquoted_path_with_args() {
        let path = extract_service_executable_path(
            r"C:\Tools\Boundless\boundless-service.exe --allowed-user-sid=S-1-5-21-1",
        );

        assert_eq!(
            path,
            StdPathBuf::from(r"C:\Tools\Boundless\boundless-service.exe")
        );
    }

    #[test]
    fn service_version_parity_reports_match_mismatch_and_unknown() {
        assert_eq!(service_version_parity(Some("v5.0.0"), "5.0.0"), "matched");
        assert_eq!(service_version_parity(Some("4.0.2"), "5.0.0"), "mismatched");
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

    #[test]
    fn ensure_orientation_safe_to_edit_rejects_hidden_peer_chain() {
        let peers = vec![
            PeerRecord {
                peer_id: "left-a".to_string(),
                display_name: "left-a".to_string(),
                connected: true,
            },
            PeerRecord {
                peer_id: "left-b".to_string(),
                display_name: "left-b".to_string(),
                connected: true,
            },
        ];
        let local = local_tokens();
        let slots =
            extract_orientation_slots("left-a,left-b,self", &peers, &local).expect("parse slots");
        let err = ensure_orientation_safe_to_edit("left-a,left-b,self", &slots, &peers, &local)
            .expect_err("must reject hidden peer");
        assert!(err.to_string().contains("would drop"));
    }

    #[test]
    fn extract_orientation_slots_recognizes_machine_id_local_token() {
        let peers = vec![PeerRecord {
            peer_id: "peer-right".to_string(),
            display_name: "peer-right".to_string(),
            connected: true,
        }];
        let local = local_tokens();
        let slots = extract_orientation_slots("local-machine,peer-right", &peers, &local)
            .expect("parse slots");
        assert_eq!(slots.right.as_deref(), Some("peer-right"));
    }
}
