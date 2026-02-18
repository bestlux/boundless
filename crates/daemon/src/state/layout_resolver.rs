use super::*;

pub(super) fn resolve_capture_handoff_target_with_fallback(
    config: &RuntimeConfig,
    current_target: Option<&str>,
    direction: SwitchDirection,
) -> Option<CaptureHandoffTarget> {
    let resolved = resolve_capture_handoff_target(config, current_target, direction);
    if resolved.is_some() {
        return resolved;
    }

    if layout_is_actionable_for_handoff(config, current_target) {
        return None;
    }

    resolve_single_peer_handoff_target(config, current_target, direction)
}

pub(super) fn resolve_capture_handoff_target(
    config: &RuntimeConfig,
    current_target: Option<&str>,
    direction: SwitchDirection,
) -> Option<CaptureHandoffTarget> {
    let matrix = parse_layout_matrix(&config.layout_matrix);
    let mut source_cell: Option<(usize, usize)> = None;

    for (row_index, row) in matrix.iter().enumerate() {
        for (column_index, token) in row.iter().enumerate() {
            let is_source = if let Some(peer_id) = current_target {
                layout_token_matches_peer(token, peer_id, &config.peers)
            } else {
                is_local_layout_token(token, config)
            };
            if !is_source {
                continue;
            }

            if source_cell.is_some() {
                return None;
            }
            source_cell = Some((row_index, column_index));
        }
    }

    let (row, column) = source_cell?;

    let token_at = |row_index: usize, column_index: usize| -> Option<String> {
        matrix
            .get(row_index)
            .and_then(|row_tokens| row_tokens.get(column_index))
            .cloned()
    };

    let token_to_target = |token: String| -> Option<CaptureHandoffTarget> {
        if token.trim().is_empty() {
            return None;
        }
        if is_local_layout_token(&token, config) {
            return Some(CaptureHandoffTarget::Local);
        }
        resolve_peer_layout_token(&token, &config.peers).map(CaptureHandoffTarget::Peer)
    };

    match direction {
        SwitchDirection::Left => {
            for next_column in (0..column).rev() {
                let Some(token) = token_at(row, next_column) else {
                    continue;
                };
                if let Some(target) = token_to_target(token) {
                    return Some(target);
                }
            }
            None
        }
        SwitchDirection::Right => {
            let row_width = matrix
                .get(row)
                .map(|row_tokens| row_tokens.len())
                .unwrap_or(0);
            for next_column in (column + 1)..row_width {
                let Some(token) = token_at(row, next_column) else {
                    continue;
                };
                if let Some(target) = token_to_target(token) {
                    return Some(target);
                }
            }
            None
        }
        SwitchDirection::Up => {
            for next_row in (0..row).rev() {
                let Some(token) = token_at(next_row, column) else {
                    continue;
                };
                if let Some(target) = token_to_target(token) {
                    return Some(target);
                }
            }
            None
        }
        SwitchDirection::Down => {
            for next_row in (row + 1)..matrix.len() {
                let Some(token) = token_at(next_row, column) else {
                    continue;
                };
                if let Some(target) = token_to_target(token) {
                    return Some(target);
                }
            }
            None
        }
    }
}

fn layout_is_actionable_for_handoff(config: &RuntimeConfig, current_target: Option<&str>) -> bool {
    let matrix = parse_layout_matrix(&config.layout_matrix);
    let mut source_count = 0usize;
    let mut has_destination = false;

    for row in matrix {
        for token in row {
            if let Some(peer_id) = current_target {
                if layout_token_matches_peer(&token, peer_id, &config.peers) {
                    source_count += 1;
                }
                if is_local_layout_token(&token, config) {
                    has_destination = true;
                }
            } else {
                if is_local_layout_token(&token, config) {
                    source_count += 1;
                }
                if resolve_peer_layout_token(&token, &config.peers).is_some() {
                    has_destination = true;
                }
            }
        }
    }

    source_count == 1 && has_destination
}

fn resolve_single_peer_handoff_target(
    config: &RuntimeConfig,
    current_target: Option<&str>,
    direction: SwitchDirection,
) -> Option<CaptureHandoffTarget> {
    if !matches!(direction, SwitchDirection::Left | SwitchDirection::Right) {
        return None;
    }

    let mut connected = config
        .peers
        .iter()
        .filter(|peer| peer.connected)
        .map(|peer| peer.peer_id.as_str());
    let peer_id = connected.next()?;
    if connected.next().is_some() {
        return None;
    }

    match current_target {
        None => Some(CaptureHandoffTarget::Peer(peer_id.to_string())),
        Some(current_peer_id) if current_peer_id == peer_id => Some(CaptureHandoffTarget::Local),
        Some(_) => None,
    }
}

pub(super) fn resolve_switch_all_target_order(config: &RuntimeConfig) -> Vec<String> {
    let mut ordered = Vec::<String>::new();
    let mut seen = HashSet::<String>::new();

    for row in parse_layout_matrix(&config.layout_matrix) {
        for token in row {
            if is_local_layout_token(&token, config) {
                continue;
            }
            let Some(peer_id) = resolve_peer_layout_token(&token, &config.peers) else {
                continue;
            };
            if seen.insert(peer_id.clone()) {
                ordered.push(peer_id);
            }
        }
    }

    let mut remainder = config
        .peers
        .iter()
        .filter(|peer| peer.connected)
        .map(|peer| (peer.display_name.to_ascii_lowercase(), peer.peer_id.clone()))
        .collect::<Vec<_>>();
    remainder
        .sort_by(|(name_a, id_a), (name_b, id_b)| name_a.cmp(name_b).then_with(|| id_a.cmp(id_b)));
    for (_, peer_id) in remainder {
        if seen.insert(peer_id.clone()) {
            ordered.push(peer_id);
        }
    }

    ordered
}

fn parse_layout_matrix(spec: &str) -> Vec<Vec<String>> {
    spec.split(';')
        .map(|row| {
            row.split(',')
                .map(|token| token.trim().to_string())
                .collect()
        })
        .collect()
}

fn is_local_layout_token(token: &str, config: &RuntimeConfig) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }

    matches!(
        token.to_ascii_lowercase().as_str(),
        "self" | "local" | "this" | "me"
    ) || token.eq_ignore_ascii_case(&config.machine_id)
        || token.eq_ignore_ascii_case(&config.device_name)
}

fn resolve_peer_layout_token(token: &str, peers: &[PeerConfig]) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    let token_lower = token.to_ascii_lowercase();
    let mut matched_peer_ids = Vec::<String>::new();

    for peer in peers.iter().filter(|peer| peer.connected) {
        let peer_id_match = peer.peer_id.eq_ignore_ascii_case(token);
        let display_name_match = peer.display_name.eq_ignore_ascii_case(token);
        let peer_id_prefix_match = peer.peer_id.to_ascii_lowercase().starts_with(&token_lower);
        if !(peer_id_match || display_name_match || peer_id_prefix_match) {
            continue;
        }

        if !matched_peer_ids
            .iter()
            .any(|peer_id| peer_id == &peer.peer_id)
        {
            matched_peer_ids.push(peer.peer_id.clone());
        }
    }

    if matched_peer_ids.len() == 1 {
        matched_peer_ids.pop()
    } else {
        None
    }
}

fn layout_token_matches_peer(token: &str, peer_id: &str, peers: &[PeerConfig]) -> bool {
    resolve_peer_layout_token(token, peers).as_deref() == Some(peer_id)
}
