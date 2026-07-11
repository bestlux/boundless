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

async fn request_broker_capture(state: &AppState, peer_id: &str) {
    state
        .set_input_capture_backend_mode(INPUT_BROKER_BACKEND_MODE)
        .await;
    state
        .set_input_capture_target(Some(peer_id))
        .await
        .expect("set capture target");
    let _ = state.input_broker_relay().set_desired_lock_active(true);
}

async fn authorize_broker_capture(state: &AppState, peer_id: &str, broker_token: &str) {
    request_broker_capture(state, peer_id).await;
    let outcome = state
        .exchange_input_broker(
            allowed_client(),
            broker_token,
            InputBrokerExchangeObservations {
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(outcome.capture_forwarding_authorized);
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
async fn versioned_daemon_rejects_unversioned_or_old_broker_before_attach() {
    let (state, root) = service_mode_broker_state("boundless-broker-protocol-skew-test").await;

    for revision in [0, ipc_api::INPUT_BROKER_PROTOCOL_REVISION + 1] {
        let outcome = state
            .attach_input_broker_versioned(
                allowed_client(),
                "old-broker".to_string(),
                true,
                revision,
            )
            .await;
        assert!(!outcome.accepted);
        assert!(outcome.broker_token.is_empty());
        assert_eq!(
            outcome.protocol_revision,
            ipc_api::INPUT_BROKER_PROTOCOL_REVISION
        );
        assert!(outcome.message.contains("protocol mismatch"));
        assert!(!state.input_broker_route_active());
    }

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
async fn exchange_rejects_captured_events_until_lock_report_is_acknowledged() {
    let (state, root) = service_mode_broker_state("boundless-broker-lock-handshake-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    request_broker_capture(&state, &peer_id).await;

    let key = InputEvent::Key {
        scan_code: 30,
        state: KeyState::Down,
        semantics: core_input::KeySemantics::Physical,
    };
    let pre_lock = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![key.clone()],
                lock_active: false,
                ..Default::default()
            },
        )
        .await;
    assert!(pre_lock.accepted);
    assert!(!pre_lock.capture_forwarding_authorized);
    assert!(
        state
            .input_broker_relay()
            .drain_captured_events()
            .is_empty()
    );

    let lock_report = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(lock_report.capture_forwarding_authorized);
    assert!(
        state
            .input_broker_relay()
            .drain_captured_events()
            .is_empty()
    );

    let authorized = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![key.clone()],
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(authorized.capture_forwarding_authorized);
    assert_eq!(
        state.input_broker_relay().drain_captured_events(),
        vec![key]
    );

    let safety_report = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up,
                    semantics: core_input::KeySemantics::Physical,
                }],
                escape_unlock_count: 1,
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(!safety_report.capture_forwarding_authorized);
    assert!(
        state
            .input_broker_relay()
            .drain_captured_events()
            .is_empty(),
        "a safety report must revoke authorization before accepting its batch"
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
    authorize_broker_capture(&state, &peer_id, &attach.broker_token).await;
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
                        semantics: core_input::KeySemantics::Physical,
                    },
                    InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Down,
                    },
                ],
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(observed.accepted);

    assert!(
        !state
            .detach_input_broker(
                verified_client(ADMIN_USER_SID, 2),
                &attach.broker_token,
                &attach.delivery_epoch,
                0,
            )
            .await,
        "non-allowed users must not detach the broker even with the token"
    );
    assert!(
        !state
            .detach_input_broker(None, &attach.broker_token, &attach.delivery_epoch, 0)
            .await,
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
            .detach_input_broker(
                allowed_client(),
                &attach.broker_token,
                &attach.delivery_epoch,
                0,
            )
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
                        semantics: core_input::KeySemantics::Physical,
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

    let first = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first.accepted);
    authorize_broker_capture(&state, &peer_id, &first.broker_token).await;
    let down = InputEvent::Key {
        scan_code: 30,
        state: KeyState::Down,
        semantics: core_input::KeySemantics::Physical,
    };
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![down.clone()],
                lock_active: true,
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
                semantics: core_input::KeySemantics::Physical,
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
    let peer_id = join_connected_peer(&state).await;
    let first = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first.accepted);
    authorize_broker_capture(&state, &peer_id, &first.broker_token).await;
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                }],
                lock_active: true,
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
            semantics: core_input::KeySemantics::Physical,
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
async fn stale_reject_cleanup_and_reattach_preserve_final_release_ordering() {
    let (state, root) = service_mode_broker_state("boundless-broker-stale-release-test").await;
    let peer_id = join_connected_peer(&state).await;
    let first = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first.accepted);
    authorize_broker_capture(&state, &peer_id, &first.broker_token).await;

    let down = InputEvent::Key {
        scan_code: 30,
        state: KeyState::Down,
        semantics: core_input::KeySemantics::Physical,
    };
    let observed = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![down.clone()],
                lock_active: true,
                ..Default::default()
            },
        )
        .await;
    assert!(observed.accepted);
    let drained = state.input_broker_relay().drain_captured_events();
    state
        .queue_input_events(&peer_id, drained)
        .await
        .expect("queue captured down");

    state.input_broker_relay().expire_attachment_for_test();
    let stale = state
        .exchange_input_broker(
            allowed_client(),
            &first.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(!stale.accepted);
    assert!(
        state
            .detach_input_broker(
                allowed_client(),
                &first.broker_token,
                &first.delivery_epoch,
                0,
            )
            .await,
        "stale rejection must preserve token identity for authoritative cleanup"
    );
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
                semantics: core_input::KeySemantics::Physical,
            },
        ]
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
                    semantics: core_input::KeySemantics::Physical,
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
    let first_batch_id = outcome.inject_batch_id;

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
                    semantics: core_input::KeySemantics::Physical,
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
            InputBrokerExchangeObservations {
                acked_inject_batch_id: first_batch_id,
                ..Default::default()
            },
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
async fn inject_batch_backpressure_preserves_fifo_until_exact_ack() {
    let (state, root) = service_mode_broker_state("boundless-broker-inject-batch-test").await;
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
                    scan_code: 1,
                    state: KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                }],
            },
        )
        .await
        .expect("route first frame");

    let first = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(first.accepted);
    assert_ne!(first.inject_batch_id, 0);
    assert_eq!(
        first
            .inject_frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        vec![1]
    );

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 2,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::Key {
                    scan_code: 2,
                    state: KeyState::Up,
                    semantics: core_input::KeySemantics::Physical,
                }],
            },
        )
        .await
        .expect("route later frame");

    let blocked = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                inject_backpressure: true,
                ..Default::default()
            },
        )
        .await;
    assert!(blocked.accepted);
    assert_eq!(blocked.inject_batch_id, first.inject_batch_id);
    assert!(blocked.inject_frames.is_empty());

    let acknowledged = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: first.inject_batch_id,
                ..Default::default()
            },
        )
        .await;
    assert!(acknowledged.accepted);
    assert_ne!(acknowledged.inject_batch_id, 0);
    assert_ne!(acknowledged.inject_batch_id, first.inject_batch_id);
    assert_eq!(acknowledged.inject_frames[0].sequence, 2);

    let duplicate_ack = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: first.inject_batch_id,
                ..Default::default()
            },
        )
        .await;
    assert!(
        duplicate_ack.accepted,
        "lost ack replies must be idempotent"
    );
    assert_eq!(duplicate_ack.inject_batch_id, acknowledged.inject_batch_id);
    assert_eq!(duplicate_ack.inject_frames[0].sequence, 2);

    let final_ack = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: acknowledged.inject_batch_id,
                ..Default::default()
            },
        )
        .await;
    assert!(final_ack.accepted);
    assert_eq!(final_ack.inject_batch_id, 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn retained_inject_batch_cancellation_replays_until_ack_and_unblocks_later_work() {
    let (state, root) = service_mode_broker_state("boundless-broker-inject-cancel-test").await;
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
                    semantics: core_input::KeySemantics::Physical,
                }],
            },
        )
        .await
        .expect("route first frame");

    let first = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(first.accepted);
    assert!(!first.inject_batch_cancelled);
    assert_eq!(first.inject_frames[0].sequence, 1);
    assert!(state.release_input_owner(&peer_id).await, "revoke owner");

    let cancellation_observation = InputBrokerExchangeObservations {
        inject_backpressure: true,
        ..Default::default()
    };
    let cancelled = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            cancellation_observation.clone(),
        )
        .await;
    assert!(cancelled.accepted);
    assert_eq!(cancelled.inject_batch_id, first.inject_batch_id);
    assert!(cancelled.inject_batch_cancelled);
    assert!(cancelled.inject_frames.is_empty());

    let replayed = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            cancellation_observation,
        )
        .await;
    assert!(replayed.accepted);
    assert_eq!(replayed.inject_batch_id, first.inject_batch_id);
    assert!(replayed.inject_batch_cancelled);
    assert!(replayed.inject_frames.is_empty());

    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("restore owner")
    );
    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 2,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::Key {
                    scan_code: 31,
                    state: KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                }],
            },
        )
        .await
        .expect("route later frame");
    let next = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: first.inject_batch_id,
                ..Default::default()
            },
        )
        .await;
    assert!(next.accepted);
    assert!(!next.inject_batch_cancelled);
    assert_ne!(next.inject_batch_id, first.inject_batch_id);
    assert_eq!(next.inject_frames[0].sequence, 2);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stale_reattach_accepts_completed_receipt_without_replaying_inject_batch() {
    let (state, root) = service_mode_broker_state("boundless-broker-stale-inject-test").await;
    let peer_id = join_connected_peer(&state).await;
    let first_attach = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first_attach.accepted);
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
                sequence: 41,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseMove { dx: 3, dy: 0 }],
            },
        )
        .await
        .expect("route frame");
    let dispatched = state
        .exchange_input_broker(
            allowed_client(),
            &first_attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_eq!(dispatched.inject_frames.len(), 1);
    assert_ne!(dispatched.inject_batch_id, 0);

    state.input_broker_relay().expire_attachment_for_test();
    let replacement = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(replacement.accepted);
    assert_eq!(
        replacement.delivery_epoch, first_attach.delivery_epoch,
        "broker sessions in one daemon process must share a delivery epoch"
    );
    let recovered = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: dispatched.inject_batch_id,
                ..Default::default()
            },
        )
        .await;
    assert!(
        recovered.accepted,
        "same-daemon reattach must retain the exact delivery ID so the surviving receipt remains valid"
    );
    assert!(
        recovered.inject_frames.is_empty(),
        "a completed same-epoch receipt must be committed before any replay"
    );
    assert_eq!(recovered.inject_batch_id, 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stale_reattach_reauthorizes_partial_batch_without_replaying_full_frames() {
    let (state, root) = service_mode_broker_state("boundless-broker-partial-reattach-test").await;
    let peer_id = join_connected_peer(&state).await;
    let first_attach = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first_attach.accepted);
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
                sequence: 42,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![
                    InputEvent::Key {
                        scan_code: 29,
                        state: KeyState::Down,
                        semantics: core_input::KeySemantics::Physical,
                    },
                    InputEvent::Key {
                        scan_code: 46,
                        state: KeyState::Down,
                        semantics: core_input::KeySemantics::Physical,
                    },
                ],
            },
        )
        .await
        .expect("route chord frame");
    let dispatched = state
        .exchange_input_broker(
            allowed_client(),
            &first_attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_eq!(dispatched.inject_frames.len(), 1);
    assert_ne!(dispatched.inject_batch_id, 0);

    state.input_broker_relay().expire_attachment_for_test();
    let replacement = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(replacement.accepted);
    assert_eq!(replacement.delivery_epoch, first_attach.delivery_epoch);
    let recovered = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                inject_backpressure: true,
                ..Default::default()
            },
        )
        .await;
    assert!(recovered.accepted);
    assert_eq!(recovered.inject_batch_id, dispatched.inject_batch_id);
    assert!(!recovered.inject_batch_cancelled);
    assert!(
        recovered.inject_frames.is_empty(),
        "the surviving tray owns the exact suffix, so reauthorization must not replay full frames"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn completed_hold_authorization_survives_response_loss_but_not_owner_or_policy_change() {
    let (state, root) = service_mode_broker_state("boundless-broker-held-auth-test").await;
    let peer_id = join_connected_peer(&state).await;
    let first_attach = state
        .attach_input_broker(allowed_client(), "first-broker".to_string(), true)
        .await;
    assert!(first_attach.accepted);
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
                sequence: 43,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::Key {
                    scan_code: 29,
                    state: KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                }],
            },
        )
        .await
        .expect("route held modifier");
    let dispatched = state
        .exchange_input_broker(
            allowed_client(),
            &first_attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_ne!(dispatched.inject_batch_id, 0);
    assert_ne!(dispatched.inject_authorization_generation, 0);

    state.input_broker_relay().expire_attachment_for_test();
    let replacement = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(replacement.accepted);
    let consumed_receipt = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                acked_inject_batch_id: dispatched.inject_batch_id,
                held_input_authorization_generation: dispatched.inject_authorization_generation,
                ..Default::default()
            },
        )
        .await;
    assert!(consumed_receipt.accepted);
    assert!(consumed_receipt.held_input_authorized);
    assert_eq!(consumed_receipt.inject_batch_id, 0);

    // The response above may be lost. Authorization is daemon-generation
    // state, not a one-shot receipt, so the exact retry remains valid.
    let response_loss_retry = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                held_input_authorization_generation: dispatched.inject_authorization_generation,
                ..Default::default()
            },
        )
        .await;
    assert!(response_loss_retry.held_input_authorized);

    assert!(state.release_input_owner(&peer_id).await);
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("reclaim same owner")
    );
    let owner_changed = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                held_input_authorization_generation: dispatched.inject_authorization_generation,
                ..Default::default()
            },
        )
        .await;
    assert!(owner_changed.accepted);
    assert!(
        !owner_changed.held_input_authorized,
        "release/reclaim must invalidate the old held-input generation"
    );

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 44,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: KeyState::Down,
                }],
            },
        )
        .await
        .expect("route new held button");
    let redispatched = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_ne!(redispatched.inject_authorization_generation, 0);
    assert_ne!(
        redispatched.inject_authorization_generation,
        dispatched.inject_authorization_generation
    );

    state
        .set_feature("share_input".to_string(), false)
        .await
        .expect("disable input sharing");
    state
        .set_feature("share_input".to_string(), true)
        .await
        .expect("re-enable input sharing");
    let policy_changed = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations {
                inject_backpressure: true,
                held_input_authorization_generation: redispatched.inject_authorization_generation,
                ..Default::default()
            },
        )
        .await;
    assert!(!policy_changed.held_input_authorized);
    assert_eq!(policy_changed.inject_batch_id, redispatched.inject_batch_id);
    assert!(
        policy_changed.inject_batch_cancelled,
        "a response-lost batch must be cancelled after its issuing generation is revoked"
    );
    assert_eq!(policy_changed.inject_authorization_generation, 0);
    assert!(policy_changed.inject_frames.is_empty());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_lock_serializes_force_and_auto_owner_switches() {
    let (state, root) = service_mode_broker_state("boundless-broker-authority-lock-test").await;
    let peer_a = join_connected_peer(&state).await;
    let peer_b = join_connected_peer(&state).await;
    assert_ne!(peer_a, peer_b);
    assert!(
        state
            .claim_input_owner(&peer_a, false)
            .await
            .expect("claim owner A")
    );

    let authority = state.input.control.authorization.read().await;
    let generation_a = authority.generation();
    assert!(authority.allows_peer(&peer_a));
    let force_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let force_state = state.clone();
    let force_peer = peer_b.clone();
    let force_task_barrier = force_barrier.clone();
    let mut force_switch = tokio::spawn(async move {
        force_task_barrier.wait().await;
        force_state
            .claim_input_owner(&force_peer, true)
            .await
            .expect("force owner B")
    });
    force_barrier.wait().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut force_switch)
            .await
            .is_err(),
        "force switch must wait for the in-use authorization snapshot"
    );
    drop(authority);
    assert!(force_switch.await.expect("join force switch"));
    assert!(
        !state
            .held_input_authorization_is_current(generation_a)
            .await,
        "force switch must invalidate owner A generation"
    );

    assert!(
        state
            .claim_input_owner(&peer_a, true)
            .await
            .expect("restore owner A")
    );
    let mut authority = state.input.control.authorization.write().await;
    let (claimed, changed) = authority.claim_owner(&peer_b, true);
    assert!(
        claimed && changed,
        "force B while holding the authority barrier"
    );
    let forced_generation_b = authority.generation();
    let auto_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let auto_state = state.clone();
    let auto_peer = peer_a.clone();
    let auto_task_barrier = auto_barrier.clone();
    let mut auto_switch = tokio::spawn(async move {
        auto_task_barrier.wait().await;
        auto_state
            .route_incoming_input_frame(
                &auto_peer,
                InputFrame {
                    source_peer_id: auto_peer.clone(),
                    sequence: 1,
                    timestamp_unix_ms: Utc::now().timestamp_millis(),
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 0 }],
                },
            )
            .await
            .expect("auto-switch route")
    });
    auto_barrier.wait().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut auto_switch)
            .await
            .is_err(),
        "auto switch must wait while the force transition owns authority"
    );
    drop(authority);
    assert!(matches!(
        auto_switch.await.expect("join auto switch"),
        core_input::RouteDecision::IgnoredWrongOwner { .. }
    ));
    assert_eq!(state.input_owner().await.as_deref(), Some(peer_b.as_str()));
    assert_eq!(
        state.input_authorization_generation().await,
        forced_generation_b,
        "auto claim must observe the force transition's cooldown atomically"
    );

    {
        let mut authority = state.input.control.authorization.write().await;
        authority.set_owner_last_changed_at_for_test(Some(
            Instant::now()
                - std::time::Duration::from_millis(
                    super::super::INPUT_OWNER_AUTO_STEAL_COOLDOWN_MS + 1,
                ),
        ));
    }
    assert!(matches!(
        state
            .route_incoming_input_frame(
                &peer_a,
                InputFrame {
                    source_peer_id: peer_a.clone(),
                    sequence: 1,
                    timestamp_unix_ms: Utc::now().timestamp_millis(),
                    events: vec![InputEvent::MouseMove { dx: 1, dy: 0 }],
                },
            )
            .await
            .expect("auto switch after cooldown"),
        core_input::RouteDecision::Applied { .. }
    ));
    assert!(
        !state
            .held_input_authorization_is_current(forced_generation_b)
            .await,
        "an allowed auto switch must advance the generation"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_staging_reads_owner_and_generation_from_one_snapshot() {
    let (state, root) = service_mode_broker_state("boundless-broker-batch-snapshot-test").await;
    let peer_a = join_connected_peer(&state).await;
    let peer_b = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    assert!(
        state
            .claim_input_owner(&peer_a, false)
            .await
            .expect("claim owner A")
    );
    state
        .route_incoming_input_frame(
            &peer_a,
            InputFrame {
                source_peer_id: peer_a.clone(),
                sequence: 1,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseMove { dx: 2, dy: 0 }],
            },
        )
        .await
        .expect("queue owner A frame");

    let mut authority = state.input.control.authorization.write().await;
    assert_eq!(authority.claim_owner(&peer_b, true), (true, true));
    assert_eq!(authority.claim_owner(&peer_a, true), (true, true));
    let reclaimed_generation_a = authority.generation();
    let exchange_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let exchange_state = state.clone();
    let broker_token = attach.broker_token.clone();
    let exchange_task_barrier = exchange_barrier.clone();
    let mut exchange = tokio::spawn(async move {
        exchange_task_barrier.wait().await;
        exchange_state
            .exchange_input_broker(
                allowed_client(),
                &broker_token,
                InputBrokerExchangeObservations::default(),
            )
            .await
    });
    exchange_barrier.wait().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut exchange)
            .await
            .is_err(),
        "batch staging must wait for the authority snapshot write barrier"
    );
    drop(authority);
    state.notify_input_owner_transition();
    let outcome = exchange.await.expect("join broker exchange");
    assert!(outcome.accepted);
    assert!(
        outcome.inject_frames.is_empty(),
        "owner A frame must not be relabeled after an A-to-B-to-A authority cycle"
    );
    assert_eq!(outcome.inject_authorization_generation, 0);
    assert_eq!(
        state.input_authorization_generation().await,
        reclaimed_generation_a
    );

    state
        .route_incoming_input_frame(
            &peer_a,
            InputFrame {
                source_peer_id: peer_a.clone(),
                sequence: 2,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: KeyState::Down,
                }],
            },
        )
        .await
        .expect("queue reclaimed owner A frame");
    let staged_a = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_ne!(staged_a.inject_batch_id, 0);
    assert_eq!(
        staged_a.inject_authorization_generation,
        reclaimed_generation_a
    );
    assert!(
        state
            .claim_input_owner(&peer_b, true)
            .await
            .expect("force owner B before retained revalidation")
    );
    let cancelled = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                inject_backpressure: true,
                held_input_authorization_generation: reclaimed_generation_a,
                ..Default::default()
            },
        )
        .await;
    assert!(cancelled.inject_batch_cancelled);
    assert!(!cancelled.held_input_authorized);
    assert_eq!(cancelled.inject_authorization_generation, 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn detach_acknowledges_completed_inject_batch_before_requeue() {
    let (state, root) = service_mode_broker_state("boundless-broker-detach-ack-test").await;
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
                sequence: 51,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseMove { dx: 5, dy: 0 }],
            },
        )
        .await
        .expect("route frame");
    let dispatched = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_ne!(dispatched.inject_batch_id, 0);

    assert!(
        state
            .detach_input_broker(
                allowed_client(),
                &attach.broker_token,
                &attach.delivery_epoch,
                dispatched.inject_batch_id,
            )
            .await,
        "cooperative detach must commit the tray's completed receipt before considering requeue"
    );

    let replacement = state
        .attach_input_broker(allowed_client(), "replacement-broker".to_string(), true)
        .await;
    assert!(replacement.accepted);
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("restore owner")
    );
    let after_detach = state
        .exchange_input_broker(
            allowed_client(),
            &replacement.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert!(after_detach.accepted);
    assert!(
        after_detach.inject_frames.is_empty(),
        "an acknowledged completed batch must not return after detach"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn detach_receipt_with_wrong_epoch_or_batch_id_fails_closed() {
    let (state, root) = service_mode_broker_state("boundless-broker-detach-receipt-test").await;
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
                sequence: 61,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events: vec![InputEvent::MouseMove { dx: 6, dy: 0 }],
            },
        )
        .await
        .expect("route frame");
    let dispatched = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations::default(),
        )
        .await;
    assert_ne!(dispatched.inject_batch_id, 0);

    assert!(
        !state
            .detach_input_broker(
                allowed_client(),
                &attach.broker_token,
                "wrong-delivery-epoch",
                dispatched.inject_batch_id,
            )
            .await,
        "a receipt from another daemon epoch must not detach or acknowledge"
    );
    assert!(state.input_broker_route_active());
    assert_eq!(
        state
            .input_broker_relay()
            .inflight_inject_batch()
            .map(|batch| batch.batch_id),
        Some(dispatched.inject_batch_id)
    );

    assert!(
        !state
            .detach_input_broker(
                allowed_client(),
                &attach.broker_token,
                &attach.delivery_epoch,
                dispatched.inject_batch_id.saturating_add(1),
            )
            .await,
        "an out-of-order receipt must not detach or requeue"
    );
    assert!(state.input_broker_route_active());
    assert_eq!(
        state
            .input_broker_relay()
            .inflight_inject_batch()
            .map(|batch| batch.batch_id),
        Some(dispatched.inject_batch_id)
    );

    assert!(
        state
            .detach_input_broker(
                allowed_client(),
                &attach.broker_token,
                &attach.delivery_epoch,
                dispatched.inject_batch_id,
            )
            .await
    );
    assert!(!state.input_broker_route_active());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn broker_observations_feed_capture_state_and_release_synthesis() {
    let (state, root) = service_mode_broker_state("boundless-broker-observations-test").await;
    let peer_id = join_connected_peer(&state).await;

    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    authorize_broker_capture(&state, &peer_id, &attach.broker_token).await;

    let outcome = state
        .exchange_input_broker(
            allowed_client(),
            &attach.broker_token,
            InputBrokerExchangeObservations {
                captured_events: vec![
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                        semantics: core_input::KeySemantics::Physical,
                    },
                    InputEvent::MouseMove { dx: 4, dy: 0 },
                ],
                cursor: Some((100, 200)),
                virtual_bounds: Some((0, 0, 1919, 1079)),
                lock_active: true,
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
            semantics: core_input::KeySemantics::Physical,
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
            Some(1),
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
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None, None)
        .await;
    assert!(staged.remote_payload.is_some());
    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("disconnect peer before local update");

    let local = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("new local".to_string())),
            Some(2),
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
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None, None)
        .await;
    assert!(
        next.remote_payload.is_none(),
        "the superseded remote payload must not reappear on the next poll"
    );

    state
        .set_peer_connected(&peer_id, true)
        .await
        .expect("reconnect peer");
    let replay = state.drain_outgoing(&peer_id).await;
    assert!(
        replay.iter().any(|payload| matches!(
            payload,
            OutboundPayload::ClipboardText { text } if text == "new local"
        )),
        "accepted disconnected local update must replay after reconnect: {replay:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn same_clipboard_sequence_retry_does_not_discard_newer_remote() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-clipboard-sequence-retry-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);

    let first = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("local".to_string())),
            Some(44),
            None,
        )
        .await;
    assert_eq!(
        first.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::Accepted
    );
    let _ = state.drain_outgoing(&peer_id).await;

    state
        .enqueue_remote_clipboard_text(&peer_id, "newer remote".to_string())
        .await
        .expect("enqueue newer remote");
    let staged = state
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None, None)
        .await;
    assert!(staged.remote_payload.is_some());

    let retry = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("local".to_string())),
            Some(44),
            None,
        )
        .await;
    assert_eq!(
        retry.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::Accepted
    );
    assert!(
        retry.remote_payload.is_some(),
        "same-sequence response-loss retry must not supersede a newer remote value"
    );

    let explicit_recopy = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("local".to_string())),
            Some(45),
            None,
        )
        .await;
    assert!(
        explicit_recopy.remote_payload.is_none(),
        "the same value with a new clipboard sequence is an explicit recopy and must supersede the remote value"
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
            Some(3),
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
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None, None)
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
            Some(4),
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

#[tokio::test]
async fn remote_apply_echo_does_not_supersede_newer_pending_remote() {
    let (state, root) =
        service_mode_broker_state("boundless-broker-clipboard-echo-order-test").await;
    let peer_id = join_connected_peer(&state).await;
    let attach = state
        .attach_input_broker(allowed_client(), "test-broker".to_string(), true)
        .await;
    assert!(attach.accepted);
    state
        .enqueue_remote_clipboard_text(&peer_id, "remote-a".to_string())
        .await
        .expect("enqueue remote A");
    let first = state
        .exchange_clipboard_broker(allowed_client(), &attach.broker_token, None, None, None)
        .await;
    let remote_a = first.remote_payload.expect("stage remote A");
    state
        .enqueue_remote_clipboard_text(&peer_id, "remote-b".to_string())
        .await
        .expect("enqueue newer remote B");

    let echoed = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("remote-a".to_string())),
            Some(77),
            Some(ClipboardBrokerApplyReport {
                source_peer_id: remote_a.peer_id,
                hash: remote_a.hash,
                applied: true,
                message: String::new(),
            }),
        )
        .await;
    assert_eq!(
        echoed.local_payload_disposition,
        ClipboardBrokerLocalPayloadDisposition::Accepted
    );
    assert!(
        matches!(
            echoed.remote_payload.as_ref().map(|item| &item.payload),
            Some(ClipboardPayload::Text(text)) if text == "remote-b"
        ),
        "remote echo A must not discard newer pending remote B: {echoed:?}"
    );

    let echo_retry = state
        .exchange_clipboard_broker(
            allowed_client(),
            &attach.broker_token,
            Some(ClipboardPayload::Text("remote-a".to_string())),
            Some(77),
            None,
        )
        .await;
    assert!(
        matches!(
            echo_retry.remote_payload.as_ref().map(|item| &item.payload),
            Some(ClipboardPayload::Text(text)) if text == "remote-b"
        ),
        "a response-loss retry of echo A must preserve staged remote B: {echo_retry:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}
