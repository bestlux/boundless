use super::*;

#[tokio::test]
async fn clipboard_sync_dedupes_and_suppresses_remote_echo() {
    let root =
        std::env::temp_dir().join(format!("boundless-clipboard-test-{}", uuid::Uuid::new_v4()));
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
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    let queued = state
        .queue_local_clipboard_text_for_connected_peers("hello".to_string())
        .await
        .expect("queue local hello");
    assert!(queued, "initial clipboard text should be queued");

    let first = state.drain_outgoing(&peer_a).await;
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "hello"
    ));

    let deduped = state
        .queue_local_clipboard_text_for_connected_peers("hello".to_string())
        .await
        .expect("dedupe");
    assert!(!deduped, "unchanged clipboard text should be ignored");

    state
        .enqueue_remote_clipboard_text(&peer_a, "remote".to_string())
        .await
        .expect("enqueue remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    assert!(matches!(
        &remote.payload,
        ClipboardPayload::Text(text) if text == "remote"
    ));
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    let suppressed = state
        .queue_local_clipboard_text_for_connected_peers("remote".to_string())
        .await
        .expect("suppress remote echo");
    assert!(
        !suppressed,
        "clipboard observer should suppress immediate echo after remote apply"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clipboard_sync_persists_disconnected_local_text_for_replay_without_immediate_queueing() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-connect-test-{}",
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

    let queued_disconnected = state
        .queue_local_clipboard_text_for_connected_peers("hello".to_string())
        .await
        .expect("queue while disconnected");
    assert!(
        !queued_disconnected,
        "must not queue without connected peers"
    );
    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "no payloads should be queued while disconnected"
    );

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "reconnect should replay retained text once"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "hello"
    ));
    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "replayed clipboard snapshot should not remain queued after one drain"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clipboard_sync_persists_disconnected_local_image_for_replay() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-image-replay-test-{}",
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

    let image = minimal_bmp_payload(128);
    let queued_disconnected = state
        .queue_local_clipboard_image_for_connected_peers(image.clone())
        .await
        .expect("queue image while disconnected");
    assert!(
        !queued_disconnected,
        "must not queue image payloads without connected peers"
    );
    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "no image payload should be queued while disconnected"
    );

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "reconnect should replay retained image once"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardImage { image_bmp }) if image_bmp == &image
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn peer_connect_schedules_clipboard_replay() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-reconnect-schedule-test-{}",
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

    state
        .queue_local_clipboard_text_for_connected_peers("hello".to_string())
        .await
        .expect("queue while disconnected");
    let mut flush_signal = state.subscribe_outgoing_flush_signal();

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        flush_signal.changed(),
    )
    .await
    .expect("connect should notify replay work")
    .expect("flush signal channel should remain open");
    assert!(
        *flush_signal.borrow_and_update() > 0,
        "replay scheduling should advance the flush generation"
    );

    let outgoing = state.drain_outgoing_bulk(&peer_a, usize::MAX).await;
    assert_eq!(
        outgoing.len(),
        1,
        "connect should schedule one replay payload"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "hello"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn mixed_topology_reconnect_replays_latest_clipboard_snapshot_to_late_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-mixed-reconnect-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    let queued = state
        .queue_local_clipboard_text_for_connected_peers("shared".to_string())
        .await
        .expect("queue local clipboard");
    assert!(
        queued,
        "connected peers should receive the live clipboard update"
    );

    let outgoing_a = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing_a.len(),
        1,
        "connected peer should get direct payload"
    );
    assert!(matches!(
        outgoing_a.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "shared"
    ));
    assert!(
        state.drain_outgoing(&peer_b).await.is_empty(),
        "disconnected peer must not receive a queued payload before reconnect"
    );

    state
        .set_peer_connected(&peer_b, true)
        .await
        .expect("connect peer-b");

    let outgoing_b = state.drain_outgoing(&peer_b).await;
    assert_eq!(
        outgoing_b.len(),
        1,
        "late peer should receive the retained latest snapshot on reconnect"
    );
    assert!(matches!(
        outgoing_b.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "shared"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_origin_reconnect_replays_latest_clipboard_snapshot_to_late_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-origin-reconnect-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    state
        .enqueue_remote_clipboard_text(&peer_a, "remote-shared".to_string())
        .await
        .expect("enqueue remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "applying a remote payload should not immediately echo it back to the source peer"
    );

    state
        .set_peer_connected(&peer_b, true)
        .await
        .expect("connect peer-b");

    let outgoing_b = state.drain_outgoing(&peer_b).await;
    assert_eq!(
        outgoing_b.len(),
        1,
        "late peer should receive the applied remote snapshot on reconnect"
    );
    assert!(matches!(
        outgoing_b.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "remote-shared"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_origin_snapshot_is_not_replayed_back_to_source_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-origin-no-echo-replay-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");
    state
        .enqueue_remote_clipboard_text(&peer_a, "remote-shared".to_string())
        .await
        .expect("enqueue remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    state
        .set_peer_connected(&peer_a, false)
        .await
        .expect("disconnect peer-a");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("reconnect peer-a");

    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "the source peer must not receive its own remote-applied snapshot back on reconnect"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn current_snapshot_resend_tracks_all_source_peers() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-current-snapshot-source-peer-set-test-{}",
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

    state
        .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
        .await
        .expect("enqueue first remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    state
        .enqueue_remote_clipboard_text(&peer_b, "dup".to_string())
        .await
        .expect("enqueue resend of current snapshot from second peer");
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "resend of the current authoritative snapshot should still be suppressed"
    );

    state
        .set_peer_connected(&peer_b, true)
        .await
        .expect("connect peer-b");

    assert!(
        state.drain_outgoing(&peer_b).await.is_empty(),
        "all peers that originated the current snapshot should be suppressed from replay"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn reconnect_does_not_schedule_duplicate_replay_when_live_payload_is_already_queued() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-no-duplicate-reconnect-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");
    let queued = state
        .queue_local_clipboard_text_for_connected_peers("hello".to_string())
        .await
        .expect("queue local clipboard");
    assert!(
        queued,
        "connected peer should get the live clipboard payload"
    );

    state
        .set_peer_connected(&peer_a, false)
        .await
        .expect("disconnect peer-a");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("reconnect peer-a");

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "reconnect should not schedule a duplicate replay when the same payload is already queued"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "hello"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn newer_local_snapshot_prunes_stale_queued_clipboard_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-prune-stale-local-queue-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");
    state
        .queue_local_clipboard_text_for_connected_peers("old".to_string())
        .await
        .expect("queue old local clipboard");
    state
        .set_peer_connected(&peer_a, false)
        .await
        .expect("disconnect peer-a");

    state
        .queue_local_clipboard_text_for_connected_peers("new".to_string())
        .await
        .expect("queue new local clipboard while disconnected");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("reconnect peer-a");

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "new authoritative local snapshot should prune stale queued clipboard payloads"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "new"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stale_drained_replay_is_dropped_after_newer_local_snapshot_supersedes_it() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-stale-drained-replay-drop-test-{}",
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

    state
        .queue_local_clipboard_text_for_connected_peers("stale".to_string())
        .await
        .expect("queue stale while disconnected");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a and schedule stale replay");

    let drained = state.drain_outgoing_bulk(&peer_a, usize::MAX).await;
    assert_eq!(
        drained.len(),
        1,
        "expected one drained stale replay payload"
    );
    assert!(matches!(
        drained.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "stale"
    ));

    let queued = state
        .queue_local_clipboard_text_for_connected_peers("fresh".to_string())
        .await
        .expect("queue fresh local clipboard");
    assert!(
        queued,
        "fresh local clipboard should queue for the connected peer"
    );

    state.requeue_outgoing_front(&peer_a, drained).await;

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "stale drained replay must be dropped instead of reentering the bulk queue"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "fresh"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn newer_remote_snapshot_prunes_stale_queued_clipboard_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-prune-stale-remote-queue-test-{}",
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

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");
    state
        .queue_local_clipboard_text_for_connected_peers("old".to_string())
        .await
        .expect("queue old local clipboard");
    state
        .set_peer_connected(&peer_a, false)
        .await
        .expect("disconnect peer-a");

    state
        .enqueue_remote_clipboard_text(&peer_b, "new".to_string())
        .await
        .expect("enqueue remote clipboard");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("reconnect peer-a");

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "new authoritative remote snapshot should prune stale queued local clipboard payloads"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "new"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn live_local_update_supersedes_stale_pending_replay_for_connected_peer() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-live-local-supersedes-replay-test-{}",
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

    state
        .queue_local_clipboard_text_for_connected_peers("stale".to_string())
        .await
        .expect("queue stale while disconnected");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a and schedule stale replay");

    let queued = state
        .queue_local_clipboard_text_for_connected_peers("fresh".to_string())
        .await
        .expect("queue fresh local clipboard");
    assert!(
        queued,
        "fresh local clipboard should use the live connected-peer path"
    );

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "fresh live payload should replace the stale pending replay for this peer"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "fresh"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_apply_cancels_pending_replay() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-cancel-test-{}",
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

    state
        .queue_local_clipboard_text_for_connected_peers("local".to_string())
        .await
        .expect("queue while disconnected");
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    state
        .enqueue_remote_clipboard_text(&peer_a, "remote".to_string())
        .await
        .expect("enqueue remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    assert!(
        state.drain_outgoing(&peer_a).await.is_empty(),
        "remote apply should cancel stale replay before it is sent"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_queue_dedupes_consecutive_duplicate_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-dedupe-test-{}",
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

    state
        .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
        .await
        .expect("enqueue first duplicate");
    state
        .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
        .await
        .expect("enqueue second duplicate");

    let first = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("first item");
    assert!(matches!(
        first.payload,
        ClipboardPayload::Text(ref text) if text == "dup"
    ));
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "consecutive duplicate remote payloads should collapse to one queued item"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_queue_suppresses_current_snapshot_resend() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-current-resend-test-{}",
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

    state
        .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
        .await
        .expect("enqueue first remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    state
        .enqueue_remote_clipboard_text(&peer_a, "dup".to_string())
        .await
        .expect("enqueue resend of current snapshot");
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "resend of the current authoritative snapshot should not be requeued"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_image_queue_dedupes_consecutive_duplicate_payloads() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-image-dedupe-test-{}",
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

    let image = minimal_bmp_payload(55);
    state
        .enqueue_remote_clipboard_image(&peer_a, image.clone())
        .await
        .expect("enqueue first duplicate image");
    state
        .enqueue_remote_clipboard_image(&peer_a, image.clone())
        .await
        .expect("enqueue second duplicate image");

    let first = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("first image item");
    assert!(matches!(
        first.payload,
        ClipboardPayload::Image(ref image_bmp) if image_bmp == &image
    ));
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "consecutive duplicate remote image payloads should collapse to one queued item"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_queue_evicts_oldest_item_at_capacity() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-eviction-test-{}",
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

    for index in 0..=MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
        state
            .enqueue_remote_clipboard_text(&peer_a, format!("payload-{index}"))
            .await
            .expect("enqueue remote payload");
    }

    let mut drained = Vec::new();
    while let Some(item) = state.dequeue_remote_clipboard_payload().await {
        drained.push(item);
    }

    assert_eq!(
        drained.len(),
        MAX_PENDING_REMOTE_CLIPBOARD_ITEMS,
        "remote clipboard queue should remain bounded at capacity"
    );
    assert!(matches!(
        drained.first(),
        Some(PendingRemoteClipboardPayload {
            payload: ClipboardPayload::Text(text),
            ..
        }) if text == "payload-1"
    ));
    assert!(matches!(
        drained.last(),
        Some(PendingRemoteClipboardPayload {
            payload: ClipboardPayload::Text(text),
            ..
        }) if text == &format!("payload-{MAX_PENDING_REMOTE_CLIPBOARD_ITEMS}")
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remote_clipboard_requeue_front_evicts_newest_item_at_capacity() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-remote-requeue-eviction-test-{}",
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

    for index in 0..MAX_PENDING_REMOTE_CLIPBOARD_ITEMS {
        state
            .enqueue_remote_clipboard_text(&peer_a, format!("payload-{index}"))
            .await
            .expect("enqueue remote payload");
    }

    state
        .requeue_remote_clipboard_payload_front(PendingRemoteClipboardPayload {
            peer_id: peer_a.clone(),
            payload: ClipboardPayload::Text("requeued".to_string()),
            hash: payload_hash_hex(&ClipboardPayload::Text("requeued".to_string())),
            retry_count: 1,
        })
        .await;

    let mut drained = Vec::new();
    while let Some(item) = state.dequeue_remote_clipboard_payload().await {
        drained.push(item);
    }

    assert_eq!(
        drained.len(),
        MAX_PENDING_REMOTE_CLIPBOARD_ITEMS,
        "front requeue should keep the remote clipboard queue bounded"
    );
    assert!(matches!(
        drained.first(),
        Some(PendingRemoteClipboardPayload {
            payload: ClipboardPayload::Text(text),
            ..
        }) if text == "requeued"
    ));
    assert!(matches!(
        drained.last(),
        Some(PendingRemoteClipboardPayload {
            payload: ClipboardPayload::Text(text),
            ..
        }) if text == &format!("payload-{}", MAX_PENDING_REMOTE_CLIPBOARD_ITEMS - 2)
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clipboard_sync_clears_stale_suppress_token_after_mismatch() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-stale-suppress-test-{}",
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
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    state
        .enqueue_remote_clipboard_text(&peer_a, "remote".to_string())
        .await
        .expect("enqueue remote");
    let remote = state
        .dequeue_remote_clipboard_payload()
        .await
        .expect("remote item");
    state
        .mark_remote_clipboard_applied(&remote.peer_id, &remote.payload, &remote.hash)
        .await;

    let different = state
        .queue_local_clipboard_text_for_connected_peers("different".to_string())
        .await
        .expect("queue different");
    assert!(different, "different local clipboard value should queue");

    let remote_again = state
        .queue_local_clipboard_text_for_connected_peers("remote".to_string())
        .await
        .expect("queue remote again");
    assert!(
        remote_again,
        "stale suppression token must not suppress later legitimate reuse"
    );

    let outgoing = state.drain_outgoing(&peer_a).await;
    assert_eq!(
        outgoing.len(),
        1,
        "later authoritative clipboard value should replace the earlier queued clipboard payload"
    );
    assert!(matches!(
        outgoing.first(),
        Some(OutboundPayload::ClipboardText { text }) if text == "remote"
    ));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn clipboard_image_sync_dedupes_current_snapshot_and_blocks_echo() {
    let root = std::env::temp_dir().join(format!(
        "boundless-clipboard-image-test-{}",
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
    state
        .set_peer_connected(&peer_a, true)
        .await
        .expect("connect peer-a");

    let image = minimal_bmp_payload(255);
    let queued = state
        .queue_local_clipboard_image_for_connected_peers(image.clone())
        .await
        .expect("queue local image");
    assert!(queued, "initial clipboard image should be queued");

    let first = state.drain_outgoing(&peer_a).await;
    assert_eq!(first.len(), 1);
    assert!(matches!(
        first.first(),
        Some(OutboundPayload::ClipboardImage { image_bmp }) if image_bmp == &image
    ));

    let deduped = state
        .queue_local_clipboard_image_for_connected_peers(image.clone())
        .await
        .expect("dedupe image");
    assert!(!deduped, "unchanged clipboard image should be ignored");

    state
        .enqueue_remote_clipboard_image(&peer_a, image.clone())
        .await
        .expect("enqueue remote image");
    assert!(
        state.dequeue_remote_clipboard_payload().await.is_none(),
        "resend of the current authoritative image snapshot should not be requeued"
    );

    let suppressed = state
        .queue_local_clipboard_image_for_connected_peers(image.clone())
        .await
        .expect("ignore image echo");
    assert!(
        !suppressed,
        "clipboard observer should still ignore the unchanged image after a remote resend"
    );

    let changed = state
        .queue_local_clipboard_image_for_connected_peers(minimal_bmp_payload(64))
        .await
        .expect("queue changed image");
    assert!(changed, "different clipboard image should queue");

    let _ = std::fs::remove_dir_all(&root);
}
