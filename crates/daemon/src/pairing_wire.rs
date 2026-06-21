use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use anyhow::{Context, Result};
use core_security::TrustBundle;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    time,
};
use tracing::{info, warn};

use crate::{
    network::bind_dual_stack_tcp_listeners,
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::{AppState, NearbyPairingStatus},
};

const PAIRING_PORT_OFFSET: u16 = 100;
const PAIRING_IO_TIMEOUT: Duration = Duration::from_secs(10);
const NEARBY_CHALLENGE_TTL_SECONDS: u64 = 120;
const NEARBY_PAIRING_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const NEARBY_PAIRING_IO_TIMEOUT: Duration = Duration::from_secs(6);
const NEARBY_PAIRING_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PairingWireRequest {
    NearbyRequestCode {
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
    },
    NearbySubmitCode {
        request_id: String,
        code: String,
        verification_nonce: String,
        requester_alias: Option<String>,
    },
    NearbyJoin {
        code: String,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
    },
    CheckNearbyJoin {
        request_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PairingWireResponse {
    CodeRequired {
        request_id: String,
        message: String,
        verification_nonce: String,
        expires_at: String,
    },
    Pending {
        request_id: String,
        message: String,
    },
    Approved {
        request_id: String,
        message: String,
        responder_bundle: TrustBundle,
    },
    Rejected {
        request_id: String,
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NearbyJoinStatusKind {
    Pending,
    Approved,
    Rejected,
    Error,
    CodeRequired,
}

impl NearbyJoinStatusKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            NearbyJoinStatusKind::Pending => "pending",
            NearbyJoinStatusKind::Approved => "approved",
            NearbyJoinStatusKind::Rejected => "rejected",
            NearbyJoinStatusKind::Error => "error",
            NearbyJoinStatusKind::CodeRequired => "code_required",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum NearbyRequestCodeStart {
    CodeRequired {
        request_id: String,
        verification_nonce: String,
        expires_at: String,
    },
    Unsupported {
        reason: String,
    },
}

pub(crate) struct NearbySubmitCode {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) request_id: String,
    pub(crate) code: String,
    pub(crate) verification_nonce: String,
    pub(crate) alias: Option<String>,
    pub(crate) endpoint_candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct NearbyJoinStatus {
    pub(crate) request_id: String,
    pub(crate) status: NearbyJoinStatusKind,
    pub(crate) message: String,
    pub(crate) peer_machine_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct NearbyPairingOutcome {
    pub(crate) peer_machine_id: String,
    pub(crate) trust_committed: bool,
    pub(crate) already_committed: bool,
    pub(crate) reconnect_status: String,
    pub(crate) message: String,
}

pub(crate) async fn request_nearby_pairing_code(
    state: &AppState,
    host: &str,
    port: u16,
    alias: Option<String>,
    endpoint_candidates: &[String],
) -> Result<NearbyRequestCodeStart> {
    let targets = nearby_pairing_targets(host, port, endpoint_candidates);
    let requester_bundle = state.export_trust_bundle().await?;
    let (response, _) = send_nearby_pairing_request_to_candidates(
        &targets,
        PairingWireRequest::NearbyRequestCode {
            requester_bundle,
            requester_alias: normalize_alias(alias),
        },
    )
    .await?;

    match response {
        PairingWireResponse::CodeRequired {
            request_id,
            verification_nonce,
            expires_at,
            ..
        } => Ok(NearbyRequestCodeStart::CodeRequired {
            request_id,
            verification_nonce,
            expires_at,
        }),
        PairingWireResponse::Error { message } => {
            let lowered = message.to_ascii_lowercase();
            if lowered.contains("unknown variant")
                || lowered.contains("parse pairing request")
                || lowered.contains("missing field")
            {
                return Ok(NearbyRequestCodeStart::Unsupported { reason: message });
            }
            anyhow::bail!("nearby pairing request failed: {message}");
        }
        PairingWireResponse::Rejected { message, .. } => {
            anyhow::bail!("nearby pairing request rejected: {message}");
        }
        PairingWireResponse::Pending { message, .. } => {
            anyhow::bail!("unexpected nearby pairing status: {message}");
        }
        PairingWireResponse::Approved { .. } => {
            anyhow::bail!("unexpected nearby pairing status: approved");
        }
    }
}

pub(crate) async fn submit_nearby_pairing_code(
    state: &AppState,
    request: NearbySubmitCode,
) -> Result<NearbyPairingOutcome> {
    let targets = nearby_pairing_targets(&request.host, request.port, &request.endpoint_candidates);
    let (response, connected_host) = send_nearby_pairing_request_to_candidates(
        &targets,
        PairingWireRequest::NearbySubmitCode {
            request_id: request.request_id.clone(),
            code: request.code,
            verification_nonce: request.verification_nonce,
            requester_alias: normalize_alias(request.alias.clone()),
        },
    )
    .await?;

    let (responder_bundle, response_message) = match response {
        PairingWireResponse::Approved {
            request_id: approved_request_id,
            message,
            responder_bundle,
            ..
        } => {
            if approved_request_id != request.request_id {
                anyhow::bail!("nearby pairing request id mismatch");
            }
            (responder_bundle, message)
        }
        PairingWireResponse::Pending { .. } => {
            anyhow::bail!(
                "unexpected pending response for code submission; start a new pairing request"
            );
        }
        PairingWireResponse::Rejected { message, .. } => {
            anyhow::bail!("nearby pairing rejected: {message}");
        }
        PairingWireResponse::Error { message } => {
            anyhow::bail!("nearby pairing failed: {message}");
        }
        PairingWireResponse::CodeRequired { message, .. } => {
            anyhow::bail!("nearby pairing failed: {message}");
        }
    };

    let mut outcome =
        import_nearby_responder_bundle(state, responder_bundle, &connected_host, request.alias)
            .await?;
    if response_message.contains("already trusted") && !outcome.already_committed {
        outcome.message = format!("{response_message}; local {}", outcome.message);
    }
    Ok(outcome)
}

pub(crate) async fn start_nearby_pairing_join(
    state: &AppState,
    host: &str,
    port: u16,
    code: String,
    alias: Option<String>,
    endpoint_candidates: &[String],
) -> Result<NearbyJoinStatus> {
    let normalized_code = code.trim().to_string();
    if normalized_code.is_empty() {
        anyhow::bail!("pairing code must not be empty");
    }

    let targets = nearby_pairing_targets(host, port, endpoint_candidates);
    let requester_bundle = state.export_trust_bundle().await?;
    let (response, connected_host) = send_nearby_pairing_request_to_candidates(
        &targets,
        PairingWireRequest::NearbyJoin {
            code: normalized_code,
            requester_bundle,
            requester_alias: normalize_alias(alias.clone()),
        },
    )
    .await?;

    map_join_status_response(state, &connected_host, response, alias).await
}

pub(crate) async fn check_nearby_pairing_join(
    state: &AppState,
    host: &str,
    port: u16,
    request_id: String,
    alias: Option<String>,
    endpoint_candidates: &[String],
) -> Result<NearbyJoinStatus> {
    let trimmed_request_id = request_id.trim().to_string();
    if trimmed_request_id.is_empty() {
        anyhow::bail!("request_id must not be empty");
    }

    let targets = nearby_pairing_targets(host, port, endpoint_candidates);
    let (response, connected_host) = send_nearby_pairing_request_to_candidates(
        &targets,
        PairingWireRequest::CheckNearbyJoin {
            request_id: trimmed_request_id,
        },
    )
    .await?;

    map_join_status_response(state, &connected_host, response, alias).await
}

pub fn start(state: AppState) {
    let task_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "pairing.listener",
            RuntimeTaskOwner::Pairing,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        async move {
            if let Err(error) = run(task_state).await {
                warn!(error = ?error, "pairing listener stopped");
            }
        },
    );
}

async fn map_join_status_response(
    state: &AppState,
    host: &str,
    response: PairingWireResponse,
    alias: Option<String>,
) -> Result<NearbyJoinStatus> {
    match response {
        PairingWireResponse::Pending {
            request_id,
            message,
        } => Ok(NearbyJoinStatus {
            request_id,
            status: NearbyJoinStatusKind::Pending,
            message,
            peer_machine_id: None,
        }),
        PairingWireResponse::Approved {
            request_id,
            message,
            responder_bundle,
        } => {
            let outcome =
                import_nearby_responder_bundle(state, responder_bundle, host, alias).await?;
            Ok(NearbyJoinStatus {
                request_id,
                status: NearbyJoinStatusKind::Approved,
                message: format!("{message}; local {}", outcome.message),
                peer_machine_id: Some(outcome.peer_machine_id),
            })
        }
        PairingWireResponse::Rejected {
            request_id,
            message,
        } => Ok(NearbyJoinStatus {
            request_id,
            status: NearbyJoinStatusKind::Rejected,
            message,
            peer_machine_id: None,
        }),
        PairingWireResponse::Error { message } => Ok(NearbyJoinStatus {
            request_id: String::new(),
            status: NearbyJoinStatusKind::Error,
            message,
            peer_machine_id: None,
        }),
        PairingWireResponse::CodeRequired {
            request_id,
            message,
            ..
        } => Ok(NearbyJoinStatus {
            request_id,
            status: NearbyJoinStatusKind::CodeRequired,
            message,
            peer_machine_id: None,
        }),
    }
}

async fn import_nearby_responder_bundle(
    state: &AppState,
    mut responder_bundle: TrustBundle,
    host: &str,
    alias: Option<String>,
) -> Result<NearbyPairingOutcome> {
    normalize_bundle_address_for_host(&mut responder_bundle, host)?;
    let machine_id = responder_bundle.machine_id.clone();
    let already_committed = state.get_peer(&machine_id).await.is_some();

    state
        .import_trust_bundle(responder_bundle, normalize_alias(alias))
        .await?;
    let reconnect_status = request_pairing_reconnect_after_import(state, &machine_id).await;

    Ok(NearbyPairingOutcome {
        peer_machine_id: machine_id,
        trust_committed: true,
        already_committed,
        message: pairing_import_message(already_committed, &reconnect_status),
        reconnect_status,
    })
}

async fn request_pairing_reconnect_after_import(state: &AppState, peer_id: &str) -> String {
    match state.request_peer_reconnect_and_reset(peer_id).await {
        Ok((generation, aborted_sessions)) => {
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "local".to_string(),
                kind: "pairing_connectivity_pending".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!(
                    "trust_committed=true generation={generation} aborted_sessions={aborted_sessions}"
                ),
                size_bytes: 0,
            });
            "connectivity_pending".to_string()
        }
        Err(error) => {
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "local".to_string(),
                kind: "pairing_reconnect_failed".to_string(),
                peer_id: peer_id.to_string(),
                detail: format!("trust_committed=true error={error}"),
                size_bytes: 0,
            });
            "reconnect_failed".to_string()
        }
    }
}

fn pairing_import_message(already_committed: bool, reconnect_status: &str) -> String {
    let trust = if already_committed {
        "nearby pairing already trusted"
    } else {
        "nearby pairing trust established"
    };
    match reconnect_status {
        "reconnect_failed" => {
            format!("{trust}; reconnect request failed; use reconnect or remove peer to re-pair")
        }
        "connectivity_pending" => format!("{trust}; connectivity pending"),
        _ => trust.to_string(),
    }
}

async fn send_nearby_pairing_request(
    target: &str,
    request: PairingWireRequest,
) -> Result<PairingWireResponse> {
    let mut socket = time::timeout(NEARBY_PAIRING_CONNECT_TIMEOUT, TcpStream::connect(target))
        .await
        .with_context(|| {
            nearby_pairing_connect_timeout_message(target, NEARBY_PAIRING_CONNECT_TIMEOUT.as_secs())
        })?
        .with_context(|| format!("connect nearby pairing endpoint {target}"))?;
    let payload = serde_json::to_string(&request).context("serialize nearby pairing request")?;
    time::timeout(
        NEARBY_PAIRING_IO_TIMEOUT,
        socket.write_all(payload.as_bytes()),
    )
    .await
    .with_context(|| {
        format!(
            "send nearby pairing request timed out after {}s",
            NEARBY_PAIRING_IO_TIMEOUT.as_secs()
        )
    })?
    .context("send nearby pairing request")?;
    time::timeout(NEARBY_PAIRING_IO_TIMEOUT, socket.write_all(b"\n"))
        .await
        .with_context(|| {
            format!(
                "terminate nearby pairing request timed out after {}s",
                NEARBY_PAIRING_IO_TIMEOUT.as_secs()
            )
        })?
        .context("terminate nearby pairing request")?;
    time::timeout(NEARBY_PAIRING_IO_TIMEOUT, socket.flush())
        .await
        .with_context(|| {
            format!(
                "flush nearby pairing request timed out after {}s",
                NEARBY_PAIRING_IO_TIMEOUT.as_secs()
            )
        })?
        .context("flush nearby pairing request")?;

    let mut reader = BufReader::new(socket);
    let mut response_line = String::new();
    let read = time::timeout(
        NEARBY_PAIRING_RESPONSE_TIMEOUT,
        reader.read_line(&mut response_line),
    )
    .await
    .with_context(|| {
        format!(
            "read nearby pairing response timed out after {}s",
            NEARBY_PAIRING_RESPONSE_TIMEOUT.as_secs()
        )
    })?
    .context("read nearby pairing response")?;
    if read == 0 {
        anyhow::bail!("nearby pairing endpoint closed without a response");
    }

    serde_json::from_str(&response_line).context("parse nearby pairing response")
}

async fn send_nearby_pairing_request_to_candidates(
    targets: &[(String, String)],
    request: PairingWireRequest,
) -> Result<(PairingWireResponse, String)> {
    let mut last_error = None;
    for (target, host) in targets {
        match send_nearby_pairing_request(target, request.clone()).await {
            Ok(response) => return Ok((response, host.clone())),
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    let attempted = targets
        .iter()
        .map(|(target, _)| target.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(error) = last_error {
        anyhow::bail!(
            "nearby pairing request failed for all endpoint candidates [{attempted}]: {error:#}"
        );
    }
    anyhow::bail!("nearby pairing request has no endpoint candidates")
}

fn nearby_pairing_targets(
    host: &str,
    port: u16,
    endpoint_candidates: &[String],
) -> Vec<(String, String)> {
    let mut targets = Vec::<(String, String)>::new();
    for endpoint in endpoint_candidates {
        if let Ok((candidate_host, candidate_port)) =
            app_services::desktop::host_and_pairing_port_from_endpoint(endpoint)
        {
            push_pairing_target(&mut targets, candidate_host, candidate_port);
        }
    }
    push_pairing_target(&mut targets, host.trim().to_string(), port);
    targets
}

fn push_pairing_target(targets: &mut Vec<(String, String)>, host: String, port: u16) {
    let target = format_host_port(&host, port);
    if !targets.iter().any(|(existing, _)| existing == &target) {
        targets.push((target, host));
    }
}

fn normalize_bundle_address_for_host(bundle: &mut TrustBundle, host: &str) -> Result<()> {
    let port = extract_port_from_network_address(bundle.network_address.trim())?;
    bundle.network_address = format_host_port(host, port);
    Ok(())
}

fn extract_port_from_network_address(address: &str) -> Result<u16> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        anyhow::bail!("invalid responder network address: empty");
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return Ok(socket.port());
    }

    if let Some((host_part, port_part)) = trimmed.rsplit_once(':') {
        if host_part.trim().is_empty() {
            anyhow::bail!("invalid responder network address: missing host");
        }
        let port = port_part
            .trim()
            .parse::<u16>()
            .context("invalid responder network address port")?;
        if port == 0 {
            anyhow::bail!("invalid responder network address port: 0");
        }
        return Ok(port);
    }

    anyhow::bail!("invalid responder network address: missing port");
}

fn format_host_port(host: &str, port: u16) -> String {
    let trimmed = host.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        format!("{trimmed}:{port}")
    } else if trimmed.contains(':') {
        format!("[{trimmed}]:{port}")
    } else {
        format!("{trimmed}:{port}")
    }
}

fn normalize_alias(alias: Option<String>) -> Option<String> {
    alias.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

async fn run(state: AppState) -> Result<()> {
    let snapshot = state.snapshot().await;
    let pairing_port = pairing_listener_port(snapshot.network_port);
    let mut listeners = bind_dual_stack_tcp_listeners(pairing_port)
        .with_context(|| format!("bind pairing listener on dual-stack TCP port {pairing_port}"))?;

    info!(
        binds = ?listeners
            .iter()
            .filter_map(|listener| listener.local_addr().ok())
            .collect::<Vec<_>>(),
        network_port = snapshot.network_port,
        "nearby pairing listener started"
    );

    if listeners.len() == 1 {
        accept_pairing_loop(state, listeners.remove(0)).await
    } else {
        let first = listeners.remove(0);
        let second = listeners.remove(0);
        tokio::select! {
            result = accept_pairing_loop(state.clone(), first) => result,
            result = accept_pairing_loop(state, second) => result,
        }
    }
}

async fn accept_pairing_loop(state: AppState, listener: TcpListener) -> Result<()> {
    loop {
        let (socket, remote) = listener
            .accept()
            .await
            .context("accept pairing connection")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_pairing_connection(state, socket, remote).await {
                warn!(remote = %remote, error = ?error, "pairing request failed");
            }
        });
    }
}

async fn handle_pairing_connection(
    state: AppState,
    socket: TcpStream,
    remote: SocketAddr,
) -> Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let read = time::timeout(PAIRING_IO_TIMEOUT, reader.read_line(&mut line))
        .await
        .context("pairing read timeout")?
        .context("pairing read error")?;
    if read == 0 {
        anyhow::bail!("pairing request empty");
    }

    let request: PairingWireRequest =
        serde_json::from_str(&line).context("parse pairing request")?;
    let response = match process_pairing_request(&state, remote.ip(), request).await {
        Ok(response) => response,
        Err(error) => PairingWireResponse::Error {
            message: error.to_string(),
        },
    };

    let payload = serde_json::to_string(&response).context("serialize pairing response")?;
    time::timeout(PAIRING_IO_TIMEOUT, async {
        writer.write_all(payload.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    })
    .await
    .context("pairing write timeout")?
    .context("pairing write error")?;

    Ok(())
}

async fn process_pairing_request(
    state: &AppState,
    remote_ip: IpAddr,
    request: PairingWireRequest,
) -> Result<PairingWireResponse> {
    match request {
        PairingWireRequest::NearbyRequestCode {
            mut requester_bundle,
            requester_alias,
        } => {
            state
                .validate_nearby_code_request_rate_limit(remote_ip)
                .await?;
            requester_bundle =
                rewrite_requester_bundle_for_remote(state, remote_ip, requester_bundle).await;
            let requester_machine_id = requester_bundle.machine_id.clone();
            let requester_display_name = requester_bundle.display_name.clone();
            let challenge = state
                .queue_nearby_pairing_code_challenge(
                    requester_bundle,
                    requester_alias,
                    remote_ip,
                    NEARBY_CHALLENGE_TTL_SECONDS,
                )
                .await?;

            info!(
                request_id = %challenge.request_id,
                requester_machine_id = %requester_machine_id,
                requester_display_name = %requester_display_name,
                "nearby pairing verification code requested"
            );

            Ok(PairingWireResponse::CodeRequired {
                request_id: challenge.request_id,
                message: "enter code shown on target machine".to_string(),
                verification_nonce: challenge
                    .verification_nonce
                    .clone()
                    .context("nearby pairing code challenge missing verification nonce")?,
                expires_at: challenge
                    .verification_expires_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_default(),
            })
        }
        PairingWireRequest::NearbySubmitCode {
            request_id,
            code,
            verification_nonce,
            requester_alias,
        } => {
            state
                .validate_nearby_code_submission_allowed(remote_ip)
                .await?;
            let commit = match state
                .submit_nearby_pairing_code(
                    &request_id,
                    &code,
                    &verification_nonce,
                    requester_alias.and_then(|value| {
                        let trimmed = value.trim().to_string();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        }
                    }),
                )
                .await
            {
                Ok(commit) => {
                    state
                        .record_nearby_code_submission_result(remote_ip, true)
                        .await;
                    commit
                }
                Err(error) => {
                    if is_invalid_nearby_verification_error(&error) {
                        state
                            .record_nearby_code_submission_result(remote_ip, false)
                            .await;
                    }
                    return Err(error);
                }
            };
            Ok(PairingWireResponse::Approved {
                request_id,
                message: commit.message,
                responder_bundle: commit.responder_bundle,
            })
        }
        PairingWireRequest::NearbyJoin {
            code,
            mut requester_bundle,
            requester_alias,
        } => {
            requester_bundle =
                rewrite_requester_bundle_for_remote(state, remote_ip, requester_bundle).await;
            let requester_machine_id = requester_bundle.machine_id.clone();
            let requester_display_name = requester_bundle.display_name.clone();
            let requester_address = requester_bundle.network_address.clone();
            let pending = state
                .queue_nearby_pairing_request_with_code(
                    &code,
                    requester_bundle,
                    requester_alias,
                    remote_ip,
                )
                .await?;

            info!(
                request_id = %pending.request_id,
                requester_machine_id = %requester_machine_id,
                requester_display_name = %requester_display_name,
                requester_address = %requester_address,
                "nearby pairing request pending local approval"
            );

            Ok(PairingWireResponse::Pending {
                request_id: pending.request_id,
                message: "pending approval on target machine".to_string(),
            })
        }
        PairingWireRequest::CheckNearbyJoin { request_id } => {
            match state.nearby_pairing_status(&request_id).await {
                NearbyPairingStatus::Pending => Ok(PairingWireResponse::Pending {
                    request_id,
                    message: "pending approval".to_string(),
                }),
                NearbyPairingStatus::Approved {
                    responder_bundle,
                    message,
                    ..
                } => Ok(PairingWireResponse::Approved {
                    request_id,
                    message,
                    responder_bundle,
                }),
                NearbyPairingStatus::Rejected { message } => Ok(PairingWireResponse::Rejected {
                    request_id,
                    message,
                }),
                NearbyPairingStatus::Missing => Ok(PairingWireResponse::Error {
                    message: "pairing request not found".to_string(),
                }),
            }
        }
    }
}

async fn rewrite_requester_bundle_for_remote(
    state: &AppState,
    remote_ip: IpAddr,
    mut requester_bundle: TrustBundle,
) -> TrustBundle {
    let default_port = state.snapshot().await.network_port;
    let requester_port = extract_port_from_address(&requester_bundle.network_address, default_port);
    requester_bundle.network_address = SocketAddr::new(remote_ip, requester_port).to_string();
    requester_bundle
}

fn is_invalid_nearby_verification_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("verification code is invalid")
        || message.contains("verification nonce is invalid")
        || message.contains("verification code and nonce are invalid")
        || message.contains("verification code expired")
        || message.contains("pairing request rejected")
}

fn pairing_listener_port(network_port: u16) -> u16 {
    if let Ok(raw) = std::env::var("BOUNDLESS_PAIRING_PORT")
        && let Ok(port) = raw.trim().parse::<u16>()
        && port != 0
    {
        return port;
    }

    if let Some(port) = network_port.checked_add(PAIRING_PORT_OFFSET) {
        return port;
    }

    network_port.saturating_sub(PAIRING_PORT_OFFSET).max(1)
}

fn extract_port_from_address(address: &str, default_port: u16) -> u16 {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return default_port;
    }

    if let Some(port) = parse_port_suffix(trimmed) {
        return port;
    }

    default_port
}

fn nearby_pairing_connect_timeout_message(target: &str, timeout_seconds: u64) -> String {
    let remote_port = parse_port_suffix(target)
        .map(|port| format!(" remote TCP {port}"))
        .unwrap_or_else(|| " the remote nearby pairing TCP port".to_string());
    format!(
        "connect nearby pairing endpoint {target} timed out after {timeout_seconds}s; likely network/firewall reachability issue for{remote_port}"
    )
}

fn parse_port_suffix(address: &str) -> Option<u16> {
    if let Some((_, rest)) = address.rsplit_once(':')
        && let Ok(port) = rest.parse::<u16>()
        && port != 0
    {
        return Some(port);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use core_security::{SecurityPaths, ensure_device_identity};

    #[test]
    fn extract_port_from_address_reads_suffix() {
        assert_eq!(extract_port_from_address("example:25100", 15100), 25100);
        assert_eq!(extract_port_from_address("[fe80::1%4]:30100", 15100), 30100);
        assert_eq!(extract_port_from_address("example", 15100), 15100);
        assert_eq!(extract_port_from_address("", 15100), 15100);
    }

    #[test]
    fn nearby_pairing_targets_derive_pairing_ports_from_endpoint_candidates() {
        let targets = nearby_pairing_targets(
            "manual-host",
            15200,
            &[
                "[2001:db8::7]:15100".to_string(),
                "10.0.0.7:15100".to_string(),
            ],
        );

        assert_eq!(
            targets,
            vec![
                ("[2001:db8::7]:15200".to_string(), "2001:db8::7".to_string()),
                ("10.0.0.7:15200".to_string(), "10.0.0.7".to_string()),
                ("manual-host:15200".to_string(), "manual-host".to_string()),
            ]
        );
    }

    #[test]
    fn nearby_pairing_targets_deduplicate_manual_candidate() {
        let targets = nearby_pairing_targets("10.0.0.7", 15200, &["10.0.0.7:15100".to_string()]);

        assert_eq!(
            targets,
            vec![("10.0.0.7:15200".to_string(), "10.0.0.7".to_string())]
        );
    }

    #[test]
    fn nearby_pairing_connect_timeout_names_reachability_and_port() {
        let message = nearby_pairing_connect_timeout_message("10.10.0.187:15200", 4);

        assert!(message.contains("10.10.0.187:15200"));
        assert!(message.contains("timed out after 4s"));
        assert!(message.contains("network/firewall reachability"));
        assert!(message.contains("remote TCP 15200"));
    }

    #[test]
    fn pairing_listener_port_avoids_transport_port_on_overflow() {
        assert_eq!(pairing_listener_port(65436), 65336);
        assert_eq!(pairing_listener_port(65535), 65435);
    }

    #[tokio::test]
    async fn process_nearby_join_queues_pending_request_and_uses_remote_ip_for_address() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-pair-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");

        let (code, _) = state.create_pairing_code(120).await;
        let response = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code,
                requester_bundle: TrustBundle {
                    machine_id: "requester-machine".to_string(),
                    display_name: "requester".to_string(),
                    network_address: "some-host:17777".to_string(),
                    ca_cert_pem: requester_identity.ca_cert_pem,
                },
                requester_alias: Some("Requester Alias".to_string()),
            },
        )
        .await
        .expect("process nearby join");
        let request_id = match response {
            PairingWireResponse::Pending { request_id, .. } => request_id,
            other => panic!("expected pending response, got {other:?}"),
        };

        let pending = state.list_pending_nearby_pairing_requests().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, request_id);
        assert_eq!(pending[0].requester_machine_id, "requester-machine");

        state
            .approve_nearby_pairing_request(&request_id, None)
            .await
            .expect("approve request");
        let requester_peer = state.get_peer("requester-machine").await.expect("peer");
        assert_eq!(requester_peer.address, "192.168.1.44:17777");
        assert_eq!(requester_peer.display_name, "Requester Alias");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn process_nearby_join_duplicate_retry_reuses_existing_pending_request() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-join-duplicate-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };
        let remote_ip = "192.168.1.44".parse().expect("ip");
        let (code, _) = state.create_pairing_code(120).await;

        let first = process_pairing_request(
            &state,
            remote_ip,
            PairingWireRequest::NearbyJoin {
                code: code.clone(),
                requester_bundle: requester_bundle.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect("first join should queue request");
        let first_request_id = match first {
            PairingWireResponse::Pending { request_id, .. } => request_id,
            other => panic!("expected pending response, got {other:?}"),
        };

        let retry = process_pairing_request(
            &state,
            remote_ip,
            PairingWireRequest::NearbyJoin {
                code,
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("retry should reuse pending request even after code was consumed");
        let retry_request_id = match retry {
            PairingWireResponse::Pending { request_id, .. } => request_id,
            other => panic!("expected pending retry response, got {other:?}"),
        };

        assert_eq!(retry_request_id, first_request_id);
        assert_eq!(state.list_pending_nearby_pairing_requests().await.len(), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn process_nearby_join_capacity_rejection_does_not_consume_pairing_code() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-join-capacity-code-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };
        let (first_code, _) = state.create_pairing_code(120).await;
        let (second_code, _) = state.create_pairing_code(120).await;
        let (capacity_rejected_code, _) = state.create_pairing_code(120).await;

        let first = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code: first_code,
                requester_bundle: requester_bundle.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect("first join should queue request");
        let first_request_id = match first {
            PairingWireResponse::Pending { request_id, .. } => request_id,
            other => panic!("expected pending response, got {other:?}"),
        };

        process_pairing_request(
            &state,
            "192.168.1.45".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code: second_code,
                requester_bundle: requester_bundle.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect("second join should queue request at per-peer cap");

        let capacity_error = process_pairing_request(
            &state,
            "192.168.1.46".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code: capacity_rejected_code.clone(),
                requester_bundle: requester_bundle.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect_err("third join should be rejected by admission capacity");
        assert!(
            capacity_error.to_string().contains("for this peer"),
            "capacity error should be returned before any code validation error"
        );

        assert!(state.reject_nearby_pairing_request(&first_request_id).await);
        let retry = process_pairing_request(
            &state,
            "192.168.1.46".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code: capacity_rejected_code,
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("capacity-rejected code should remain usable after capacity is freed");
        assert!(
            matches!(retry, PairingWireResponse::Pending { .. }),
            "retry should queue instead of reporting consumed code"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn request_code_then_submit_code_completes_pairing() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-code-pair-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let code_request = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyRequestCode {
                requester_bundle: requester_bundle.clone(),
                requester_alias: Some("Requester Alias".to_string()),
            },
        )
        .await
        .expect("request code");
        let (request_id, verification_nonce) = match code_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                expires_at,
                ..
            } => {
                assert!(!expires_at.is_empty(), "expires_at should be present");
                assert!(
                    !verification_nonce.is_empty(),
                    "verification_nonce should be present"
                );
                (request_id, verification_nonce)
            }
            other => panic!("expected code_required response, got {other:?}"),
        };

        let pending = state.list_pending_nearby_pairing_requests().await;
        assert_eq!(pending.len(), 1);
        let verification_code = pending[0]
            .verification_code
            .clone()
            .expect("verification code");
        assert_eq!(pending[0].request_id, request_id);

        let submitted = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id,
                code: verification_code,
                verification_nonce,
                requester_alias: None,
            },
        )
        .await
        .expect("submit code");
        let submit_message = match submitted {
            PairingWireResponse::Approved { message, .. } => message,
            other => panic!("expected approved response, got {other:?}"),
        };
        assert!(
            submit_message.contains("trust established"),
            "submit should report committed trust"
        );
        assert!(
            submit_message.contains("connectivity pending"),
            "submit should report connectivity separately"
        );

        let requester_peer = state.get_peer("requester-machine").await.expect("peer");
        assert_eq!(requester_peer.address, "192.168.1.44:17777");
        assert_eq!(requester_peer.display_name, "Requester Alias");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn check_nearby_join_reports_pending_then_approved_for_manual_flow() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-check-status-manual-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");

        let (code, _) = state.create_pairing_code(120).await;
        let join = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyJoin {
                code,
                requester_bundle: TrustBundle {
                    machine_id: "requester-machine".to_string(),
                    display_name: "requester".to_string(),
                    network_address: "some-host:17777".to_string(),
                    ca_cert_pem: requester_identity.ca_cert_pem,
                },
                requester_alias: None,
            },
        )
        .await
        .expect("process nearby join");
        let request_id = match join {
            PairingWireResponse::Pending { request_id, .. } => request_id,
            other => panic!("expected pending response, got {other:?}"),
        };

        let pending_status = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::CheckNearbyJoin {
                request_id: request_id.clone(),
            },
        )
        .await
        .expect("check pending status");
        assert!(
            matches!(pending_status, PairingWireResponse::Pending { .. }),
            "status should be pending before approval"
        );

        let first_approval = state
            .approve_nearby_pairing_request(&request_id, None)
            .await
            .expect("approve request");
        assert!(
            !first_approval.already_committed,
            "first approval should be a fresh trust commit"
        );
        assert_eq!(first_approval.reconnect_status, "connectivity_pending");

        let duplicate_approval = state
            .approve_nearby_pairing_request(&request_id, None)
            .await
            .expect("duplicate approval should be idempotent");
        assert!(
            duplicate_approval.already_committed,
            "duplicate approval should replay committed trust"
        );
        assert!(
            duplicate_approval.message.contains("already trusted"),
            "duplicate approval should explain the already-trusted state"
        );

        let approved_status = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::CheckNearbyJoin { request_id },
        )
        .await
        .expect("check approved status");
        assert!(
            matches!(approved_status, PairingWireResponse::Approved { .. }),
            "status should be approved after local approval"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn request_code_replay_submission_returns_already_trusted_status() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-code-replay-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let code_request = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("request code");
        let (request_id, verification_nonce) = match code_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        let verification_code = state
            .list_pending_nearby_pairing_requests()
            .await
            .into_iter()
            .find(|request| request.request_id == request_id)
            .and_then(|request| request.verification_code)
            .expect("verification code");

        let first_submit = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id: request_id.clone(),
                code: verification_code.clone(),
                verification_nonce: verification_nonce.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect("first submit should approve");
        assert!(
            matches!(first_submit, PairingWireResponse::Approved { .. }),
            "first submit should approve"
        );

        let replay_submit = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id: request_id.clone(),
                code: verification_code,
                verification_nonce,
                requester_alias: None,
            },
        )
        .await
        .expect("replay submission should return approved");
        let replay_message = match replay_submit {
            PairingWireResponse::Approved { message, .. } => message,
            other => panic!("expected approved replay response, got {other:?}"),
        };
        assert!(
            replay_message.contains("already trusted"),
            "replay should be idempotent and actionable"
        );
        assert!(
            replay_message.contains("connectivity pending"),
            "replay should preserve connectivity-pending status"
        );

        let status_after_replay = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::CheckNearbyJoin {
                request_id: request_id.clone(),
            },
        )
        .await
        .expect("status after replay");
        assert!(
            matches!(status_after_replay, PairingWireResponse::Approved { .. }),
            "diagnostic status should remain approved after replay attempt"
        );

        let requester_peer = state.get_peer("requester-machine").await.expect("peer");
        assert_eq!(requester_peer.address, "192.168.1.44:17777");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn request_code_rejects_after_max_invalid_attempts() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-code-invalid-attempts-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let code_request = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("request code");
        let (request_id, verification_nonce) = match code_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        for expected_remaining in (1..5_u8).rev() {
            let error = process_pairing_request(
                &state,
                "192.168.1.44".parse().expect("ip"),
                PairingWireRequest::NearbySubmitCode {
                    request_id: request_id.clone(),
                    code: "000000".to_string(),
                    verification_nonce: verification_nonce.clone(),
                    requester_alias: None,
                },
            )
            .await
            .expect_err("must reject invalid code");
            assert!(
                error
                    .to_string()
                    .contains(format!("attempts_remaining={expected_remaining}").as_str()),
                "error should include remaining attempts"
            );
        }

        let final_error = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id: request_id.clone(),
                code: "000000".to_string(),
                verification_nonce,
                requester_alias: None,
            },
        )
        .await
        .expect_err("must reject final invalid code");
        assert!(
            final_error.to_string().contains("pairing request rejected"),
            "final error should reject pairing request"
        );

        assert!(matches!(
            state.nearby_pairing_status(&request_id).await,
            NearbyPairingStatus::Rejected { .. }
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn check_nearby_join_reports_rejected_after_code_attempts_exhausted() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-check-status-rejected-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let code_request = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("request code");
        let (request_id, verification_nonce) = match code_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        for _ in 0..5 {
            let _ = process_pairing_request(
                &state,
                "192.168.1.44".parse().expect("ip"),
                PairingWireRequest::NearbySubmitCode {
                    request_id: request_id.clone(),
                    code: "000000".to_string(),
                    verification_nonce: verification_nonce.clone(),
                    requester_alias: None,
                },
            )
            .await;
        }

        let rejected_status = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::CheckNearbyJoin { request_id },
        )
        .await
        .expect("check rejected status");
        match rejected_status {
            PairingWireResponse::Rejected { message, .. } => assert!(
                message.contains("too many attempts"),
                "rejected status should carry rejection reason"
            ),
            other => panic!("expected rejected response, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn request_code_rejects_invalid_nonce_and_accepts_correct_nonce() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-code-nonce-mismatch-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem,
        };

        let code_request = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("request code");
        let (request_id, verification_nonce) = match code_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        let pending = state.list_pending_nearby_pairing_requests().await;
        assert_eq!(pending.len(), 1);
        let verification_code = pending[0]
            .verification_code
            .clone()
            .expect("verification code");

        let wrong_nonce_error = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id: request_id.clone(),
                code: verification_code.clone(),
                verification_nonce: "wrong-nonce".to_string(),
                requester_alias: None,
            },
        )
        .await
        .expect_err("must reject incorrect nonce");
        assert!(
            wrong_nonce_error
                .to_string()
                .contains("attempts_remaining=4"),
            "wrong nonce should consume an attempt"
        );

        let approved = process_pairing_request(
            &state,
            "192.168.1.44".parse().expect("ip"),
            PairingWireRequest::NearbySubmitCode {
                request_id,
                code: verification_code,
                verification_nonce,
                requester_alias: None,
            },
        )
        .await
        .expect("submit with correct nonce");
        assert!(
            matches!(approved, PairingWireResponse::Approved { .. }),
            "correct nonce should approve"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn repeated_invalid_submissions_trigger_ip_lockout() {
        let root = std::env::temp_dir().join(format!(
            "boundless-nearby-code-lockout-test-{}",
            uuid::Uuid::new_v4()
        ));
        let receiver_config_path = root.join("receiver-config.json");
        let receiver_security_root = root.join("receiver-security");
        let state =
            AppState::load_or_create_with_paths(receiver_config_path, receiver_security_root)
                .expect("receiver state");

        let requester_paths = SecurityPaths::for_root(root.join("requester-security"));
        let requester_identity = ensure_device_identity(
            &requester_paths,
            "requester-machine",
            "requester",
            Some("10.10.0.5"),
        )
        .expect("requester identity");
        let requester_bundle = TrustBundle {
            machine_id: "requester-machine".to_string(),
            display_name: "requester".to_string(),
            network_address: "some-host:17777".to_string(),
            ca_cert_pem: requester_identity.ca_cert_pem.clone(),
        };
        let remote_ip = "192.168.1.44".parse().expect("ip");

        let first_request = process_pairing_request(
            &state,
            remote_ip,
            PairingWireRequest::NearbyRequestCode {
                requester_bundle: requester_bundle.clone(),
                requester_alias: None,
            },
        )
        .await
        .expect("first request code");
        let (first_request_id, first_verification_nonce) = match first_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        for _ in 0..5 {
            let _ = process_pairing_request(
                &state,
                remote_ip,
                PairingWireRequest::NearbySubmitCode {
                    request_id: first_request_id.clone(),
                    code: "000000".to_string(),
                    verification_nonce: first_verification_nonce.clone(),
                    requester_alias: None,
                },
            )
            .await;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;

        let second_request = process_pairing_request(
            &state,
            remote_ip,
            PairingWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await
        .expect("second request code");
        let (second_request_id, second_verification_nonce) = match second_request {
            PairingWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                ..
            } => (request_id, verification_nonce),
            other => panic!("expected code_required response, got {other:?}"),
        };

        for _ in 0..3 {
            let _ = process_pairing_request(
                &state,
                remote_ip,
                PairingWireRequest::NearbySubmitCode {
                    request_id: second_request_id.clone(),
                    code: "000000".to_string(),
                    verification_nonce: second_verification_nonce.clone(),
                    requester_alias: None,
                },
            )
            .await;
        }

        let lockout_error = process_pairing_request(
            &state,
            remote_ip,
            PairingWireRequest::NearbySubmitCode {
                request_id: second_request_id,
                code: "000000".to_string(),
                verification_nonce: second_verification_nonce,
                requester_alias: None,
            },
        )
        .await
        .expect_err("must lock out repeated invalid submissions");
        assert!(
            lockout_error.to_string().contains("temporarily locked"),
            "lockout error should be returned"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
