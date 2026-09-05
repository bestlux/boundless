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
async fn set_peer_connected_does_not_persist_already_disconnected_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-disconnected-persist-test-{}",
        uuid::Uuid::new_v4()
    ));
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
    let reconcile = state.peer_reconcile_wake_signal();
    let _ = reconcile.take_pending();
    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("mark disconnected");
    let after = std::fs::read_to_string(&config_path).expect("read after");

    assert_eq!(
        before, after,
        "already-disconnected retry should not rewrite config"
    );
    assert!(
        !reconcile.take_pending(),
        "unchanged connection must not wake its retry supervisor"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn set_feature_rejects_unsupported_network_policy_names() {
    let root = std::env::temp_dir().join(format!(
        "boundless-feature-policy-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let err = state
        .set_feature("same_subnet_only".to_string(), true)
        .await
        .expect_err("unsupported network policy must fail");
    assert!(err.to_string().contains("unsupported"));
    assert!(
        !state.feature_map().await.contains_key("same_subnet_only"),
        "unsupported feature must not persist"
    );

    let unknown_err = state
        .set_feature("not_a_real_feature".to_string(), true)
        .await
        .expect_err("unknown feature must fail");
    assert!(unknown_err.to_string().contains("unknown feature"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn set_hotkey_rejects_semantic_duplicate_bindings() {
    let root = std::env::temp_dir().join(format!(
        "boundless-hotkey-duplicate-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
    state
        .set_hotkey("toggle_easy_mouse".to_string(), "Ctrl+Alt+R".to_string())
        .await
        .expect("set first hotkey");
    let err = state
        .set_hotkey("switch_all".to_string(), "Alt+Ctrl+R".to_string())
        .await
        .expect_err("semantic duplicate must fail");
    assert!(err.to_string().contains("already assigned"));

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
async fn peer_runtime_transitions_and_input_release_survive_unwritable_config() {
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

    state
        .set_peer_connected(&peer_id, true)
        .await
        .expect("runtime connection does not write configuration");
    assert!(state.get_peer(&peer_id).await.unwrap().connected);
    state
        .claim_input_owner(&peer_id, false)
        .await
        .expect("claim owner");
    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("capture target");
    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("disconnect must fail open even when disk is unwritable");
    assert!(!state.get_peer(&peer_id).await.unwrap().connected);
    assert!(state.input_owner().await.is_none());
    assert!(state.input_capture_target().await.is_none());

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
    state.register_transport_session_for_peer(&peer_id, session.abort_handle());

    let aborted = state.abort_transport_sessions_for_peer(&peer_id).await;
    assert_eq!(aborted, 1);
    let join_error = session.await.expect_err("session should be aborted");
    assert!(join_error.is_cancelled());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn shutdown_abort_cancels_all_transport_sessions_without_clearing_diagnostics() {
    let root = std::env::temp_dir().join(format!(
        "boundless-transport-shutdown-abort-test-{}",
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

    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "local".to_string(),
        kind: "shutdown_test_event".to_string(),
        peer_id: "none".to_string(),
        detail: "before_shutdown_abort".to_string(),
        size_bytes: 0,
    });

    let pending_session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    state.register_pending_transport_session(pending_session.abort_handle());

    let peer_session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    state.register_transport_session_for_peer(&peer_id, peer_session.abort_handle());
    let pending_bind_session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let pending_bind_session_id =
        state.register_pending_transport_session(pending_bind_session.abort_handle());

    state.begin_transport_session_shutdown();

    assert!(
        !state.bind_pending_transport_session_to_peer(pending_bind_session_id, &peer_id),
        "pending sessions must not bind to peers after shutdown begins"
    );

    let late_pending_session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let late_pending_id =
        state.register_pending_transport_session(late_pending_session.abort_handle());
    assert_eq!(
        late_pending_id, 0,
        "pending sessions registered after shutdown begins should be refused"
    );

    let late_peer_session = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let late_peer_session_id =
        state.register_transport_session_for_peer(&peer_id, late_peer_session.abort_handle());
    assert_eq!(
        late_peer_session_id, 0,
        "peer sessions registered after shutdown begins should be refused"
    );

    let late_pending_error = late_pending_session
        .await
        .expect_err("late pending session should be aborted immediately");
    assert!(late_pending_error.is_cancelled());
    let late_peer_error = late_peer_session
        .await
        .expect_err("late peer session should be aborted immediately");
    assert!(late_peer_error.is_cancelled());
    let pending_bind_error = pending_bind_session
        .await
        .expect_err("pending bind session should be aborted during shutdown bind");
    assert!(pending_bind_error.is_cancelled());

    let aborted = state.abort_all_transport_sessions_for_shutdown().await;
    assert_eq!(
        aborted, 2,
        "pending and peer-bound sessions should both be aborted"
    );

    let pending_error = pending_session
        .await
        .expect_err("pending session should be aborted");
    assert!(pending_error.is_cancelled());
    let peer_error = peer_session
        .await
        .expect_err("peer session should be aborted");
    assert!(peer_error.is_cancelled());

    assert_eq!(
        state.abort_all_transport_sessions_for_shutdown().await,
        0,
        "shutdown abort should drain session registrations"
    );
    let events = state.transport_events().await;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "shutdown_test_event"),
        "shutdown abort must not clear transport diagnostics"
    );

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
    state.register_transport_session_for_peer(&peer_id, session.abort_handle());

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
    state.register_transport_session_for_peer(&peer_one, session_one.abort_handle());
    state.register_transport_session_for_peer(&peer_two, session_two.abort_handle());

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

#[tokio::test]
async fn legacy_connected_config_is_not_restored_or_repersisted() {
    let root = std::env::temp_dir().join(format!(
        "boundless-volatile-peer-test-{}",
        uuid::Uuid::new_v4()
    ));
    let path = root.join("config.json");
    let state =
        AppState::load_or_create_with_paths(path.clone(), root.join("security")).expect("state");
    let remote = AppState::load_or_create_with_paths(
        root.join("remote/config.json"),
        root.join("remote/security"),
    )
    .expect("remote state");
    let mut bundle = remote.export_trust_bundle().await.expect("remote trust");
    bundle.network_address = "127.0.0.1:15100".into();
    let peer = bundle.machine_id.clone();
    state
        .import_trust_bundle(bundle, Some("retained peer".into()))
        .await
        .expect("import peer trust");
    let original_trust = state.trusted_records().await.expect("trust");
    state
        .set_feature("share_clipboard".into(), false)
        .await
        .expect("custom feature");
    state
        .set_layout_and_queue_sync(format!("self,{peer}"))
        .await
        .expect("custom layout");
    let original = std::fs::read(&path).expect("config");
    state
        .set_peer_connected(&peer, true)
        .await
        .expect("connected");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "runtime transitions must not write settings"
    );
    let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
    legacy["config_version"] = serde_json::json!("5");
    legacy["peers"][0]["connected"] = serde_json::json!(true);
    legacy["peers"][0]["last_seen"] = serde_json::json!("2099-01-01T00:00:00Z");
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let reloaded =
        AppState::load_or_create_with_paths(path.clone(), root.join("security")).unwrap();
    let old_snapshot = state.snapshot().await;
    let new_snapshot = reloaded.snapshot().await;
    assert_eq!(new_snapshot.config_version, "6");
    assert_eq!(old_snapshot.machine_id, new_snapshot.machine_id);
    assert_eq!(old_snapshot.peers[0].address, new_snapshot.peers[0].address);
    assert_eq!(old_snapshot.peers[0].peer_id, new_snapshot.peers[0].peer_id);
    assert_eq!(old_snapshot.layout_matrix, new_snapshot.layout_matrix);
    assert_eq!(old_snapshot.features, new_snapshot.features);
    assert_eq!(
        state.identity().device_cert_pem,
        reloaded.identity().device_cert_pem
    );
    let reloaded_trust = reloaded.trusted_records().await.expect("reloaded trust");
    assert_eq!(original_trust.len(), reloaded_trust.len());
    for original in original_trust {
        assert!(reloaded_trust.iter().any(|record| {
            record.machine_id == original.machine_id
                && record.ca_cert_pem == original.ca_cert_pem
                && (record.machine_id != peer || record.added_at == original.added_at)
        }));
    }
    assert!(!reloaded.get_peer(&peer).await.unwrap().connected);
    assert!(reloaded.get_peer(&peer).await.unwrap().last_seen < chrono::Utc::now());
    reloaded
        .update_bind("127.0.0.1:50052".into())
        .await
        .expect("save settings");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved["peers"][0].get("connected").is_none());
    assert!(saved["peers"][0].get("last_seen").is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn reimported_trust_resets_runtime_ownership_after_durable_save() {
    let root = std::env::temp_dir().join(format!(
        "boundless-reimport-runtime-test-{}",
        uuid::Uuid::new_v4()
    ));
    let state =
        AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
            .expect("state");
    let remote = AppState::load_or_create_with_paths(
        root.join("remote/config.json"),
        root.join("remote/security"),
    )
    .expect("remote");
    let mut bundle = remote.export_trust_bundle().await.expect("trust bundle");
    bundle.network_address = "127.0.0.1:15100".into();
    let peer = bundle.machine_id.clone();
    state
        .import_trust_bundle(bundle.clone(), None)
        .await
        .unwrap();
    let child = tokio::spawn(std::future::pending::<()>());
    let session_id = state.register_transport_session_for_peer(&peer, child.abort_handle());
    state
        .claim_transport_session(
            &peer,
            session_id,
            true,
            Arc::new(RuntimeWakeSignal::default()),
        )
        .await;
    state.set_peer_connected(&peer, true).await.unwrap();
    state.claim_input_owner(&peer, false).await.unwrap();
    state.set_input_capture_target(Some(&peer)).await.unwrap();
    state.update_bind("127.0.0.1:50052".into()).await.unwrap();
    assert!(state.get_peer(&peer).await.unwrap().connected);
    assert!(state.has_active_transport_session(&peer));

    state
        .import_trust_bundle(bundle, Some("updated".into()))
        .await
        .unwrap();
    assert!(
        child
            .await
            .expect_err("old trusted route aborted")
            .is_cancelled()
    );
    assert!(!state.get_peer(&peer).await.unwrap().connected);
    assert!(!state.has_active_transport_session(&peer));
    assert!(state.input_owner().await.is_none());
    assert!(state.input_capture_target().await.is_none());
    let _ = std::fs::remove_dir_all(root);
}
