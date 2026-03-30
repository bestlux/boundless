use super::*;

pub(super) fn resolve_discovered_peer<'a>(
    snapshot: &'a ConsoleSnapshot,
    selector: &str,
) -> Result<&'a ConsoleDiscoveredPeer> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 {
            bail!("pair request index must start at 1");
        }
        return snapshot
            .discovered_peers
            .get(index - 1)
            .ok_or_else(|| anyhow::anyhow!("no discovered peer at index {index}"));
    }

    let normalized = selector.trim();
    if normalized.is_empty() {
        bail!("selector must not be empty");
    }
    let selector_lower = normalized.to_ascii_lowercase();
    let matches = snapshot
        .discovered_peers
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
        bail!("multiple discovered peers match `{selector}`; use full machine_id or index");
    }
    Ok(matches[0])
}

pub(super) fn prompt_pairing_code() -> Result<String> {
    print!("pairing code: ");
    io::stdout().flush().context("flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read pairing code")?;
    Ok(line.trim().to_string())
}

pub(super) fn prompt_pairing_nonce() -> Result<String> {
    print!("pairing nonce: ");
    io::stdout().flush().context("flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("read pairing nonce")?;
    Ok(line.trim().to_string())
}

pub(super) fn nearby_pairing_port(transport_port: u16) -> u16 {
    if transport_port <= u16::MAX - 100 {
        return transport_port + 100;
    }

    let fallback = transport_port.saturating_sub(100);
    if fallback == 0 { 1 } else { fallback }
}

pub(super) fn short_machine_id(machine_id: &str) -> &str {
    machine_id.get(..8).unwrap_or(machine_id)
}

pub(super) fn filter_connectable_discovery_records<T, F>(
    discovered_peers: Vec<T>,
    local_machine_id: &str,
    paired_peer_ids: &[String],
    mut machine_id_of: F,
) -> Vec<T>
where
    F: FnMut(&T) -> String,
{
    let local_machine_id = local_machine_id.to_ascii_lowercase();
    let paired_peer_ids = paired_peer_ids
        .iter()
        .map(|peer_id| peer_id.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();

    discovered_peers
        .into_iter()
        .filter(|peer| {
            let machine_id = machine_id_of(peer).to_ascii_lowercase();
            machine_id != local_machine_id && !paired_peer_ids.contains(&machine_id)
        })
        .collect()
}

pub(super) fn parse_npipe_endpoint(endpoint: &str) -> Result<Option<String>> {
    let Some(rest) = endpoint.strip_prefix("npipe://") else {
        return Ok(None);
    };
    if let Some(name) = rest.strip_prefix("./pipe/") {
        return pipe_path_from_name(name).map(Some);
    }
    if let Some(name) = rest.strip_prefix(r"\\.\pipe\") {
        return pipe_path_from_name(name).map(Some);
    }

    bail!("invalid named-pipe endpoint {endpoint}; expected npipe://./pipe/<name>")
}

fn pipe_path_from_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("named-pipe endpoint is missing pipe name");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        bail!("named-pipe endpoint pipe name must not contain path separators");
    }

    Ok(format!(r"\\.\pipe\{trimmed}"))
}

#[cfg(windows)]
#[derive(Clone)]
pub(super) struct NamedPipeConnector {
    pipe_path: String,
}

#[cfg(windows)]
impl NamedPipeConnector {
    pub(super) fn new(pipe_path: String) -> Self {
        Self { pipe_path }
    }
}

#[cfg(windows)]
impl Service<Uri> for NamedPipeConnector {
    type Response = TokioIo<NamedPipeClient>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        let pipe_path = self.pipe_path.clone();
        Box::pin(async move {
            let client = open_named_pipe_with_retry(pipe_path).await?;
            Ok(TokioIo::new(client))
        })
    }
}

#[cfg(windows)]
const ERROR_PIPE_BUSY_CODE: i32 = 231;
#[cfg(windows)]
const PIPE_BUSY_MAX_RETRIES: u32 = 20;
#[cfg(windows)]
const PIPE_BUSY_BACKOFF_MS: u64 = 25;

#[cfg(windows)]
async fn open_named_pipe_with_retry(pipe_path: String) -> io::Result<NamedPipeClient> {
    let mut attempt = 0_u32;

    loop {
        match ClientOptions::new().open(pipe_path.as_str()) {
            Ok(client) => return Ok(client),
            Err(error) if is_pipe_busy_error(&error) && attempt < PIPE_BUSY_MAX_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(PIPE_BUSY_BACKOFF_MS)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
pub(super) fn is_pipe_busy_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE)
}

pub(super) fn validate_bmp_payload(bytes: &[u8]) -> Result<()> {
    validate_bmp_bytes(bytes).map_err(anyhow::Error::from)
}

pub(super) fn extract_port_from_network_address(address: &str) -> Result<u16> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        bail!("invalid responder network address: empty");
    }

    if let Ok(socket) = trimmed.parse::<std::net::SocketAddr>() {
        return Ok(socket.port());
    }

    if let Some((host_part, port_part)) = trimmed.rsplit_once(':') {
        if host_part.trim().is_empty() {
            bail!("invalid responder network address: missing host");
        }

        let port = port_part
            .trim()
            .parse::<u16>()
            .context("invalid responder network address port")?;
        if port == 0 {
            bail!("invalid responder network address port: 0");
        }
        return Ok(port);
    }

    bail!("invalid responder network address: missing port");
}

pub(super) fn format_host_port(host: &str, port: u16) -> String {
    let trimmed = host.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        format!("{trimmed}:{port}")
    } else if trimmed.contains(':') {
        format!("[{trimmed}]:{port}")
    } else {
        format!("{trimmed}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct DiscoveryRecord {
        machine_id: String,
    }

    #[test]
    fn filter_connectable_discovery_records_removes_local_and_paired_machine_ids() {
        let records = vec![
            DiscoveryRecord {
                machine_id: "local-machine".to_string(),
            },
            DiscoveryRecord {
                machine_id: "paired-machine".to_string(),
            },
            DiscoveryRecord {
                machine_id: "new-machine".to_string(),
            },
        ];

        let filtered = filter_connectable_discovery_records(
            records,
            "LOCAL-MACHINE",
            &["paired-machine".to_string()],
            |record| record.machine_id.clone(),
        );

        assert_eq!(
            filtered.len(),
            1,
            "only the unpaired remote peer should remain"
        );
        assert_eq!(filtered[0].machine_id, "new-machine");
    }
}
