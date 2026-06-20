use super::*;

#[tokio::test]
async fn store_incoming_file_rejects_unsafe_name() {
    let root = std::env::temp_dir().join(format!(
        "boundless-incoming-file-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let err = state
        .store_incoming_file("peer-a", "../evil.txt", b"bad".to_vec())
        .await
        .expect_err("must reject unsafe file name");
    assert!(err.to_string().contains("path separators"));

    let escaped_path = root.join("evil.txt");
    assert!(!escaped_path.exists(), "unsafe path must never be created");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn store_incoming_file_uses_configured_receive_dir() {
    let root = std::env::temp_dir().join(format!(
        "boundless-incoming-file-receive-dir-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let receive_dir = root.join("received");

    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = receive_dir.display().to_string();
    save_config_at(&config_path, &config).expect("seed config");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let final_path = state
        .store_incoming_file("peer-a", "report.txt", b"payload".to_vec())
        .await
        .expect("store incoming file");

    assert_eq!(final_path.parent(), Some(receive_dir.as_path()));
    assert_eq!(
        final_path.file_name().and_then(|name| name.to_str()),
        Some("report.txt")
    );
    assert_eq!(
        std::fs::read(&final_path).expect("read stored file"),
        b"payload"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn store_incoming_file_from_temp_error_leaves_no_visible_partial() {
    let root = std::env::temp_dir().join(format!(
        "boundless-incoming-file-temp-error-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let receive_dir = root.join("received");

    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = receive_dir.display().to_string();
    save_config_at(&config_path, &config).expect("seed config");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
    let missing_temp = root.join("missing-temp.bin");

    let err = state
        .store_incoming_file_from_temp("peer-a", "report.txt", &missing_temp, 7)
        .await
        .expect_err("missing temp source should fail");
    assert!(
        err.to_string().contains("open inbound temp source"),
        "unexpected error: {err:?}"
    );

    assert!(
        !receive_dir.join("report.txt").exists(),
        "failed fallback copy must not expose final path"
    );
    let leftovers = std::fs::read_dir(&receive_dir)
        .expect("receive dir")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("list receive dir");
    assert!(
        leftovers.is_empty(),
        "failed fallback copy should remove reserved .part files"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn reserve_incoming_file_allocates_same_name_conflicts_exclusively() {
    let root = std::env::temp_dir().join(format!(
        "boundless-incoming-file-conflict-reserve-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let receive_dir = root.join("received");

    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = receive_dir.display().to_string();
    save_config_at(&config_path, &config).expect("seed config");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let mut first = state
        .reserve_incoming_file("peer-a", "report.txt", 3)
        .await
        .expect("reserve first");
    let mut second = state
        .reserve_incoming_file("peer-a", "report.txt", 3)
        .await
        .expect("reserve second");

    assert_eq!(first.final_path, receive_dir.join("report.txt"));
    assert_eq!(second.final_path, receive_dir.join("report (1).txt"));
    assert!(first.temp_path.exists());
    assert!(second.temp_path.exists());
    assert!(
        !first.final_path.exists() && !second.final_path.exists(),
        "reserved receives must not expose partial final paths"
    );

    tokio::io::AsyncWriteExt::write_all(&mut first.temp_file, b"one")
        .await
        .expect("write first");
    first.temp_file.sync_all().await.expect("sync first");
    tokio::io::AsyncWriteExt::write_all(&mut second.temp_file, b"two")
        .await
        .expect("write second");
    second.temp_file.sync_all().await.expect("sync second");
    drop(first.temp_file);
    drop(second.temp_file);

    state
        .complete_incoming_file(
            "peer-a",
            first.sanitized_name.clone(),
            &first.temp_path,
            &first.final_path,
            3,
        )
        .await
        .expect("complete first");
    state
        .complete_incoming_file(
            "peer-a",
            second.sanitized_name.clone(),
            &second.temp_path,
            &second.final_path,
            3,
        )
        .await
        .expect("complete second");

    assert_eq!(std::fs::read(&first.final_path).expect("first"), b"one");
    assert_eq!(std::fs::read(&second.final_path).expect("second"), b"two");
    assert!(!first.temp_path.exists());
    assert!(!second.temp_path.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn store_incoming_file_uses_safe_peer_directory_name() {
    let root = std::env::temp_dir().join(format!(
        "boundless-incoming-file-peer-dir-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let receive_dir = root.join("received");

    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = receive_dir.display().to_string();
    config.file_transfer.organize_by_peer = true;
    save_config_at(&config_path, &config).expect("seed config");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let final_path = state
        .store_incoming_file(r"..\escaped-peer", "report.txt", b"payload".to_vec())
        .await
        .expect("store incoming file");

    assert!(final_path.starts_with(&receive_dir));
    assert_ne!(final_path.parent(), Some(receive_dir.as_path()));
    assert!(
        final_path
            .parent()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("peer-"))
    );
    assert!(!root.join("escaped-peer").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn update_file_transfer_config_persists_receive_dir() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-transfer-config-update-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let receive_dir = root.join("custom-receive");

    let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
        .expect("load state");

    let mut config = state.file_transfer_config().await;
    config.receive_dir = receive_dir.display().to_string();
    state
        .update_file_transfer_config(config)
        .await
        .expect("update file transfer config");

    let final_path = state
        .store_incoming_file("peer-a", "report.txt", b"payload".to_vec())
        .await
        .expect("store incoming file");
    assert_eq!(final_path.parent(), Some(receive_dir.as_path()));

    let reloaded = load_or_create_config_at(&config_path).expect("reload config");
    assert_eq!(
        reloaded.file_transfer.receive_dir,
        receive_dir.display().to_string()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn file_transfer_uses_configured_size_limit() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-transfer-configured-limit-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");

    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
    let mut config = state.file_transfer_config().await;
    config.max_file_bytes = 2;
    state
        .update_file_transfer_config(config)
        .await
        .expect("update file transfer config");

    let err = state
        .store_incoming_file("peer-a", "too-large.txt", b"abc".to_vec())
        .await
        .expect_err("configured limit must reject oversized file");
    assert!(err.to_string().contains("exceeds transfer limit"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remove_queued_file_transfer_drops_deferred_cursor() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-transfer-remove-queued-test-{}",
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
    let file_path = root.join("flow.bin");
    tokio::fs::write(
        &file_path,
        vec![9u8; crate::state::FILE_TRANSFER_CHUNK_BYTES + 7],
    )
    .await
    .expect("write payload");

    state
        .queue_file_from_path(&peer_id, &file_path)
        .await
        .expect("queue file");
    let mut queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
    let transfer_id = match queued.first() {
        Some(OutboundPayload::FileStart { transfer_id, .. }) => transfer_id.clone(),
        other => panic!("expected first payload to be file start, got {other:?}"),
    };
    queued.remove(0);
    state.requeue_outgoing_front(&peer_id, queued).await;

    state
        .remove_queued_file_transfer(&peer_id, &transfer_id)
        .await;

    assert!(
        state
            .drain_outgoing_bulk(&peer_id, usize::MAX)
            .await
            .is_empty()
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disconnect_clears_inbound_sequence_state_for_reconnect() {
    let root = std::env::temp_dir().join(format!(
        "boundless-reconnect-seq-test-{}",
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

    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim")
    );
    let first = state
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
        .expect("first route");
    assert!(matches!(first, RouteDecision::Applied { .. }));

    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("disconnect");
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("re-claim")
    );

    let second = state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 1,
                timestamp_unix_ms: 2,
                events: vec![InputEvent::MouseMove { dx: 2, dy: 2 }],
            },
        )
        .await
        .expect("sequence should restart after disconnect");
    assert!(matches!(second, RouteDecision::Applied { .. }));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn route_incoming_input_frame_queues_for_injection_when_applied() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-queue-test-{}",
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

    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner")
    );

    let decision = state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: 1,
                timestamp_unix_ms: 1,
                events: vec![
                    InputEvent::MouseMove { dx: 2, dy: -1 },
                    InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    },
                ],
            },
        )
        .await
        .expect("route");
    assert!(matches!(
        decision,
        RouteDecision::Applied { event_count: 2 }
    ));

    let queued = state
        .dequeue_pending_inject_input_frame()
        .await
        .expect("queued frame");
    assert_eq!(queued.peer_id, peer_id);
    assert_eq!(queued.sequence, 1);
    assert_eq!(queued.events.len(), 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn route_incoming_input_frame_auto_claims_owner_when_missing() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-auto-claim-test-{}",
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

    let decision = state
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
        .expect("route");
    assert!(matches!(decision, RouteDecision::Applied { .. }));
    assert_eq!(state.input_owner().await.as_deref(), Some(peer_id.as_str()));

    let incoming = state
        .transport_events()
        .await
        .into_iter()
        .find(|event| event.kind == "input_frame" && event.direction == "incoming")
        .expect("incoming event");
    assert!(incoming.detail.contains("auto_claimed_owner=true"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn route_incoming_input_frame_auto_steals_owner_when_mismatched() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-auto-steal-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let (code_a, _) = state.create_pairing_code(120).await;
    let peer_a = state
        .join_peer(
            code_a,
            "127.0.0.1:15100".to_string(),
            Some("peer-a".to_string()),
        )
        .await
        .expect("join peer-a");
    let (code_b, _) = state.create_pairing_code(120).await;
    let peer_b = state
        .join_peer(
            code_b,
            "127.0.0.1:15101".to_string(),
            Some("peer-b".to_string()),
        )
        .await
        .expect("join peer-b");

    assert!(
        state
            .claim_input_owner(&peer_a, false)
            .await
            .expect("claim")
    );
    assert_eq!(state.input_owner().await.as_deref(), Some(peer_a.as_str()));

    let decision = state
        .route_incoming_input_frame(
            &peer_b,
            InputFrame {
                source_peer_id: peer_b.clone(),
                sequence: 1,
                timestamp_unix_ms: 2,
                events: vec![InputEvent::MouseMove { dx: 2, dy: 2 }],
            },
        )
        .await
        .expect("route");
    assert!(matches!(decision, RouteDecision::Applied { .. }));
    assert_eq!(state.input_owner().await.as_deref(), Some(peer_b.as_str()));

    let incoming = state
        .transport_events()
        .await
        .into_iter()
        .find(|event| {
            event.kind == "input_frame" && event.direction == "incoming" && event.peer_id == peer_b
        })
        .expect("incoming event");
    assert!(incoming.detail.contains("auto_claimed_owner=true"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn route_incoming_input_frame_records_latency_detail_fields() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-latency-detail-test-{}",
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
                timestamp_unix_ms: 1,
                events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
            },
        )
        .await
        .expect("route");

    let events = state.transport_events().await;
    let incoming = events
        .iter()
        .find(|event| event.kind == "input_frame" && event.direction == "incoming")
        .expect("incoming input frame event");
    assert!(incoming.detail.contains("sequence=42"));
    assert!(incoming.detail.contains("capture_to_receive_ms="));

    let queued = events
        .iter()
        .find(|event| event.kind == "input_inject_queued" && event.direction == "local")
        .expect("queued input inject event");
    assert!(queued.detail.contains("sequence=42"));
    assert!(queued.detail.contains("capture_to_queue_ms="));
    assert!(queued.detail.contains("receive_to_queue_ms="));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn update_input_handoff_config_persists_policy_and_notifies_capture_runtime() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-handoff-config-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
        .expect("load state");

    let signal = state.input_capture_wake_signal();
    let notified = signal.notified();
    tokio::pin!(notified);
    let next = InputHandoffConfig {
        block_screen_corners: false,
        corner_block_px: 12,
        relative_mouse: false,
        hide_cursor_at_edge: true,
        draw_cursor_marker: true,
    };
    state
        .update_input_handoff_config(next.clone())
        .await
        .expect("update handoff config");

    tokio::time::timeout(std::time::Duration::from_millis(200), &mut notified)
        .await
        .expect("capture signal should fire after input policy update");
    assert!(signal.take_pending());

    let reloaded = load_or_create_config_at(&config_path).expect("reload config");
    assert_eq!(reloaded.input_handoff, next);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn input_runtime_stats_expose_capture_backend_mode_and_queue_depth() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-runtime-stats-test-{}",
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
        .claim_input_owner(&peer_id, false)
        .await
        .expect("claim owner");

    state.set_input_capture_backend_mode("scripted").await;
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
        .expect("route");

    assert_eq!(state.input_capture_backend_mode().await, "scripted");
    assert_eq!(state.pending_inject_frame_stats().await, (1, 1));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn transport_events_ring_buffer_keeps_most_recent_records() {
    let root = std::env::temp_dir().join(format!(
        "boundless-transport-event-ring-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    for index in 0..(MAX_TRANSPORT_EVENTS + 8) {
        state.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "ring_probe".to_string(),
            peer_id: "peer-a".to_string(),
            detail: format!("idx={index}"),
            size_bytes: index as u64,
        });
    }

    let events = state.transport_events().await;
    assert_eq!(events.len(), MAX_TRANSPORT_EVENTS);
    assert!(
        events
            .first()
            .is_some_and(|event| event.detail == "idx=8" && event.size_bytes == 8),
        "oldest retained event should reflect dropped head records"
    );
    assert!(
        events
            .last()
            .is_some_and(|event| event.detail == format!("idx={}", MAX_TRANSPORT_EVENTS + 7)),
        "newest event should always be retained"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn disconnect_clears_pending_injection_frames_for_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-clear-queue-test-{}",
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
                timestamp_unix_ms: 1,
                events: vec![InputEvent::MouseMove { dx: 1, dy: 1 }],
            },
        )
        .await
        .expect("route");

    state
        .set_peer_connected(&peer_id, false)
        .await
        .expect("disconnect");
    assert!(
        state.dequeue_pending_inject_input_frame().await.is_none(),
        "disconnect should clear queued injection frames for that peer"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn pending_input_injection_queue_coalesces_adjacent_move_frames() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-coalesce-test-{}",
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

    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner")
    );

    for sequence in 1..=2u64 {
        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence,
                    timestamp_unix_ms: sequence as i64,
                    events: vec![InputEvent::MouseMove {
                        dx: sequence as i32,
                        dy: 1,
                    }],
                },
            )
            .await
            .expect("route");
    }

    let merged = state
        .dequeue_pending_inject_input_frame()
        .await
        .expect("first queued");
    assert_eq!(
        merged.sequence, 2,
        "merged frame should keep newest sequence"
    );
    assert!(matches!(
        merged.events.as_slice(),
        [InputEvent::MouseMove { dx, dy }] if *dx == 3 && *dy == 2
    ));
    assert!(
        state.dequeue_pending_inject_input_frame().await.is_none(),
        "adjacent move frames should collapse into one queue entry"
    );

    let events = state.transport_events().await;
    assert!(
        events.iter().any(|event| {
            event.kind == "input_queue_coalesced"
                && event.detail.contains("queue=inject")
                && event.peer_id == peer_id
        }),
        "inject coalescing should be observable in diagnostics"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn pending_input_injection_queue_drops_new_move_when_full_of_non_move_frames() {
    let root = std::env::temp_dir().join(format!(
        "boundless-input-overflow-test-{}",
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

    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner")
    );

    for sequence in 1..=(MAX_PENDING_INJECT_INPUT_FRAMES as u64) {
        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence,
                    timestamp_unix_ms: sequence as i64,
                    events: vec![InputEvent::Key {
                        scan_code: (sequence % 64) as u16 + 1,
                        state: KeyState::Down,
                    }],
                },
            )
            .await
            .expect("route");
    }

    state
        .route_incoming_input_frame(
            &peer_id,
            InputFrame {
                source_peer_id: peer_id.clone(),
                sequence: MAX_PENDING_INJECT_INPUT_FRAMES as u64 + 1,
                timestamp_unix_ms: 999,
                events: vec![InputEvent::MouseMove { dx: 5, dy: 7 }],
            },
        )
        .await
        .expect("route overflow");

    let first = state
        .dequeue_pending_inject_input_frame()
        .await
        .expect("first queued");
    assert_eq!(
        first.sequence, 1,
        "new move should be dropped before older non-move control events"
    );

    let mut count = 1usize;
    while state.dequeue_pending_inject_input_frame().await.is_some() {
        count += 1;
    }
    assert_eq!(count, MAX_PENDING_INJECT_INPUT_FRAMES);

    let events = state.transport_events().await;
    assert!(
        events.iter().any(|event| {
            event.kind == "input_queue_overflow_drop"
                && event.detail.contains("queue=inject")
                && event.detail.contains("reason=drop_new_move")
        }),
        "overflow policy should record why the move frame was dropped"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_input_events_validates_size_and_increments_sequence() {
    let root = std::env::temp_dir().join(format!(
        "boundless-queue-input-events-test-{}",
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

    let empty_err = state
        .queue_input_events(&peer_id, Vec::new())
        .await
        .expect_err("empty events must fail");
    assert!(empty_err.to_string().contains("at least one event"));

    let too_many = vec![InputEvent::MouseMove { dx: 1, dy: 1 }; MAX_EVENTS_PER_FRAME + 1];
    let too_many_err = state
        .queue_input_events(&peer_id, too_many)
        .await
        .expect_err("oversized frame must fail");
    assert!(too_many_err.to_string().contains("exceeds limit"));

    state
        .queue_input_events(
            &peer_id,
            vec![
                InputEvent::MouseMove { dx: 2, dy: 3 },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                },
            ],
        )
        .await
        .expect("queue frame 1");
    state
        .queue_input_events(
            &peer_id,
            vec![InputEvent::Key {
                scan_code: 30,
                state: KeyState::Up,
            }],
        )
        .await
        .expect("queue frame 2");

    let queued = state.drain_outgoing(&peer_id).await;
    assert_eq!(queued.len(), 2);
    assert!(matches!(
        queued.first(),
        Some(OutboundPayload::InputFrame { sequence: 1, events, .. }) if events.len() == 2
    ));
    assert!(matches!(
        queued.get(1),
        Some(OutboundPayload::InputFrame { sequence: 2, events, .. }) if events.len() == 1
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_input_events_notifies_outgoing_flush_signal() {
    let root = std::env::temp_dir().join(format!(
        "boundless-outgoing-flush-signal-test-{}",
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

    let mut flush_signal = state.subscribe_outgoing_flush_signal();
    state
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 1, dy: 1 }])
        .await
        .expect("queue frame");

    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        flush_signal.changed(),
    )
    .await
    .expect("flush signal should be observed")
    .expect("flush signal channel should remain open");
    assert!(
        *flush_signal.borrow_and_update() > 0,
        "flush signal generation should advance after enqueue"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn route_incoming_input_frame_notifies_inject_wake_signal() {
    let root = std::env::temp_dir().join(format!(
        "boundless-inject-wake-test-{}",
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
    assert!(
        state
            .claim_input_owner(&peer_id, false)
            .await
            .expect("claim owner")
    );

    let signal = state.input_inject_wake_signal();
    let notified = signal.notified();
    tokio::pin!(notified);

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
        .expect("route");

    tokio::time::timeout(std::time::Duration::from_millis(200), &mut notified)
        .await
        .expect("inject wake should fire promptly");
    assert!(
        signal.take_pending(),
        "inject wake should remain pending for the runtime loop"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn set_input_capture_target_notifies_capture_wake_signal() {
    let root = std::env::temp_dir().join(format!(
        "boundless-capture-wake-test-{}",
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

    let signal = state.input_capture_wake_signal();
    let notified = signal.notified();
    tokio::pin!(notified);

    state
        .set_input_capture_target(Some(&peer_id))
        .await
        .expect("set target");

    tokio::time::timeout(std::time::Duration::from_millis(200), &mut notified)
        .await
        .expect("capture wake should fire promptly");
    assert!(
        signal.take_pending(),
        "capture wake should remain pending for the runtime loop"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn drain_outgoing_prioritizes_input_frames_over_bulk_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-outgoing-priority-test-{}",
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
        .queue_clipboard_text(&peer_id, "bulk".to_string())
        .await
        .expect("queue bulk");
    state
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 1, dy: 2 }])
        .await
        .expect("queue input");

    let drained = state.drain_outgoing(&peer_id).await;
    assert_eq!(drained.len(), 2);
    assert!(
        matches!(drained.first(), Some(OutboundPayload::InputFrame { .. })),
        "input frame should drain before bulk payloads"
    );
    assert!(
        matches!(drained.get(1), Some(OutboundPayload::ClipboardText { .. })),
        "bulk payload should follow drained input frame"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_input_events_coalesces_adjacent_move_frames() {
    let root = std::env::temp_dir().join(format!(
        "boundless-outgoing-input-coalesce-test-{}",
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
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 2, dy: 3 }])
        .await
        .expect("queue move one");
    state
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: -1, dy: 4 }])
        .await
        .expect("queue move two");

    let queued = state.drain_outgoing(&peer_id).await;
    assert_eq!(
        queued.len(),
        1,
        "adjacent outgoing move frames should collapse"
    );
    assert!(matches!(
        queued.first(),
        Some(OutboundPayload::InputFrame { sequence: 2, events, .. })
            if matches!(events.as_slice(), [InputEvent::MouseMove { dx, dy }] if *dx == 1 && *dy == 7)
    ));

    let events = state.transport_events().await;
    assert!(
        events.iter().any(|event| {
            event.kind == "input_queue_coalesced"
                && event.detail.contains("queue=outgoing_input")
                && event.peer_id == peer_id
        }),
        "outgoing coalescing should be observable in diagnostics"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn requeue_outgoing_front_coalesces_move_frames_across_boundary() {
    let root = std::env::temp_dir().join(format!(
        "boundless-outgoing-requeue-coalesce-test-{}",
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
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 3, dy: 1 }])
        .await
        .expect("queue first");
    let drained = state.drain_outgoing_input(&peer_id, 1).await;
    state
        .queue_input_events(&peer_id, vec![InputEvent::MouseMove { dx: 4, dy: -2 }])
        .await
        .expect("queue second");

    state.requeue_outgoing_front(&peer_id, drained).await;

    let queued = state.drain_outgoing(&peer_id).await;
    assert_eq!(
        queued.len(),
        1,
        "requeue boundary should still collapse adjacent moves"
    );
    assert!(matches!(
        queued.first(),
        Some(OutboundPayload::InputFrame { sequence: 2, events, .. })
            if matches!(events.as_slice(), [InputEvent::MouseMove { dx, dy }] if *dx == 7 && *dy == -1)
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn drain_outgoing_bulk_respects_max_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-outgoing-bulk-limit-test-{}",
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
        .queue_clipboard_text(&peer_id, "one".to_string())
        .await
        .expect("queue one");
    state
        .queue_clipboard_text(&peer_id, "two".to_string())
        .await
        .expect("queue two");
    state
        .queue_clipboard_text(&peer_id, "three".to_string())
        .await
        .expect("queue three");

    let first_batch = state.drain_outgoing_bulk(&peer_id, 2).await;
    assert_eq!(first_batch.len(), 2);
    assert!(matches!(
        first_batch.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "one"
    ));
    assert!(matches!(
        first_batch.get(1),
        Some(OutboundPayload::ClipboardText { text }) if text == "two"
    ));

    let second_batch = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
    assert_eq!(second_batch.len(), 1);
    assert!(matches!(
        second_batch.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "three"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_file_from_path_enqueues_chunked_bulk_transfer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-outgoing-chunk-test-{}",
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

    let file_path = root.join("payload.bin");
    let payload = vec![7u8; FILE_TRANSFER_CHUNK_BYTES * 2 + 17];
    tokio::fs::write(&file_path, &payload)
        .await
        .expect("write payload");

    state
        .queue_file_from_path(&peer_id, &file_path)
        .await
        .expect("queue file");

    let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
    assert_eq!(queued.len(), 2, "start + cursor expected");
    assert_eq!(state.outbound_file_transfer_count().await, 1);

    let transfer_id = match queued.first() {
        Some(OutboundPayload::FileStart {
            transfer_id,
            file_name,
            total_bytes,
        }) => {
            assert_eq!(file_name, "payload.bin");
            assert_eq!(*total_bytes, payload.len() as u64);
            transfer_id.clone()
        }
        other => panic!("expected file start payload, got {other:?}"),
    };

    match queued.get(1) {
        Some(OutboundPayload::FileTransferCursor {
            transfer_id: cursor_transfer_id,
        }) => {
            assert_eq!(cursor_transfer_id, &transfer_id);
        }
        other => panic!("expected file transfer cursor payload, got {other:?}"),
    }

    state.requeue_outgoing_front(&peer_id, queued).await;
    assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_large_file_uses_bounded_cursor_state() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-outgoing-large-cursor-test-{}",
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

    let file_path = root.join("large.bin");
    tokio::fs::create_dir_all(&root).await.expect("create root");
    let file = tokio::fs::File::create(&file_path)
        .await
        .expect("create sparse payload");
    file.set_len(100 * 1024 * 1024)
        .await
        .expect("size sparse payload");
    drop(file);

    state
        .queue_file_from_path(&peer_id, &file_path)
        .await
        .expect("queue large file");

    assert_eq!(
        state.outgoing_bulk_queue_len(&peer_id).await,
        2,
        "large file should queue only start + cursor"
    );
    assert_eq!(state.outbound_file_transfer_count().await, 1);

    let queued = state.drain_outgoing_bulk(&peer_id, usize::MAX).await;
    assert_eq!(queued.len(), 2);
    assert!(matches!(
        queued.first(),
        Some(OutboundPayload::FileStart { .. })
    ));
    assert!(matches!(
        queued.get(1),
        Some(OutboundPayload::FileTransferCursor { .. })
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn cancel_outbound_file_transfer_before_start_emits_one_terminal_event() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-outgoing-cancel-before-start-test-{}",
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
    let file_path = root.join("cancel.bin");
    tokio::fs::write(&file_path, vec![3u8; FILE_TRANSFER_CHUNK_BYTES + 1])
        .await
        .expect("write payload");

    state
        .queue_file_from_path(&peer_id, &file_path)
        .await
        .expect("queue file");
    let queued = state.drain_outgoing_bulk(&peer_id, 1).await;
    let transfer_id = match queued.first() {
        Some(OutboundPayload::FileStart { transfer_id, .. }) => transfer_id.clone(),
        other => panic!("expected file start payload, got {other:?}"),
    };
    state.requeue_outgoing_front(&peer_id, queued).await;

    assert!(
        state
            .cancel_outbound_file_transfer(&peer_id, &transfer_id, "user_cancelled")
            .await
    );
    assert!(
        !state
            .cancel_outbound_file_transfer(&peer_id, &transfer_id, "user_cancelled")
            .await
    );
    assert_eq!(state.outgoing_bulk_queue_len(&peer_id).await, 0);
    assert_eq!(state.outbound_file_transfer_count().await, 0);
    let cancelled = state
        .transport_events()
        .await
        .into_iter()
        .filter(|event| event.kind == "file_transfer_cancelled")
        .count();
    assert_eq!(cancelled, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn queue_file_from_path_rejects_non_regular_paths() {
    let root = std::env::temp_dir().join(format!(
        "boundless-file-outgoing-invalid-path-test-{}",
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

    let err = state
        .queue_file_from_path(&peer_id, &root)
        .await
        .expect_err("directory path must fail");
    assert!(
        err.to_string().contains("regular file"),
        "error should indicate non-regular input"
    );
    assert!(
        state
            .drain_outgoing_bulk(&peer_id, usize::MAX)
            .await
            .is_empty(),
        "invalid source path must not enqueue bulk payloads"
    );

    let _ = std::fs::remove_dir_all(&root);
}
