use super::*;

fn fixture() -> (AppState, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("boundless-file-authority-{}", uuid::Uuid::new_v4()));
    let config_path = root.join("config.json");
    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = root.join("received").display().to_string();
    save_config_at(&config_path, &config).expect("fixture config");
    let state = AppState::load_or_create_with_paths(config_path, root.join("security"))
        .expect("fixture state");
    (state, root)
}

async fn peer(state: &AppState) -> String {
    let (code, _) = state.create_pairing_code(120).await;
    state
        .join_peer(
            code,
            "127.0.0.1:15100".to_string(),
            Some("fixture peer".to_string()),
        )
        .await
        .expect("fixture peer")
}

#[tokio::test]
async fn disabled_transfer_never_opens_source_or_creates_receive_directory() {
    let (state, root) = fixture();
    state
        .set_feature("transfer_file".to_string(), false)
        .await
        .expect("disable");
    let error = state
        .queue_file_from_path("missing peer", &root.join("missing-source"))
        .await
        .expect_err("disabled before source lookup");
    assert!(error.to_string().contains("file transfer is disabled"));
    assert!(
        state
            .reserve_incoming_file("peer", "file.txt", 3)
            .await
            .is_err()
    );
    assert!(!root.join("received").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disabling_file_transfer_cancels_active_source_and_prevents_retry() {
    let (state, root) = fixture();
    let peer = peer(&state).await;
    let source = root.join("source.txt");
    std::fs::write(&source, b"fixture").expect("source");
    let transfer = state
        .queue_file_from_path(&peer, &source)
        .await
        .expect("queue");
    assert_eq!(
        state
            .materialize_outbound_file_chunk(&peer, &transfer)
            .await
            .expect("first chunk")
            .data,
        b"fixture"
    );
    state
        .set_feature("transfer_file".to_string(), false)
        .await
        .expect("disable");
    assert_eq!(state.outbound_file_transfer_count().await, 0);
    assert!(
        state
            .drain_outgoing_bulk(&peer, usize::MAX)
            .await
            .is_empty()
    );
    assert!(
        state
            .materialize_outbound_file_chunk(&peer, &transfer)
            .await
            .is_err()
    );
    assert!(
        state
            .retry_file_transfer_from_beginning(&transfer)
            .await
            .is_err()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn queued_source_uses_authorized_handle_after_path_replacement() {
    let (state, root) = fixture();
    let peer = peer(&state).await;
    let source = root.join("source.txt");
    std::fs::write(&source, b"original").expect("source");
    let transfer = state
        .queue_file_from_path(&peer, &source)
        .await
        .expect("queue");
    std::fs::rename(&source, root.join("original.txt")).expect("replace source name");
    std::fs::write(&source, b"replaced").expect("replacement");
    assert_eq!(
        state
            .materialize_outbound_file_chunk(&peer, &transfer)
            .await
            .expect("chunk")
            .data,
        b"original"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn retained_source_handles_are_bounded_and_cancellation_restores_capacity() {
    let (state, root) = fixture();
    let peer = peer(&state).await;
    let source = root.join("source.txt");
    std::fs::write(&source, b"fixture").expect("source");
    let mut transfers = Vec::new();
    for _ in 0..MAX_OUTBOUND_FILE_HANDLES {
        transfers.push(
            state
                .queue_file_from_path(&peer, &source)
                .await
                .expect("available slot"),
        );
    }
    let error = state
        .queue_file_from_path(&peer, &root.join("unopened-missing-source"))
        .await
        .expect_err("capacity before source open");
    assert!(error.to_string().contains("capacity reached"));
    assert!(
        state
            .cancel_outbound_file_transfer(&peer, &transfers[0], "fixture_cancel")
            .await
    );
    assert!(state.queue_file_from_path(&peer, &source).await.is_ok());
    state
        .set_feature("transfer_file".to_string(), false)
        .await
        .expect("close handles");
    assert_eq!(
        state.outbound_file_handle_slots.available_permits(),
        MAX_OUTBOUND_FILE_HANDLES
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn missing_service_user_authority_blocks_user_paths_but_keeps_state_queryable() {
    let (state, root) = fixture();
    let peer = peer(&state).await;
    state.input_broker.mark_service_session_input();
    let source = root.join("source.txt");
    std::fs::write(&source, b"fixture").expect("source");
    assert!(state.queue_file_from_path(&peer, &source).await.is_err());
    assert!(
        state
            .reserve_incoming_file(&peer, "received.txt", 7)
            .await
            .is_err()
    );
    assert!(
        state
            .diagnostics_dump(Some(root.join("export").display().to_string()))
            .await
            .is_err()
    );
    let mut config = state.file_transfer_config().await;
    config.receive_dir = root.join("new-receive").display().to_string();
    assert!(state.update_file_transfer_config(config).await.is_err());
    assert!(!root.join("received").exists());
    assert!(!root.join("new-receive").exists());
    assert!(!root.join("export").exists());
    assert!(
        !state.snapshot().await.machine_id.is_empty(),
        "daemon health/config queries remain available"
    );
    let _ = std::fs::remove_dir_all(root);
}
