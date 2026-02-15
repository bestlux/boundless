use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        ClientConfig, RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
        server::WebPkiClientVerifier,
    },
};
use tracing::{error, info, warn};

use core_protocol::{PROTOCOL_CURRENT, WireMessage, decode_line, encode_line};

use crate::state::AppState;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const SUPERVISOR_TICK: Duration = Duration::from_secs(3);
const MAX_BACKOFF_SECONDS: u64 = 30;

pub fn start(state: AppState) {
    tokio::spawn(listener_loop(state.clone()));
    tokio::spawn(supervisor_loop(state));
}

async fn listener_loop(state: AppState) {
    let snapshot = state.snapshot().await;
    let bind = format!("0.0.0.0:{}", snapshot.network_port);

    let listener = match TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(%error, bind = %bind, "transport listener failed to bind");
            return;
        }
    };

    info!(bind = %bind, "transport listener started");

    loop {
        match listener.accept().await {
            Ok((socket, remote)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_incoming_connection(state, socket).await {
                        warn!(error = ?error, remote = %remote, "incoming session ended with error");
                    }
                });
            }
            Err(error) => {
                warn!(%error, "transport accept failed");
                time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

async fn supervisor_loop(state: AppState) {
    let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();

    loop {
        workers.retain(|_, handle| !handle.is_finished());

        let snapshot = state.snapshot().await;
        for peer in snapshot.peers {
            if peer.peer_id == snapshot.machine_id || peer.address.trim().is_empty() {
                continue;
            }

            if workers.contains_key(&peer.peer_id) {
                continue;
            }

            let state = state.clone();
            let peer_id = peer.peer_id.clone();
            let handle = tokio::spawn(async move {
                peer_worker(state, peer_id).await;
            });
            workers.insert(peer.peer_id, handle);
        }

        time::sleep(SUPERVISOR_TICK).await;
    }
}

async fn peer_worker(state: AppState, peer_id: String) {
    let mut backoff_secs: u64 = 1;

    loop {
        let Some(peer) = state.get_peer(&peer_id).await else {
            info!(peer_id = %peer_id, "peer worker exiting; peer removed");
            return;
        };

        match connect_and_run_outbound(state.clone(), &peer_id, &peer.address).await {
            Ok(()) => {
                backoff_secs = 1;
            }
            Err(error) => {
                warn!(peer_id = %peer_id, address = %peer.address, error = ?error, "outbound connect failed");
                if let Err(mark_error) = state.set_peer_connected(&peer_id, false).await {
                    warn!(%mark_error, "failed to mark peer disconnected");
                }

                time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            }
        }
    }
}

async fn connect_and_run_outbound(state: AppState, peer_id: &str, address: &str) -> Result<()> {
    let socket = TcpStream::connect(address)
        .await
        .with_context(|| format!("tcp connect {address}"))?;

    let connector = build_tls_connector(&state).await?;
    let server_name = parse_server_name(address)?;
    let stream = connector
        .connect(server_name, socket)
        .await
        .with_context(|| format!("tls connect {address}"))?;

    run_session(
        state,
        Some(peer_id.to_string()),
        tokio_rustls::TlsStream::Client(stream),
        true,
    )
    .await
}

async fn handle_incoming_connection(state: AppState, socket: TcpStream) -> Result<()> {
    let acceptor = build_tls_acceptor(&state).await?;
    let stream = acceptor.accept(socket).await.context("tls accept")?;
    run_session(state, None, tokio_rustls::TlsStream::Server(stream), false).await
}

async fn run_session<S>(
    state: AppState,
    peer_hint: Option<String>,
    stream: tokio_rustls::TlsStream<S>,
    is_outbound: bool,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut interval = time::interval(HEARTBEAT_INTERVAL);

    let snapshot = state.snapshot().await;
    let local_hello = WireMessage::Hello {
        machine_id: snapshot.machine_id.clone(),
        display_name: snapshot.device_name.clone(),
        protocol: PROTOCOL_CURRENT,
        capability_count: core_protocol::default_capabilities().len(),
    };

    writer
        .write_all(encode_line(&local_hello)?.as_bytes())
        .await
        .context("send hello")?;

    let mut remote_peer_id = peer_hint;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let heartbeat = WireMessage::Heartbeat {
                    machine_id: snapshot.machine_id.clone(),
                    timestamp_unix_ms: now_millis(),
                };
                writer
                    .write_all(encode_line(&heartbeat)?.as_bytes())
                    .await
                    .context("send heartbeat")?;
                writer.flush().await.context("flush heartbeat")?;
            }
            read = reader.read_line(&mut line) => {
                let read = read.context("read transport line")?;
                if read == 0 {
                    break;
                }

                let message = decode_line(&line).context("decode wire message")?;
                line.clear();

                match message {
                    WireMessage::Hello { machine_id, .. } => {
                        remote_peer_id.get_or_insert(machine_id.clone());

                        if let Some(peer_id) = &remote_peer_id {
                            let _ = state.set_peer_connected(peer_id, true).await;
                        }

                        if !is_outbound {
                            let ack = WireMessage::HelloAck {
                                machine_id: snapshot.machine_id.clone(),
                                accepted: true,
                            };
                            writer
                                .write_all(encode_line(&ack)?.as_bytes())
                                .await
                                .context("send hello_ack")?;
                            writer.flush().await.context("flush hello_ack")?;
                        }
                    }
                    WireMessage::HelloAck { accepted, .. } => {
                        if accepted && let Some(peer_id) = &remote_peer_id {
                            let _ = state.set_peer_connected(peer_id, true).await;
                        }
                    }
                    WireMessage::Heartbeat { .. } => {
                        if let Some(peer_id) = &remote_peer_id {
                            let _ = state.touch_peer(peer_id).await;
                        }
                    }
                    WireMessage::Error { message } => {
                        warn!(%message, "remote error frame");
                    }
                }
            }
        }
    }

    if let Some(peer_id) = &remote_peer_id {
        let _ = state.set_peer_connected(peer_id, false).await;
    }

    Ok(())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn build_tls_acceptor(state: &AppState) -> Result<TlsAcceptor> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("build client verifier")?;

    let cert_chain = parse_cert_chain(&identity.device_cert_pem)?;
    let private_key = parse_private_key(&identity.device_key_pem)?;

    let server = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server tls config")?;

    Ok(TlsAcceptor::from(Arc::new(server)))
}

async fn build_tls_connector(state: &AppState) -> Result<TlsConnector> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;

    let cert_chain = parse_cert_chain(&identity.device_cert_pem)?;
    let private_key = parse_private_key(&identity.device_key_pem)?;

    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, private_key)
        .context("build client tls config")?;

    Ok(TlsConnector::from(Arc::new(client)))
}

fn build_root_store(records: &[core_security::TrustRecord]) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();

    for record in records {
        for cert in CertificateDer::pem_slice_iter(record.ca_cert_pem.as_bytes()) {
            roots
                .add(cert.context("parse trusted CA certificate")?)
                .context("add trusted CA certificate")?;
        }
    }

    Ok(roots)
}

fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse cert chain")
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parse private key")
}

fn parse_server_name(address: &str) -> Result<ServerName<'static>> {
    if let Ok(socket) = address.parse::<std::net::SocketAddr>() {
        return ServerName::try_from(socket.ip().to_string())
            .context("parse server name from socket address");
    }

    let host = if address.starts_with('[') {
        address
            .split(']')
            .next()
            .map(|s| s.trim_start_matches('[').to_string())
            .unwrap_or_else(|| address.to_string())
    } else {
        address
            .rsplit_once(':')
            .map(|(host, _)| host.to_string())
            .unwrap_or_else(|| address.to_string())
    };

    ServerName::try_from(host).context("parse server name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_server_name_from_ipv4_socket() {
        let server_name = parse_server_name("127.0.0.1:15100").expect("server name");
        assert_eq!(server_name.to_str(), "127.0.0.1");
    }

    #[test]
    fn extracts_server_name_from_dns() {
        let server_name = parse_server_name("peer.local:15100").expect("server name");
        assert_eq!(server_name.to_str(), "peer.local");
    }

    #[test]
    fn rejects_invalid_server_name() {
        let err = parse_server_name("!").expect_err("must fail");
        assert!(err.to_string().contains("parse server name"));
    }
}
