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

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return Ok((socket.ip().to_string(), nearby_pairing_port(socket.port())));
    }

    if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(host, _)| host.to_string())
    {
        let port = extract_port_from_endpoint(trimmed)?;
        return Ok((host, nearby_pairing_port(port)));
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
