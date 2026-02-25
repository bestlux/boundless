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

use crate::state::{AppState, NearbyPairingStatus};

const PAIRING_BIND_HOST: &str = "0.0.0.0";
const PAIRING_PORT_OFFSET: u16 = 100;
const PAIRING_IO_TIMEOUT: Duration = Duration::from_secs(10);
const NEARBY_CHALLENGE_TTL_SECONDS: u64 = 120;

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Serialize)]
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

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "pairing listener stopped");
        }
    });
}

async fn run(state: AppState) -> Result<()> {
    let snapshot = state.snapshot().await;
    let pairing_port = pairing_listener_port(snapshot.network_port);
    let bind = format!("{PAIRING_BIND_HOST}:{pairing_port}");
    let listener = TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind pairing listener {bind}"))?;

    info!(
        bind = %bind,
        network_port = snapshot.network_port,
        "nearby pairing listener started"
    );

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
            let responder_bundle = match state
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
                Ok(bundle) => {
                    state
                        .record_nearby_code_submission_result(remote_ip, true)
                        .await;
                    bundle
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
                message: "approved".to_string(),
                responder_bundle,
            })
        }
        PairingWireRequest::NearbyJoin {
            code,
            mut requester_bundle,
            requester_alias,
        } => {
            state.consume_pairing_code(&code).await?;
            requester_bundle =
                rewrite_requester_bundle_for_remote(state, remote_ip, requester_bundle).await;
            let requester_machine_id = requester_bundle.machine_id.clone();
            let requester_display_name = requester_bundle.display_name.clone();
            let requester_address = requester_bundle.network_address.clone();
            let pending = state
                .queue_nearby_pairing_request(requester_bundle, requester_alias)
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
                NearbyPairingStatus::Approved { responder_bundle } => {
                    Ok(PairingWireResponse::Approved {
                        request_id,
                        message: "approved".to_string(),
                        responder_bundle,
                    })
                }
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
        assert!(
            matches!(submitted, PairingWireResponse::Approved { .. }),
            "submit should approve"
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

        state
            .approve_nearby_pairing_request(&request_id, None)
            .await
            .expect("approve request");

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
    async fn request_code_replay_submission_is_rejected_and_status_stays_approved() {
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

        let replay_error = process_pairing_request(
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
        .expect_err("replay submission must fail");
        assert!(
            replay_error.to_string().contains("not found"),
            "replay should fail because request is already finalized"
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
