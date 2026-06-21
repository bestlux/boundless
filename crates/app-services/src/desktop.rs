use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
};

use anyhow::{Context, Result, bail};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

const PAIRING_PORT_OFFSET: u16 = 100;

pub const CANONICAL_LOCAL_LAYOUT_TOKEN: &str = "self";
pub const MAX_LAYOUT_REMOTE_PEERS: usize = 4;

#[derive(Debug, Clone)]
pub struct LayoutPeerToken {
    pub peer_id: String,
    pub display_name: String,
}

pub fn nearby_pairing_port(transport_port: u16) -> u16 {
    if transport_port <= u16::MAX - PAIRING_PORT_OFFSET {
        return transport_port + PAIRING_PORT_OFFSET;
    }
    transport_port.saturating_sub(PAIRING_PORT_OFFSET).max(1)
}

pub fn host_and_pairing_port_from_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        bail!("discovery endpoint is empty");
    }

    if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(host, _)| host.to_string())
    {
        let port = extract_port_from_endpoint(trimmed)?;
        return Ok((host, nearby_pairing_port(port)));
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return Ok((socket.ip().to_string(), nearby_pairing_port(socket.port())));
    }

    if let Some((host, _)) = trimmed.rsplit_once(':') {
        let host = host.trim();
        if host.is_empty() {
            bail!("discovery endpoint is missing host");
        }
        let port = extract_port_from_endpoint(trimmed)?;
        return Ok((host.to_string(), nearby_pairing_port(port)));
    }

    bail!("discovery endpoint must include host and port")
}

pub fn redacted_tcp_endpoint_label(endpoint: &str) -> String {
    match parse_endpoint_family_and_port(endpoint) {
        Some((family, port)) => format!("tcp {family} port {port}"),
        None => "tcp invalid endpoint".to_string(),
    }
}

pub fn redacted_tcp_endpoint_labels(endpoints: &[String]) -> String {
    if endpoints.is_empty() {
        return "none".to_string();
    }
    endpoints
        .iter()
        .map(|endpoint| redacted_tcp_endpoint_label(endpoint))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_endpoint_family_and_port(endpoint: &str) -> Option<(&'static str, u16)> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        let family = if socket.is_ipv4() { "ipv4" } else { "ipv6" };
        return Some((family, socket.port()));
    }

    let (host, _) = if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        (host.0.trim(), host.1)
    } else {
        trimmed.rsplit_once(':')?
    };
    let port = extract_port_from_endpoint(trimmed).ok()?;
    let family = if host.contains(':') {
        "ipv6"
    } else if host.parse::<std::net::Ipv4Addr>().is_ok() {
        "ipv4"
    } else {
        "hostname"
    };
    Some((family, port))
}

pub fn parse_pairing_port(value: &str) -> Result<u16> {
    let pairing_port = value
        .parse::<u16>()
        .context("pairing port must be a number in 1..=65535")?;
    if pairing_port == 0 {
        bail!("pairing port must be in 1..=65535");
    }
    Ok(pairing_port)
}

pub fn resolve_boundlessd_candidates(current_exe: Option<PathBuf>) -> Vec<String> {
    let mut candidates = Vec::<String>::new();
    if let Ok(path) = std::env::var("BOUNDLESS_DAEMON_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
    }

    if let Some(current_exe) = current_exe
        && let Some(parent) = current_exe.parent()
    {
        #[cfg(windows)]
        {
            candidates.push(parent.join("boundlessd.exe").display().to_string());
        }
        candidates.push(parent.join("boundlessd").display().to_string());
    }

    candidates.push("boundlessd".to_string());
    #[cfg(windows)]
    candidates.push("boundlessd.exe".to_string());

    candidates.sort();
    candidates.dedup();
    candidates
}

pub fn spawn_boundlessd_process(candidates: &[String]) -> Result<String> {
    let mut errors = Vec::new();
    for candidate in candidates {
        let mut command = ProcessCommand::new(candidate);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        match command.spawn() {
            Ok(_) => return Ok(candidate.clone()),
            Err(error) => errors.push(format!("{candidate}: {error}")),
        }
    }

    bail!(
        "failed to start boundlessd; candidates attempted: {}",
        errors.join("; ")
    )
}

pub fn terminate_boundlessd_processes() -> Result<bool> {
    #[cfg(windows)]
    {
        let mut command = ProcessCommand::new("taskkill");
        command
            .args(["/IM", "boundlessd.exe", "/F", "/T"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);

        let output = command
            .output()
            .context("run taskkill for boundlessd.exe")?;
        if output.status.success() {
            return Ok(true);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("No tasks are running")
            || combined.contains("not found")
            || combined.contains("ERROR: The process")
        {
            return Ok(false);
        }

        bail!(
            "taskkill boundlessd.exe failed with exit code {:?}: {}",
            output.status.code(),
            combined.trim()
        )
    }

    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

pub fn is_local_layout_token(token: &str, machine_id: &str, display_name: Option<&str>) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return false;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "self" | "local" | "this" | "me"
    ) || trimmed.eq_ignore_ascii_case(machine_id)
        || display_name.is_some_and(|name| trimmed.eq_ignore_ascii_case(name))
}

pub fn validate_layout_before_apply(
    layout_grid: &HashMap<(i32, i32), String>,
    local_machine_id: &str,
) -> Result<()> {
    match count_local_layout_cells(layout_grid, local_machine_id) {
        1 => Ok(()),
        0 => anyhow::bail!("layout must include This PC exactly once before applying"),
        _ => anyhow::bail!("layout must include This PC exactly once before applying"),
    }
}

pub fn validate_layout_matrix_spec(
    matrix: &str,
    local_machine_id: &str,
    local_display_name: Option<&str>,
    peers: &[LayoutPeerToken],
) -> Result<()> {
    canonicalize_layout_matrix_spec(matrix, local_machine_id, local_display_name, peers).map(|_| ())
}

pub fn canonicalize_layout_matrix_spec(
    matrix: &str,
    local_machine_id: &str,
    local_display_name: Option<&str>,
    peers: &[LayoutPeerToken],
) -> Result<String> {
    let rows = parse_layout_matrix(matrix);
    let mut local_count = 0usize;
    let mut peer_positions = HashMap::<String, (i32, i32)>::new();
    let mut occupied_positions = Vec::<(i32, i32)>::new();
    let mut canonical_rows = Vec::<Vec<String>>::new();

    for (row_index, row) in rows.iter().enumerate() {
        let mut canonical_row = Vec::<String>::new();
        for (column_index, token) in row.iter().enumerate() {
            let token = token.trim();
            if token.is_empty() {
                canonical_row.push(String::new());
                continue;
            }

            let position = (column_index as i32, row_index as i32);
            if is_local_layout_token(token, local_machine_id, local_display_name) {
                local_count += 1;
                occupied_positions.push(position);
                canonical_row.push(CANONICAL_LOCAL_LAYOUT_TOKEN.to_string());
                continue;
            }

            let peer_id = resolve_layout_peer_token(token, peers)?;
            if let Some(previous) = peer_positions.insert(peer_id.clone(), position) {
                anyhow::bail!(
                    "layout peer `{}` appears more than once at ({}, {}) and ({}, {})",
                    peer_id,
                    previous.0,
                    previous.1,
                    position.0,
                    position.1
                );
            }
            occupied_positions.push(position);
            canonical_row.push(peer_id);
        }
        canonical_rows.push(canonical_row);
    }

    match local_count {
        1 => {}
        0 => anyhow::bail!("layout must include This PC exactly once"),
        _ => anyhow::bail!("layout must include This PC exactly once"),
    }

    if peer_positions.len() > MAX_LAYOUT_REMOTE_PEERS {
        anyhow::bail!("layout supports at most {MAX_LAYOUT_REMOTE_PEERS} peers plus This PC");
    }

    if occupied_positions.len() > 1
        && !layout_positions_are_cardinally_connected(&occupied_positions)
    {
        anyhow::bail!(
            "layout devices must form one connected cardinal group; avoid diagonal-only or isolated devices"
        );
    }

    Ok(canonical_rows
        .into_iter()
        .map(|row| row.join(","))
        .collect::<Vec<_>>()
        .join(";"))
}

pub fn serialize_layout_matrix(
    layout_grid: &HashMap<(i32, i32), String>,
    local_machine_id: &str,
) -> String {
    let mut positions = layout_grid.keys();
    let Some(&(first_x, first_y)) = positions.next() else {
        return String::new();
    };

    let mut min_x = first_x;
    let mut max_x = first_x;
    let mut min_y = first_y;
    let mut max_y = first_y;

    for (x, y) in positions {
        if *x < min_x {
            min_x = *x;
        }
        if *x > max_x {
            max_x = *x;
        }
        if *y < min_y {
            min_y = *y;
        }
        if *y > max_y {
            max_y = *y;
        }
    }

    let mut rows = Vec::new();
    for y in min_y..=max_y {
        let mut cols = Vec::new();
        for x in min_x..=max_x {
            if let Some(id) = layout_grid.get(&(x, y)) {
                cols.push(if id.eq_ignore_ascii_case(local_machine_id) {
                    CANONICAL_LOCAL_LAYOUT_TOKEN.to_string()
                } else {
                    id.clone()
                });
            } else {
                cols.push(String::new());
            }
        }
        rows.push(cols.join(","));
    }

    rows.join(";")
}

pub fn build_orientation_matrix(
    left: Option<&str>,
    right: Option<&str>,
    up: Option<&str>,
    down: Option<&str>,
) -> String {
    let center = format!(
        "{},{},{}",
        left.unwrap_or(""),
        CANONICAL_LOCAL_LAYOUT_TOKEN,
        right.unwrap_or("")
    );
    let mut rows = Vec::<String>::new();
    if let Some(up) = up {
        rows.push(format!(",{},", up));
    }
    rows.push(center);
    if let Some(down) = down {
        rows.push(format!(",{},", down));
    }
    rows.join(";")
}

pub fn parse_layout_matrix(matrix: &str) -> Vec<Vec<String>> {
    matrix
        .split(';')
        .map(|row| {
            row.split(',')
                .map(|token| token.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn resolve_layout_peer_token(token: &str, peers: &[LayoutPeerToken]) -> Result<String> {
    let token = token.trim();
    let token_lower = token.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(token)
                || peer.peer_id.to_ascii_lowercase().starts_with(&token_lower)
                || peer.display_name.eq_ignore_ascii_case(token)
        })
        .collect::<Vec<_>>();

    if matches.is_empty() {
        anyhow::bail!("layout token `{token}` does not resolve to a known peer");
    }
    if matches.len() > 1 {
        anyhow::bail!("layout token `{token}` is ambiguous across peers");
    }
    Ok(matches[0].peer_id.clone())
}

fn layout_positions_are_cardinally_connected(positions: &[(i32, i32)]) -> bool {
    use std::collections::{HashSet, VecDeque};

    let occupied = positions.iter().copied().collect::<HashSet<_>>();
    let Some(&start) = occupied.iter().next() else {
        return true;
    };

    let mut seen = HashSet::<(i32, i32)>::new();
    let mut queue = VecDeque::from([start]);
    while let Some((x, y)) = queue.pop_front() {
        if !seen.insert((x, y)) {
            continue;
        }
        for neighbor in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
            if occupied.contains(&neighbor) && !seen.contains(&neighbor) {
                queue.push_back(neighbor);
            }
        }
    }

    seen.len() == occupied.len()
}

fn extract_port_from_endpoint(endpoint: &str) -> Result<u16> {
    endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .context("discovery endpoint must include a non-zero port")
}

fn count_local_layout_cells(
    layout_grid: &HashMap<(i32, i32), String>,
    local_machine_id: &str,
) -> usize {
    layout_grid
        .values()
        .filter(|id| id.eq_ignore_ascii_case(local_machine_id))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(peer_id: &str, display_name: &str) -> LayoutPeerToken {
        LayoutPeerToken {
            peer_id: peer_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn layout_matrix_validation_accepts_four_remote_peers_plus_local() {
        let peers = [
            peer("peer-left", "left"),
            peer("peer-right", "right"),
            peer("peer-up", "up"),
            peer("peer-down", "down"),
        ];

        let canonical = canonicalize_layout_matrix_spec(
            ",up,;left,self,right;,down,",
            "local-machine",
            Some("local-device"),
            &peers,
        )
        .expect("four peers plus local should validate");
        assert_eq!(canonical, ",peer-up,;peer-left,self,peer-right;,peer-down,");
    }

    #[test]
    fn host_and_pairing_port_parses_bracketed_ipv6_endpoint() {
        let (host, port) =
            host_and_pairing_port_from_endpoint("[fe80::1%4]:15100").expect("parse endpoint");

        assert_eq!(host, "fe80::1%4");
        assert_eq!(port, 15200);
    }

    #[test]
    fn redacted_tcp_endpoint_labels_preserve_family_and_port_only() {
        let labels = redacted_tcp_endpoint_labels(&[
            "10.0.0.9:15100".to_string(),
            "[fe80::1%4]:15200".to_string(),
            "office-pc.local:15100".to_string(),
        ]);

        assert_eq!(
            labels,
            "tcp ipv4 port 15100, tcp ipv6 port 15200, tcp hostname port 15100"
        );
        assert!(!labels.contains("10.0.0.9"));
        assert!(!labels.contains("fe80::1"));
        assert!(!labels.contains("office-pc"));
    }

    #[test]
    fn layout_matrix_validation_rejects_unknown_ambiguous_and_duplicate_tokens() {
        let peers = [peer("peer-left-a", "office"), peer("peer-left-b", "office")];

        let unknown = validate_layout_matrix_spec("self,missing", "local", None, &peers)
            .expect_err("unknown peer token must fail");
        assert!(unknown.to_string().contains("does not resolve"));

        let ambiguous = validate_layout_matrix_spec("self,office", "local", None, &peers)
            .expect_err("ambiguous peer token must fail");
        assert!(ambiguous.to_string().contains("ambiguous"));

        let duplicate =
            validate_layout_matrix_spec("self,peer-left-a;peer-left-a,", "local", None, &peers)
                .expect_err("duplicate peer token must fail");
        assert!(duplicate.to_string().contains("appears more than once"));
    }

    #[test]
    fn layout_matrix_validation_rejects_isolated_or_too_large_layouts() {
        let peers = [
            peer("peer-a", "a"),
            peer("peer-b", "b"),
            peer("peer-c", "c"),
            peer("peer-d", "d"),
            peer("peer-e", "e"),
        ];

        let isolated = validate_layout_matrix_spec("self,; ,peer-a", "local", None, &peers)
            .expect_err("diagonal-only device must fail");
        assert!(isolated.to_string().contains("connected cardinal group"));

        let too_large = validate_layout_matrix_spec("a,b,c;d,self,e", "local", None, &peers)
            .expect_err("more than four peers must fail");
        assert!(too_large.to_string().contains("at most"));
    }
}
