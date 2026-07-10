use super::*;

use crate::input::{InputRuntimeMode, apply_startup_mode};
use core_input::MouseButton;

const ALLOWED_USER_SID: &str = "S-1-5-21-1000-2000-3000-1001";
const OTHER_USER_SID: &str = "S-1-5-21-1000-2000-3000-1002";
const ADMIN_USER_SID: &str = "S-1-5-21-1000-2000-3000-500";
const SYSTEM_SID: &str = "S-1-5-18";

fn verified_client(user_sid: &str, session_id: u32) -> Option<InputBrokerClientIdentity> {
    Some(InputBrokerClientIdentity {
        user_sid: Some(user_sid.to_string()),
        session_id: Some(session_id),
    })
}

fn allowed_client() -> Option<InputBrokerClientIdentity> {
    verified_client(ALLOWED_USER_SID, 2)
}

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
    state.set_input_broker_allowed_user_sid(ALLOWED_USER_SID);
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

async fn assert_attach_rejected_event(state: &AppState, reason: &str) {
    let events = state.transport_events().await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "input_broker_attach_rejected"
                && event.detail.contains(&format!("reason={reason}"))),
        "rejected attach should record truthful diagnostics with reason={reason}"
    );
}

#[tokio::test]
async fn attach_fails_closed_when_daemon_owns_interactive_input() {
    let (state, root) = broker_state("boundless-broker-attach-user-mode-test").await;
    state.set_input_broker_allowed_user_sid(ALLOWED_USER_SID);

    let outcome = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
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
async fn attach_fails_closed_without_verified_client_identity() {
    let (state, root) = service_mode_broker_state("boundless-broker-attach-unverified-test").await;

    // No transport-verified identity: nothing the caller self-reports can
    // substitute, so attach must fail closed.
    let outcome = state
        .attach_input_broker(None, "test-broker".to_string(), true)
        .await;

    assert!(!outcome.accepted);
    assert!(outcome.broker_token.is_empty());
    assert!(!state.input_broker_route_active());
    assert_attach_rejected_event(&state, "unverified_client").await;

    let partially_verified = Some(InputBrokerClientIdentity {
        user_sid: None,
        session_id: Some(2),
    });
    let outcome = state
        .attach_input_broker(partially_verified, "test-broker".to_string(), true)
        .await;
    assert!(
        !outcome.accepted,
        "identity without a resolved user SID must be rejected"
    );
    assert_attach_rejected_event(&state, "unverified_user").await;

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_fails_closed_for_verified_session_zero_client() {
    let (state, root) = service_mode_broker_state("boundless-broker-attach-session0-test").await;

    let outcome = state
        .attach_input_broker(
            verified_client(ALLOWED_USER_SID, 0),
            "test-broker".to_string(),
            true,
        )
        .await;

    assert!(
        !outcome.accepted,
        "session 0 pipe client must be rejected even for the allowed user"
    );
    assert!(outcome.broker_token.is_empty());
    assert!(!state.input_broker_route_active());
    assert_attach_rejected_event(&state, "non_interactive_session").await;

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_fails_closed_for_wrong_admin_and_system_users() {
    let (state, root) = service_mode_broker_state("boundless-broker-attach-wrong-user-test").await;

    for wrong_sid in [OTHER_USER_SID, ADMIN_USER_SID, SYSTEM_SID] {
        let outcome = state
            .attach_input_broker(
                verified_client(wrong_sid, 2),
                "test-broker".to_string(),
                true,
            )
            .await;
        assert!(
            !outcome.accepted,
            "pipe client {wrong_sid} is not the allowed desktop user and must be rejected"
        );
        assert!(outcome.broker_token.is_empty());
        assert!(!state.input_broker_route_active());
    }
    assert_attach_rejected_event(&state, "wrong_user").await;

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_fails_closed_when_allowed_user_not_configured() {
    let (state, root) = broker_state("boundless-broker-attach-unconfigured-test").await;
    apply_startup_mode(&state, InputRuntimeMode::ServiceSessionUnsupported).await;

    let outcome = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;

    assert!(
        !outcome.accepted,
        "without a configured allowed user SID every attach must fail closed"
    );
    assert_attach_rejected_event(&state, "allowed_user_not_configured").await;

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn wrong_user_attach_cannot_replace_live_allowed_user_broker() {
    let (state, root) = service_mode_broker_state("boundless-broker-no-steal-test").await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    assert!(state.input_broker_route_active());

    for wrong_sid in [OTHER_USER_SID, ADMIN_USER_SID, SYSTEM_SID] {
        let steal = state
            .attach_input_broker(
                verified_client(wrong_sid, 2),
                "test-broker".to_string(),
                true,
            )
            .await;
        assert!(
            !steal.accepted,
            "{wrong_sid} must not replace a live broker"
        );
    }

    assert!(
        state.input_broker_route_active(),
        "allowed-user broker must remain attached after rejected attach attempts"
    );
    let outcome = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(
        outcome.accepted,
        "original allowed-user broker token must remain valid"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn exchange_rejects_wrong_token_and_routes_nothing() {
    let (state, root) = service_mode_broker_state("boundless-broker-wrong-token-test").await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let outcome = state
        .exchange_input_broker(
            allowed_client(),
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
async fn exchange_rejects_wrong_user_even_with_valid_token() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-exchange-wrong-user-test").await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    for wrong_client in [
        None,
        verified_client(ADMIN_USER_SID, 2),
        verified_client(ALLOWED_USER_SID, 0),
    ] {
        let outcome = state
            .exchange_input_broker(
                wrong_client,
                &attach.broker_token,
                InputBrokerExchangeObservations {
                    captured_events: vec![InputEvent::MouseMove { dx: 5, dy: 5 }],
                    ..Default::default()
                },
            )
            .await;
        assert!(
            !outcome.accepted,
            "exchange must verify the pipe client identity, not just the token"
        );
        assert!(outcome.inject_frames.is_empty());
    }
    assert!(
        state
            .input_broker_relay()
            .drain_captured_events()
            .is_empty(),
        "rejected exchanges must not queue captured events"
    );

    let outcome = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(
        outcome.accepted,
        "allowed-user broker must remain attached after rejected exchanges"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn detach_rejects_wrong_user_and_keeps_broker_attached() {
    let (state, root) = service_mode_broker_state("boundless-broker-detach-wrong-user-test").await;
    let peer_id = join_connected_peer(&state).await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set capture target");
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim input owner")
    );
    let _ = state.drain_outgoing(&peer_id).await;
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    },
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Down,
                    },
                ],
                ..Default::default()
            },
        )
        .await;
    assert!(observed.accepted);

    assert!(
        !state
            .detach_input_broker(verified_client(ADMIN_USER_SID, 2), &attach.broker_token)
            .await,
        "non-allowed users must not detach the broker even with the token"
    );
    assert!(
        !state.detach_input_broker(None, &attach.broker_token).await,
        "unverified callers must not detach the broker"
    );
    assert!(state.input_broker_route_active());
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(peer_id.as_str()),
        "unauthorized detach must not mutate capture state"
    );
    assert_eq!(
        state.input_owner().await.as_deref(),
        Some(peer_id.as_str()),
        "unauthorized detach must not mutate owner state"
    );

    assert!(
        state
            .detach_input_broker(allowed_client(), &attach.broker_token)
            .await,
        "allowed-user broker must be able to detach itself"
    );
    assert!(!state.input_broker_route_active());
    assert!(
        state.input_capture_target().await.is_none(),
        "detach must clear stale capture target before tray relaunch"
    );
    assert!(
        state.input_owner().await.is_none(),
        "detach must release stale incoming owner before tray relaunch"
    );
    let outgoing = state.drain_outgoing(&peer_id).await;
    assert!(
        outgoing.iter().any(|payload| matches!(
            payload,
            OutboundPayload::InputFrame { events, .. }
                if events == &vec![
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Up,
                    },
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Up,
                    },
                ]
        )),
        "authorized detach must queue authoritative held-input releases before clearing capture: {outgoing:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attach_replaces_previous_broker_token() {
    let (state, root) = service_mode_broker_state("boundless-broker-replace-test").await;

    let first = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    let second = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(first.accepted && second.accepted);

    let stale = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(!stale.accepted, "replaced token must be rejected");

    let fresh = state
        .exchange_input_broker(
            allowed_client(),
            &second.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(fresh.accepted, "current token must stay valid");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn replacement_attach_queues_final_release_after_prior_captured_down() {
    let (state, root) = service_mode_broker_state("boundless-broker-replace-release-test").await;
    let peer_id = join_connected_peer(&state).await;
    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set capture target");

    let first = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first.accepted);
    let down = InputEvent::Key {
        scan_code: 30,
        state: KeyState::Down,
    };
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![down.clone()],
                ..Default::default()
            },
        )
        .await;
    assert!(observed.accepted);

    // Model the capture pass having drained and routed the Down while the
    // relay still owns authoritative pressed-state tracking.
    let drained = state.input_broker_relay().drain_captured_events();
    assert_eq!(drained, vec![down.clone()]);
    state
        .queue_input_events(&peer_id, drained)
        .await
        .expect("queue captured down");

    let second = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(second.accepted);

    let routed_events = state
        .drain_outgoing(&peer_id)
        .await
        .into_iter()
        .filter_map(|payload| match payload {
            OutboundPayload::InputFrame { events, .. } => Some(events),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(
        routed_events,
        vec![
            down,
            InputEvent::Key {
                scan_code: 30,
                state: KeyState::Up,
            },
        ],
        "replacement attach must order a final release after the prior broker's Down"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn replacement_attach_queue_failure_preserves_prior_broker_and_pressed_state() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-replace-queue-failure-test").await;
    let first = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first.accepted);
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                }],
                ..Default::default()
            },
        )
        .await;
    assert!(observed.accepted);

    // Force queue_input_events to fail after replacement has identified a
    // target, without mutating the old attachment itself.
    *state.input.control.capture_target_peer_id.write().await = Some("missing-peer".to_string());
    let replacement = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(!replacement.accepted);
    assert!(replacement.broker_token.is_empty());

    let old_token_still_valid = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(old_token_still_valid.accepted);
    assert_eq!(
        state.input_broker_relay().release_events_snapshot(),
        vec![InputEvent::Key {
            scan_code: 30,
            state: KeyState::Up,
        }],
        "failed replacement must preserve authoritative pressed state for retry"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stale_broker_fails_closed_until_reattach() {
    let (state, root) = service_mode_broker_state("boundless-broker-stale-test").await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
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
            allowed_client(),
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
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
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
            allowed_client(),
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
            allowed_client(),
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
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let outcome = state
        .exchange_input_broker(
            allowed_client(),
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

#[tokio::test]
async fn clipboard_broker_exchange_routes_local_payloads_to_connected_peers() {
    let (state, root) = service_mode_broker_state("boundless-broker-clipboard-local-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    let last_input_exchange_at = state
        .input_broker_relay()
        .last_exchange_at_for_test()
        .expect("attach heartbeat");

    let outcome = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("broker-local".to_string())),
            None,
        )
        .await;

    assert!(outcome.accepted);
    assert_eq!(
        outcome.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::Accepted
    );
    assert_eq!(
        state
            .input_broker_relay()
            .last_exchange_at_for_test()
            .expect("heartbeat remains present"),
        last_input_exchange_at,
        "clipboard traffic must not extend input broker liveness"
    );
    assert!(outcome.remote_payload.is_none());
    let outgoing = state.drain_outgoing(&peer_id).await;
    assert!(matches!(
        outgoing.as_slice(),
        [OutboundPayload::ClipboardText { text }] if text == "broker-local"
    ));

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn newer_local_clipboard_payload_supersedes_broker_inflight_remote() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-clipboard-supersede-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    state
        .enqueue_remote_clipboard_text(&peer_id, "stale remote".to_string())
        .await
        .expect("enqueue remote clipboard");

    let staged = state
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None)
        .await;
    assert!(staged.remote_payload.is_some());

    let local = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("new local".to_string())),
            None,
        )
        .await;
    assert!(local.accepted);
    assert_eq!(
        local.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::Accepted
    );
    assert!(
        local.remote_payload.is_none(),
        "an accepted local update must not return the older remote payload"
    );

    let next = state
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None)
        .await;
    assert!(
        next.remote_payload.is_none(),
        "the superseded remote payload must not reappear on the next poll"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn invalid_local_clipboard_payload_reports_deterministic_rejection() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-clipboard-deterministic-test").await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let outcome = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Image(vec![0; 64])),
            None,
        )
        .await;
    assert!(outcome.accepted);
    assert_eq!(
        outcome.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::DeterministicRejected
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn clipboard_broker_remote_payload_waits_for_apply_report_before_echo_suppression() {
    let (state, root) = service_mode_broker_state("boundless-broker-clipboard-remote-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    state
        .enqueue_remote_clipboard_text(&peer_id, "remote".to_string())
        .await
        .expect("enqueue remote clipboard");

    let staged = state
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None)
        .await;
    assert!(staged.accepted);
    let remote = staged
        .remote_payload
        .as_ref()
        .expect("remote payload should be staged for broker");
    assert!(matches!(
        &remote.payload,
        ClipboardPayload::Text(text) if text == "remote"
    ));
    assert_eq!(remote.retry_count, 0);
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "staged remote payload should be broker-inflight, not still queued"
    );

    let reported = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("remote".to_string())),
            Some(ClipboardBrokerApplyReport {
                source_peer_id: remote.peer_id.clone(),
                hash: remote.hash.clone(),
                applied: true,
                message: String::new(),
            }),
        )
        .await;
    assert!(reported.accepted);
    let outgoing = state.drain_outgoing(&peer_id).await;
    assert!(
        outgoing.is_empty(),
        "broker echo after remote apply must be suppressed"
    );

    let _ = std::fs::remove_dir_all(root);
}
