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

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum PairingWireRequest {
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
        PairingWireRequest::NearbyJoin {
            code,
            mut requester_bundle,
            requester_alias,
        } => {
            state.consume_pairing_code(&code).await?;

            let default_port = state.snapshot().await.network_port;
            let requester_port =
                extract_port_from_address(&requester_bundle.network_address, default_port);
            requester_bundle.network_address =
                SocketAddr::new(remote_ip, requester_port).to_string();
            let requester_machine_id = requester_bundle.machine_id.clone();
            let requester_display_name = requester_bundle.display_name.clone();
            let pending = state
                .queue_nearby_pairing_request(requester_bundle, requester_alias)
                .await;

            info!(
                request_id = %pending.request_id,
                requester_machine_id = %requester_machine_id,
                requester_display_name = %requester_display_name,
                requester_address = %SocketAddr::new(remote_ip, requester_port),
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
}
