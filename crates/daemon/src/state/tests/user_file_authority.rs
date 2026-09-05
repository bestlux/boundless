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
async fn cancelled_source_open_keeps_capacity_until_blocking_worker_finishes() {
    let (state, root) = fixture();
    let source = root.join("source.txt");
    std::fs::write(&source, b"user content").expect("fixture file");
    let lease = state.user_io_lease().await.expect("user authority");
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = slots.clone().try_acquire_owned().expect("initial capacity");
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let caller = tokio::spawn(async move {
        super::super::clipboard_ops::open_outbound_source_with_capacity(&lease, permit, move || {
            let file = std::fs::File::open(source)?;
            let _ = opened_tx.send(());
            finish_rx.recv_timeout(std::time::Duration::from_secs(5))?;
            let metadata = file.metadata()?;
            Ok((file, metadata))
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), opened_rx)
        .await
        .expect("worker opened fixture")
        .expect("worker signal");
    caller.abort();
    assert!(caller.await.expect_err("cancelled RPC").is_cancelled());
    assert_eq!(slots.available_permits(), 0);
    assert!(slots.clone().try_acquire_owned().is_err());
    finish_tx.send(()).expect("finish uncancellable worker");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while slots.available_permits() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("worker completion restores capacity");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconnect_cancellation_keeps_source_capacity_until_blocked_read_finishes() {
    let (state, root) = fixture();
    let peer = peer(&state).await;
    state
        .set_peer_connected(&peer, true)
        .await
        .expect("connected");
    let source = root.join("source.txt");
    std::fs::write(&source, b"fixture").expect("source");
    let transfer = state
        .queue_file_from_path(&peer, &source)
        .await
        .expect("queue");
    let remaining_slots = state
        .outbound_file_handle_slots
        .clone()
        .try_acquire_many_owned((MAX_OUTBOUND_FILE_HANDLES - 1) as u32)
        .expect("reserve other capacity");
    let (reading_tx, reading_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let reader_state = state.clone();
    let reader_peer = peer.clone();
    let session = tokio::spawn(async move {
        reader_state
            .materialize_outbound_file_chunk_with_reader(
                &reader_peer,
                &transfer,
                move |file, offset, length| {
                    use std::io::{Read, Seek};
                    file.seek(std::io::SeekFrom::Start(offset))?;
                    let _ = reading_tx.send(());
                    finish_rx.recv_timeout(std::time::Duration::from_secs(5))?;
                    let mut data = vec![0u8; length];
                    file.read_exact(&mut data)?;
                    assert_eq!(data, b"fixture");
                    Ok(data)
                },
            )
            .await
    });
    state.register_transport_session_for_peer(&peer, session.abort_handle());
    tokio::time::timeout(std::time::Duration::from_secs(2), reading_rx)
        .await
        .expect("worker started")
        .expect("read signal");
    let (_, aborted) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.request_peer_reconnect_and_reset(&peer),
    )
    .await
    .expect("reconnect does not wait for storage")
    .expect("reconnect");
    assert_eq!(aborted, 1);
    assert!(matches!(session.await, Err(error) if error.is_cancelled()));
    assert_eq!(state.outbound_file_transfer_count().await, 0);
    assert_eq!(
        state.outbound_file_handle_slots.available_permits(),
        0,
        "the cancelled worker still owns the source handle and capacity"
    );
    let error = state
        .queue_file_from_path(&peer, &source)
        .await
        .expect_err("stalled worker retains capacity");
    assert!(error.to_string().contains("capacity reached"));
    finish_tx.send(()).expect("finish read");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while state.outbound_file_handle_slots.available_permits() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("completed worker restores capacity");
    assert!(state.queue_file_from_path(&peer, &source).await.is_ok());
    state
        .set_feature("transfer_file".into(), false)
        .await
        .expect("close fixture handles");
    drop(remaining_slots);
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
