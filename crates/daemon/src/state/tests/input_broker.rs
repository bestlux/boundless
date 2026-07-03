use super::*;

use crate::input::{InputRuntimeMode, apply_startup_mode};

async fn broker_state(prefix: &str) -> (AppState, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
    (state, root)
}

async fn service_mode_broker_state(prefix: &str) -> (AppState, std::path::PathBuf) {
    let (state, root) = broker_state(prefix).await;
    apply_startup_mode(&state, InputRuntimeMode::ServiceSessionUnsupported).await;
    (state, root)
}

async fn join_connected_peer(state: &AppState) -> String {
    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(
            code,
            "127.0.0.1:15100".to_string(),
            Some("peer".to_string()),
        )
        .await
        .expect("join peer");
    state
        .set_peer_connected(&peer_id, true)
        .await
        .expect("connect peer");
    peer_id
}

#[tokio::test]
async fn attach_fails_closed_when_daemon_owns_interactive_input() {
    let (state, root) = broker_state("boundless-broker-attach-user-mode-test").await;

    let outcome = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;

    assert!(
        !outcome.accepted,
        "non-service daemon must reject broker attach"
    );
    assert!(outcome.broker_token.is_empty());
    assert!(outcome.message.contains("not required"));
    assert!(
        !state.input_broker_route_active(),
        "rejected attach must not activate the broker route"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_fails_closed_for_session_zero_broker() {
    let (state, root) = service_mode_broker_state("boundless-broker-attach-session0-test").await;

    let outcome = state
        .attach_input_broker(0, "test-broker".to_string(), true)
        .await;

    assert!(!outcome.accepted, "session 0 broker must be rejected");
    assert!(outcome.broker_token.is_empty());
    assert!(!state.input_broker_route_active());
    let events = state.transport_events().await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "input_broker_attach_rejected"
                && event.detail.contains("non_interactive_session")),
        "rejected attach should surface truthful diagnostics"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn exchange_rejects_wrong_token_and_routes_nothing() {
    let (state, root) = service_mode_broker_state("boundless-broker-wrong-token-test").await;

    let attach = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let outcome = state
        .exchange_input_broker(
            "not-the-issued-token",
            InputBrokerExchangeObservations {
                captured_events: vec![InputEvent::MouseMove { dx: 5, dy: 5 }],
                ..Default::default()
            },
        )
        .await;

    assert!(!outcome.accepted, "wrong token must be rejected");
    assert!(outcome.inject_frames.is_empty());
    assert!(
        state
            .input_broker_relay()
            .drain_captured_events()
            .is_empty(),
        "rejected exchange must not queue captured events"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_replaces_previous_broker_token() {
    let (state, root) = service_mode_broker_state("boundless-broker-replace-test").await;

    let first = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    let second = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    assert!(first.accepted && second.accepted);

    let stale = state
        .exchange_input_broker(
            &first.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(!stale.accepted, "replaced token must be rejected");

    let fresh = state
        .exchange_input_broker(
            &second.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(fresh.accepted, "current token must stay valid");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stale_broker_fails_closed_until_reattach() {
    let (state, root) = service_mode_broker_state("boundless-broker-stale-test").await;

    let attach = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    assert!(state.input_broker_route_active());

    state.input_broker_relay().expire_attachment_for_test();
    assert!(
        !state.input_broker_route_active(),
        "a silent broker must stop owning the inject route"
    );

    let outcome = state
        .exchange_input_broker(
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(
        !outcome.accepted,
        "stale token must be rejected instead of silently resuming"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn exchange_returns_inject_frames_only_for_current_input_owner() {
    let (state, root) = service_mode_broker_state("boundless-broker-inject-owner-test").await;
    let peer_id = join_connected_peer(&state).await;

    let attach = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner")
    );

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 1,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                }],
            },
        )
        .await
        .expect("route owned frame");

    let outcome = state
        .exchange_input_broker(
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(outcome.accepted);
    assert_eq!(
        outcome.inject_frames.len(),
        1,
        "owner frame must be dispatched to the broker"
    );
    assert_eq!(outcome.inject_frames[0].peer_id, peer_id);

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 2,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up,
                }],
            },
        )
        .await
        .expect("route second frame");
    assert!(state.release_input_owner(&peer_id).await, "release owner");

    let outcome = state
        .exchange_input_broker(
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(outcome.accepted);
    assert!(
        outcome.inject_frames.is_empty(),
        "frames without a matching owner must not reach the broker"
    );
    let events = state.transport_events().await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "input_inject_skipped" && event.peer_id == peer_id),
        "skipped dispatch should be recorded truthfully"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn broker_observations_feed_capture_state_and_release_synthesis() {
    let (state, root) = service_mode_broker_state("boundless-broker-observations-test").await;

    let attach = state
        .attach_input_broker(2, "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let outcome = state
        .exchange_input_broker(
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    },
                    InputEvent::MouseMove { dx: 4, dy: 0 },
                ],
                cursor: Some((100, 200)),
                virtual_bounds: Some((0, 0, 1919, 1079)),
                ..Default::default()
            },
        )
        .await;
    assert!(outcome.accepted);

    let relay = state.input_broker_relay();
    assert_eq!(relay.cursor_position(), Some((100, 200)));
    assert_eq!(relay.virtual_bounds(), Some((0, 0, 1919, 1079)));
    assert_eq!(relay.drain_captured_events().len(), 2);
    assert_eq!(
        relay.drain_release_events(),
        vec![InputEvent::Key {
            scan_code: 30,
            state: KeyState::Up,
        }],
        "held keys reported by the broker must synthesize releases"
    );

    let _ = std::fs::remove_dir_all(root);
}
