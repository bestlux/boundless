use super::*;

pub(super) async fn build_tls_acceptor(state: &AppState) -> Result<TlsAcceptor> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("build client verifier")?;

    let cert_chain = build_presented_cert_chain(&identity)?;
    let private_key = parse_private_key(&identity.device_key_pem)?;

    let server = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, private_key)
        .context("build server tls config")?;

    Ok(TlsAcceptor::from(Arc::new(server)))
}

pub(super) async fn build_tls_connector(state: &AppState) -> Result<TlsConnector> {
    let identity = state.identity().clone();
    let trusted = state.trusted_records().await?;
    let roots = build_root_store(&trusted)?;

    let cert_chain = build_presented_cert_chain(&identity)?;
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

fn build_presented_cert_chain(
    identity: &core_security::DeviceIdentity,
) -> Result<Vec<CertificateDer<'static>>> {
    let mut chain = parse_cert_chain(&identity.device_cert_pem)?;
    let mut ca_chain = parse_cert_chain(&identity.ca_cert_pem)?;
    chain.append(&mut ca_chain);
    Ok(chain)
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parse private key")
}

pub(super) fn machine_id_from_presented_ca(
    records: &[core_security::TrustRecord],
    presented_ca: &CertificateDer<'_>,
) -> Result<Option<String>> {
    let mut matched_machine_id: Option<String> = None;

    for record in records {
        for cert in CertificateDer::pem_slice_iter(record.ca_cert_pem.as_bytes()) {
            let cert = cert.context("parse trusted CA certificate")?;
            if cert.as_ref() != presented_ca.as_ref() {
                continue;
            }

            if let Some(existing) = &matched_machine_id {
                if existing != &record.machine_id {
                    bail!("presented CA certificate matched multiple machine records");
                }
            } else {
                matched_machine_id = Some(record.machine_id.clone());
            }
        }
    }

    Ok(matched_machine_id)
}

pub(super) fn parse_server_name(address: &str) -> Result<ServerName<'static>> {
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

pub(super) fn parse_server_name_for_peer(
    peer_id: &str,
    address: &str,
) -> Result<ServerName<'static>> {
    if !peer_id.trim().is_empty()
        && let Ok(server_name) = ServerName::try_from(peer_id.to_string())
    {
        return Ok(server_name);
    }

    parse_server_name(address)
}
