use super::*;

pub(super) fn describe_route_decision(decision: &RouteDecision) -> String {
    match decision {
        RouteDecision::Applied { event_count } => format!("applied events={event_count}"),
        RouteDecision::IgnoredFeatureDisabled => "ignored feature_disabled".to_string(),
        RouteDecision::IgnoredNoOwner => "ignored no_owner".to_string(),
        RouteDecision::IgnoredWrongOwner { owner_peer_id } => {
            format!("ignored wrong_owner={owner_peer_id}")
        }
    }
}

pub(super) fn describe_input_frame_decision(
    decision: &RouteDecision,
    sequence: u64,
    capture_timestamp_unix_ms: i64,
    received_timestamp_unix_ms: i64,
    auto_claimed_owner: bool,
) -> String {
    let capture_to_receive_ms = elapsed_ms(capture_timestamp_unix_ms, received_timestamp_unix_ms);
    format!(
        "sequence={sequence} capture_to_receive_ms={capture_to_receive_ms} auto_claimed_owner={auto_claimed_owner} {}",
        describe_route_decision(decision)
    )
}

pub(super) fn elapsed_ms(start_unix_ms: i64, end_unix_ms: i64) -> i64 {
    (end_unix_ms - start_unix_ms).max(0)
}
