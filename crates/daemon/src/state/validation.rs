use super::*;

pub(super) fn validate_bind_address(bind: &str) -> Result<()> {
    bind.parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid bind address {bind}"))?;
    Ok(())
}

pub(super) fn normalize_peer_address(address: &str, default_port: u16) -> Result<String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        anyhow::bail!("peer address must not be empty");
    }

    if let Some(parsed) = parse_manual_target(trimmed, default_port) {
        return Ok(parsed.to_string());
    }

    Ok(trimmed.to_string())
}

pub(super) fn validate_pipe_name(pipe_name: &str) -> Result<()> {
    let trimmed = pipe_name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("pipe name must not be empty");
    }

    if trimmed.contains('/') || trimmed.contains('\\') {
        anyhow::bail!("pipe name must not contain path separators");
    }

    Ok(())
}

pub(super) fn normalize_optional_alias(alias: String) -> Option<String> {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(super) fn validate_and_consume_pairing_code(
    pairing_codes: &mut HashMap<String, DateTime<Utc>>,
    code: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    if code.trim().is_empty() {
        anyhow::bail!("pairing code must not be empty");
    }

    let Some(expires_at) = pairing_codes.remove(code) else {
        anyhow::bail!("pairing code is invalid or was already used");
    };

    if expires_at < now {
        anyhow::bail!("pairing code has expired");
    }

    Ok(())
}

pub(super) fn validate_ca_cert_pem(ca_cert_pem: &str) -> Result<()> {
    let certs = CertificateDer::pem_slice_iter(ca_cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parse trust bundle CA certificate PEM")?;

    if certs.is_empty() {
        anyhow::bail!("trust bundle must include at least one CA certificate");
    }

    Ok(())
}

pub(super) fn sanitize_incoming_file_name(file_name: &str) -> Result<String> {
    let mut components = Path::new(file_name).components();
    let Some(component) = components.next() else {
        anyhow::bail!("incoming file name must not be empty");
    };
    if components.next().is_some() {
        anyhow::bail!("incoming file name must not include path separators");
    }

    let Component::Normal(name) = component else {
        anyhow::bail!("incoming file name must be a plain file name");
    };

    let sanitized = name.to_string_lossy().trim().to_string();
    if sanitized.is_empty() {
        anyhow::bail!("incoming file name must not be empty");
    }

    Ok(sanitized)
}
