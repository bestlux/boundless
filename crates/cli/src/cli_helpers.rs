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

pub(super) fn validate_bmp_payload(bytes: &[u8]) -> Result<()> {
    validate_bmp_bytes(bytes).map_err(anyhow::Error::from)
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
