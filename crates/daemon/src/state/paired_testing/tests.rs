use super::*;

fn leased(now: Instant) -> Inner {
    Inner {
        lease: Some(Lease {
            peer_id: "peer-a".into(),
            expires: now + Duration::from_secs(10),
            remaining_bytes: 128,
            remaining_requests: 2,
        }),
        ..Default::default()
    }
}

#[test]
fn permission_is_peer_scoped_expires_and_consumes_hard_budgets() {
    let now = Instant::now();
    let mut inner = leased(now);
    assert_eq!(
        inner.authorize_probe("peer-b", 64, now),
        Err("consent_required")
    );
    assert_eq!(
        inner.authorize_probe("peer-a", MAX_PROBE_BYTES + 1, now),
        Err("payload_limit")
    );
    assert_eq!(inner.authorize_probe("peer-a", 64, now), Ok(()));
    assert_eq!(
        inner.authorize_probe("peer-a", 65, now),
        Err("lease_budget_exhausted")
    );
    assert_eq!(inner.authorize_probe("peer-a", 64, now), Ok(()));
    assert_eq!(
        inner.authorize_probe("peer-a", 0, now),
        Err("lease_budget_exhausted")
    );
    assert!(!inner.consent(now).enabled);
    let mut inner = leased(now);
    assert_eq!(
        inner.authorize_probe("peer-a", 64, now + Duration::from_secs(10)),
        Err("consent_required")
    );
    assert!(!inner.consent(now + Duration::from_secs(11)).enabled);
    assert!(inner.lease.is_none());
}

#[test]
fn cancellation_discards_unsent_data_and_closes_reply_slot() {
    let state = PairedTestingState::default();
    let (send, mut receive) = oneshot::channel();
    state.inner.lock().unwrap().pending = Some(Pending {
        peer_id: "peer-a".into(),
        request_id: uuid::Uuid::new_v4().to_string(),
        payload: Some(vec![123; MAX_PROBE_BYTES]),
        deadline: Instant::now() + Duration::from_secs(1),
        sent_on: None,
        response: send,
    });
    drop(PendingGuard(&state));
    assert!(state.inner.lock().unwrap().pending.is_none());
    assert!(matches!(
        receive.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
}

#[tokio::test]
async fn replies_cannot_cross_peer_request_or_session_boundaries() {
    let root = std::env::temp_dir().join(format!(
        "boundless-diagnostic-correlation-{}",
        uuid::Uuid::new_v4()
    ));
    let mut config = RuntimeConfig::default();
    config.file_transfer.receive_dir = root.join("inbox").to_string_lossy().into_owned();
    save_config_at(&root.join("config.json"), &config).unwrap();
    let state =
        AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
            .unwrap();
    state
        .claim_transport_session("peer-a", 42, true, Arc::new(RuntimeWakeSignal::default()))
        .await;
    let request_id = uuid::Uuid::new_v4().to_string();
    let (send, mut receive) = oneshot::channel();
    state.paired_testing.inner.lock().unwrap().pending = Some(Pending {
        peer_id: "peer-a".into(),
        request_id: request_id.clone(),
        payload: Some(vec![1, 2, 3]),
        deadline: Instant::now() + Duration::from_secs(10),
        sent_on: None,
        response: send,
    });
    assert!(
        state
            .take_diagnostic_probe("peer-b", 42, EvidenceCategory::Synthetic)
            .is_none()
    );
    assert!(
        state
            .take_diagnostic_probe("peer-a", 42, EvidenceCategory::Synthetic)
            .is_some()
    );
    let reply = |request_id: String| WireMessage::DiagnosticReply {
        request_id,
        status: "ok".into(),
        payload: vec![1, 2, 3],
        metadata_json: String::new(),
        session_id: 99,
    };
    state.complete_diagnostic_probe("peer-b", 42, reply(request_id.clone()));
    state.complete_diagnostic_probe("peer-a", 43, reply(request_id.clone()));
    state.complete_diagnostic_probe("peer-a", 42, reply(uuid::Uuid::new_v4().to_string()));
    assert!(matches!(
        receive.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    state.complete_diagnostic_probe("peer-a", 42, reply(request_id));
    let received = receive.await.unwrap();
    assert_eq!(received.payload, [1, 2, 3]);
    assert_eq!(received.local_session_id, 42);
    assert_eq!(received.remote_session_id, 99);
    assert!(state.paired_testing.inner.lock().unwrap().pending.is_none());
    drop(state);
    std::fs::remove_dir_all(root).unwrap();
}
