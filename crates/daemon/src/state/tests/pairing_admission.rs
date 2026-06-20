use super::*;

fn admission_bundle(machine_id: &str) -> TrustBundle {
    TrustBundle {
        machine_id: machine_id.to_string(),
        display_name: format!("{machine_id}-display"),
        network_address: "127.0.0.1:15100".to_string(),
        ca_cert_pem: "unused in admission tests".to_string(),
    }
}

async fn admission_state(test_name: &str) -> (AppState, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("boundless-{test_name}-{}", uuid::Uuid::new_v4()));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
    (state, root)
}

#[tokio::test]
async fn pairing_admission_reuses_duplicate_manual_request_and_enforces_peer_cap() {
    let (state, root) = admission_state("pairing-admission-peer-cap").await;
    let first_source = "192.0.2.10".parse().expect("ip");
    let second_source = "192.0.2.11".parse().expect("ip");
    let third_source = "192.0.2.12".parse().expect("ip");

    let first = state
        .queue_nearby_pairing_request(admission_bundle("requester-machine"), None, first_source)
        .await
        .expect("queue first request");
    let duplicate = state
        .queue_nearby_pairing_request(admission_bundle("requester-machine"), None, first_source)
        .await
        .expect("duplicate source should reuse pending request");
    assert_eq!(
        duplicate.request_id, first.request_id,
        "duplicate requester/source admission should be idempotent"
    );
    assert_eq!(state.list_pending_nearby_pairing_requests().await.len(), 1);

    state
        .queue_nearby_pairing_request(admission_bundle("requester-machine"), None, second_source)
        .await
        .expect("second source stays within per-peer cap");
    let peer_cap_error = state
        .queue_nearby_pairing_request(admission_bundle("requester-machine"), None, third_source)
        .await
        .expect_err("third source should exceed per-peer cap");
    assert!(
        peer_cap_error.to_string().contains("for this peer"),
        "error should distinguish peer capacity from trust/connectivity failures"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pairing_admission_enforces_source_and_global_caps_with_stable_capacity_errors() {
    let (state, root) = admission_state("pairing-admission-capacity").await;
    let shared_source = "198.51.100.20".parse().expect("ip");

    for index in 0..MAX_PENDING_NEARBY_PAIRING_REQUESTS_PER_SOURCE {
        state
            .queue_nearby_pairing_request(
                admission_bundle(&format!("source-peer-{index}")),
                None,
                shared_source,
            )
            .await
            .expect("source request within cap");
    }
    let source_cap_error = state
        .queue_nearby_pairing_request(
            admission_bundle("source-peer-overflow"),
            None,
            shared_source,
        )
        .await
        .expect_err("source cap should reject overflow");
    assert!(
        source_cap_error.to_string().contains("from this source"),
        "error should distinguish source capacity from trust/connectivity failures"
    );

    let (global_state, global_root) = admission_state("pairing-admission-global-cap").await;
    for index in 0..MAX_PENDING_NEARBY_PAIRING_REQUESTS {
        let source = format!("203.0.113.{index}").parse().expect("ip");
        global_state
            .queue_nearby_pairing_request(
                admission_bundle(&format!("global-peer-{index}")),
                None,
                source,
            )
            .await
            .expect("global request within cap");
    }
    let global_cap_error = global_state
        .queue_nearby_pairing_request(
            admission_bundle("global-peer-overflow"),
            None,
            "2001:db8::1".parse().expect("ip"),
        )
        .await
        .expect_err("global cap should reject overflow");
    assert!(
        global_cap_error
            .to_string()
            .contains("too many pending pairing requests; try again later"),
        "global capacity error should stay stable"
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(global_root);
}

#[tokio::test]
async fn expired_or_rejected_pairing_requests_release_admission_capacity() {
    let (state, root) = admission_state("pairing-admission-release").await;
    let peer = "expiring-peer";
    let first = state
        .queue_nearby_pairing_request(
            admission_bundle(peer),
            None,
            "192.0.2.30".parse().expect("ip"),
        )
        .await
        .expect("queue first request");
    let second = state
        .queue_nearby_pairing_request(
            admission_bundle(peer),
            None,
            "192.0.2.31".parse().expect("ip"),
        )
        .await
        .expect("queue second request");

    {
        let mut pending = state.pairing.pending_requests.write().await;
        let first_record = pending
            .get_mut(&first.request_id)
            .expect("first request record");
        first_record.summary.created_at =
            Utc::now() - chrono::TimeDelta::seconds(NEARBY_PAIRING_PENDING_REQUEST_TTL_SECONDS + 1);
    }

    state
        .queue_nearby_pairing_request(
            admission_bundle(peer),
            None,
            "192.0.2.32".parse().expect("ip"),
        )
        .await
        .expect("expired request should release per-peer capacity");
    assert!(matches!(
        state.nearby_pairing_status(&first.request_id).await,
        NearbyPairingStatus::Rejected { message } if message == "pairing request expired"
    ));

    assert!(
        state
            .reject_nearby_pairing_request(&second.request_id)
            .await
    );
    state
        .queue_nearby_pairing_request(
            admission_bundle(peer),
            None,
            "192.0.2.33".parse().expect("ip"),
        )
        .await
        .expect("rejected request should release capacity");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn code_challenge_admission_reuses_duplicate_source_and_bounds_peer_requests() {
    let (state, root) = admission_state("pairing-admission-code-challenge").await;
    let first_source = "192.0.2.40".parse().expect("ip");
    let second_source = "192.0.2.41".parse().expect("ip");
    let third_source = "192.0.2.42".parse().expect("ip");

    let first = state
        .queue_nearby_pairing_code_challenge(admission_bundle("code-peer"), None, first_source, 120)
        .await
        .expect("queue code challenge");
    let duplicate = state
        .queue_nearby_pairing_code_challenge(admission_bundle("code-peer"), None, first_source, 120)
        .await
        .expect("duplicate source should reuse code challenge");
    assert_eq!(duplicate.request_id, first.request_id);
    assert_eq!(duplicate.verification_nonce, first.verification_nonce);

    state
        .queue_nearby_pairing_code_challenge(
            admission_bundle("code-peer"),
            None,
            second_source,
            120,
        )
        .await
        .expect("second code source stays within per-peer cap");
    let peer_cap_error = state
        .queue_nearby_pairing_code_challenge(admission_bundle("code-peer"), None, third_source, 120)
        .await
        .expect_err("third code source should exceed per-peer cap");
    assert!(peer_cap_error.to_string().contains("for this peer"));

    let _ = std::fs::remove_dir_all(root);
}
