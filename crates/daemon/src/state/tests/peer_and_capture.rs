use super::*;

#[tokio::test]
async fn join_peer_requires_issued_code_and_consumes_it() {
    let root = std::env::temp_dir().join(format!("boundless-state-test-{}", uuid::Uuid::new_v4()));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let missing_err = state
        .join_peer("missing".to_string(), "127.0.0.1:15100".to_string(), None)
        .await
        .expect_err("must reject unknown code");
    assert!(
        missing_err
            .to_string()
            .contains("invalid or was already used")
    );

    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(code.clone(), "127.0.0.1:15100".to_string(), None)
        .await
        .expect("issued code should join");
    assert!(!peer_id.is_empty());

    let reused_err = state
        .join_peer(code, "127.0.0.1:15100".to_string(), None)
        .await
        .expect_err("reused code must fail");
    assert!(
        reused_err
            .to_string()
            .contains("invalid or was already used")
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn touch_peer_does_not_persist_config_on_heartbeat() {
    let root = std::env::temp_dir().join(format!("boundless-touch-test-{}", uuid::Uuid::new_v4()));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
        .expect("load state");

    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(code, "127.0.0.1:15100".to_string(), None)
        .await
        .expect("join");

    let before = std::fs::read_to_string(&config_path).expect("read before");
    state.touch_peer(&peer_id).await.expect("touch");
    let after = std::fs::read_to_string(&config_path).expect("read after");

    assert_eq!(before, after, "touch should not write config file");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remove_peer_clears_input_owner_and_allows_new_claim() {
    let root = std::env::temp_dir().join(format!(
        "boundless-remove-owner-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code_one, _) = state.create_pairing_code(120).await;
    let peer_one = state
        .join_peer(
            code_one,
            "127.0.0.1:15100".to_string(),
            Some("peer-one".to_string()),
        )
        .await
        .expect("join peer one");

    let (code_two, _) = state.create_pairing_code(120).await;
    let peer_two = state
        .join_peer(
            code_two,
            "127.0.0.1:15101".to_string(),
            Some("peer-two".to_string()),
        )
        .await
        .expect("join peer two");

    let claimed = state
        .claim_input_owner(&peer_one, false)
        .await
        .expect("claim owner");
    assert!(claimed);
    assert_eq!(
        state.input_owner().await.as_deref(),
        Some(peer_one.as_str())
    );

    let removed = state.remove_peer(&peer_one).await.expect("remove peer");
    assert!(removed);
    assert!(
        state.input_owner().await.is_none(),
        "owner should be cleared"
    );

    let claimed_second = state
        .claim_input_owner(&peer_two, false)
        .await
        .expect("claim second peer");
    assert!(
        claimed_second,
        "new claim should not be blocked by stale owner"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn input_capture_target_requires_known_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-capture-target-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let err = state
        .set_input_capture_target(Some("missing-peer"))
        .await
        .expect_err("unknown peer must fail");
    assert!(err.to_string().contains("unknown peer"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn set_peer_connected_rolls_back_in_memory_state_when_config_save_fails() {
    let root = std::env::temp_dir().join(format!(
        "boundless-peer-connect-save-fail-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let mut state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(
            code,
            "127.0.0.1:15100".to_string(),
            Some("peer".to_string()),
        )
        .await
        .expect("join peer");

    let blocked_path = root.join("blocked-config-path");
    std::fs::create_dir_all(&blocked_path).expect("create blocked path directory");
    *std::sync::Arc::make_mut(&mut state.config_path) = blocked_path;

    let error = state
        .set_peer_connected(&peer_id, true)
        .await
        .expect_err("save failure should bubble up");
    assert!(
        error.to_string().contains("write"),
        "unexpected error: {error:#}"
    );

    let peer = state
        .get_peer(&peer_id)
        .await
        .expect("peer must still exist");
    assert!(
        !peer.connected,
        "failed persistence must not leave the in-memory peer marked connected"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn active_input_capture_target_requires_connected_peer_and_feature_enabled() {
    let root = std::env::temp_dir().join(format!(
        "boundless-capture-active-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(
            code,
            "127.0.0.1:15100".to_string(),
            Some("peer".to_string()),
        )
        .await
        .expect("join peer");

    let set = state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set capture target");
    assert_eq!(set.as_deref(), Some(peer_id.as_str()));
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(peer_id.as_str())
    );
    assert!(
        state.active_input_capture_target().await.is_none(),
        "disconnected peer must not be capture-active"
    );

    state
        .set_peer_connected(&peer_id, true)
        .await
        .expect("connect peer");
    assert_eq!(
        state.active_input_capture_target().await.as_deref(),
        Some(peer_id.as_str())
    );

    state
        .set_feature("share_input".to_string(), false)
        .await
        .expect("disable input share");
    assert!(
        state.active_input_capture_target().await.is_none(),
        "disabled share_input must block capture"
    );

    state
        .set_feature("share_input".to_string(), true)
        .await
        .expect("enable input share");
    assert_eq!(
        state.active_input_capture_target().await.as_deref(),
        Some(peer_id.as_str())
    );

    state.clear_input_capture_target().await;
    assert!(state.input_capture_target().await.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remove_peer_clears_input_capture_target() {
    let root = std::env::temp_dir().join(format!(
        "boundless-remove-capture-target-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

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
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set target");
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(peer_id.as_str())
    );

    state.remove_peer(&peer_id).await.expect("remove");
    assert!(
        state.input_capture_target().await.is_none(),
        "removed peer should clear capture target"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn disconnect_peer_clears_input_capture_target() {
    let root = std::env::temp_dir().join(format!(
        "boundless-disconnect-capture-target-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

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
    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set target");
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(peer_id.as_str())
    );

    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("disconnect peer");
    assert!(
        state.input_capture_target().await.is_none(),
        "disconnected peer should clear capture target"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn switch_all_capture_target_cycles_connected_layout_peers() {
    let root = std::env::temp_dir().join(format!(
        "boundless-switch-all-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code_one, _) = state.create_pairing_code(120).await;
    let left_peer = state
        .join_peer(
            code_one,
            "127.0.0.1:15100".to_string(),
            Some("left".to_string()),
        )
        .await
        .expect("join left");

    let (code_two, _) = state.create_pairing_code(120).await;
    let right_peer = state
        .join_peer(
            code_two,
            "127.0.0.1:15101".to_string(),
            Some("right".to_string()),
        )
        .await
        .expect("join right");

    state
        .set_layout("right,self,left".to_string())
        .await
        .expect("set layout");
    state
        .set_peer_connected(&left_peer, true)
        .await
        .expect("connect left");
    state
        .set_peer_connected(&right_peer, true)
        .await
        .expect("connect right");

    assert_eq!(
        state.apply_switch_all_capture_target().await.as_deref(),
        Some(right_peer.as_str())
    );
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(right_peer.as_str())
    );

    assert_eq!(
        state.apply_switch_all_capture_target().await.as_deref(),
        Some(left_peer.as_str())
    );
    assert_eq!(
        state.input_capture_target().await.as_deref(),
        Some(left_peer.as_str())
    );

    state
        .set_peer_connected(&left_peer, false)
        .await
        .expect("disconnect left");
    assert_eq!(
        state.apply_switch_all_capture_target().await.as_deref(),
        Some(right_peer.as_str()),
        "disconnected peers must be skipped from switch-all rotation"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn abort_transport_sessions_for_peer_cancels_registered_tasks() {
    let root = std::env::temp_dir().join(format!(
        "boundless-transport-abort-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code, _) = state.create_pairing_code(120).await;
    let peer_id = state
        .join_peer(
            code,
            "127.0.0.1:15100".to_string(),
            Some("peer".to_string()),
        )
        .await
        .expect("join peer");

    let session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    state
        .register_transport_session_for_peer(&peer_id, session.abort_handle())
        .await;

    let aborted = state.abort_transport_sessions_for_peer(&peer_id).await;
    assert_eq!(aborted, 1);
    let join_error = session.await.expect_err("session should be aborted");
    assert!(join_error.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn request_peer_reconnect_and_reset_clears_input_state_and_sessions() {
    let root = std::env::temp_dir().join(format!(
        "boundless-reconnect-reset-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

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
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner"),
        "owner claim should succeed"
    );
    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set capture target");

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 1,
                timestamp_unix_ms: 1,
                events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
            },
        )
        .await
        .expect("queue incoming frame");

    let session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    state
        .register_transport_session_for_peer(&peer_id, session.abort_handle())
        .await;

    let (generation, aborted_sessions) = state
        .request_peer_reconnect_and_reset(&peer_id)
        .await
        .expect("request reconnect reset");
    assert!(generation > 0, "reconnect generation should increment");
    assert_eq!(aborted_sessions, 1, "active session should be aborted");

    assert_eq!(state.input_owner().await, None, "owner should be released");
    assert!(
        state.input_capture_target().await.is_none(),
        "capture target should be cleared"
    );
    assert!(
        state.dequeue_pending_inject_input_frame().await.is_none(),
        "pending inject frames should be cleared"
    );
    let peer = state
        .get_peer(&peer_id)
        .await
        .expect("peer must still exist");
    assert!(!peer.connected, "peer should be marked disconnected");
    assert!(
        state.peer_reconnect_generation(&peer_id).await >= generation,
        "reconnect generation should be visible"
    );

    let join_error = session.await.expect_err("session should be aborted");
    assert!(join_error.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn request_all_peers_reconnect_and_reset_clears_shared_input_state() {
    let root = std::env::temp_dir().join(format!(
        "boundless-reconnect-reset-all-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code_one, _) = state.create_pairing_code(120).await;
    let peer_one = state
        .join_peer(
            code_one,
            "127.0.0.1:15100".to_string(),
            Some("peer-one".to_string()),
        )
        .await
        .expect("join peer one");
    let (code_two, _) = state.create_pairing_code(120).await;
    let peer_two = state
        .join_peer(
            code_two,
            "127.0.0.1:15101".to_string(),
            Some("peer-two".to_string()),
        )
        .await
        .expect("join peer two");

    state
        .set_peer_connected(&peer_one, true)
        .await
        .expect("connect peer one");
    state
        .set_peer_connected(&peer_two, true)
        .await
        .expect("connect peer two");
    assert!(
        state
            .claim_input_owner(&peer_one, false)
            .await
            .expect("claim owner"),
        "owner claim should succeed"
    );
    state
        .set_input_capture_target(Some(&peer_one))
        .await
        .expect("set capture target");

    state
        .route_incoming_input_frame(
            &peer_one,
            InputFrame {
                source_peer_id: peer_one.clone(),
                sequence: 1,
                timestamp_unix_ms: 1,
                events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
            },
        )
        .await
        .expect("queue incoming frame");

    let session_one = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let session_two = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    state
        .register_transport_session_for_peer(&peer_one, session_one.abort_handle())
        .await;
    state
        .register_transport_session_for_peer(&peer_two, session_two.abort_handle())
        .await;

    let (disconnected, aborted_sessions) = state
        .request_all_peers_reconnect_and_reset()
        .await
        .expect("request all reconnect reset");
    assert_eq!(
        disconnected, 2,
        "both connected peers should be disconnected"
    );
    assert_eq!(
        aborted_sessions, 2,
        "both active sessions should be aborted"
    );

    assert_eq!(state.input_owner().await, None, "owner should be released");
    assert!(
        state.input_capture_target().await.is_none(),
        "capture target should be cleared"
    );
    assert!(
        state.dequeue_pending_inject_input_frame().await.is_none(),
        "pending inject frames should be cleared"
    );

    let peers = state.list_peers().await;
    assert!(
        peers.iter().all(|peer| !peer.connected),
        "all peers should be marked disconnected"
    );
    assert!(
        state.peer_reconnect_generation(&peer_one).await > 0,
        "peer one reconnect generation should increment"
    );
    assert!(
        state.peer_reconnect_generation(&peer_two).await > 0,
        "peer two reconnect generation should increment"
    );

    let join_error_one = session_one
        .await
        .expect_err("session one should be aborted");
    assert!(join_error_one.is_cancelled());
    let join_error_two = session_two
        .await
        .expect_err("session two should be aborted");
    assert!(join_error_two.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}
