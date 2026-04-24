use super::*;

#[tokio::test]
async fn import_trust_bundle_rejects_invalid_address_without_persisting_trust() {
    let root = std::env::temp_dir().join(format!(
        "boundless-import-bundle-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state = AppState::load_or_create_with_paths(config_path, security_root.clone())
        .expect("load state");

    let remote_paths = core_security::SecurityPaths::for_root(root.join("remote-security"));
    let remote_identity = core_security::ensure_device_identity(
        &remote_paths,
        "remote-machine",
        "remote",
        Some("127.0.0.1"),
    )
    .expect("remote identity");

    let err = state
        .import_trust_bundle(
            core_security::TrustBundle {
                machine_id: "remote-machine".to_string(),
                display_name: "remote".to_string(),
                network_address: "   ".to_string(),
                ca_cert_pem: remote_identity.ca_cert_pem,
            },
            None,
        )
        .await
        .expect_err("invalid address must fail");
    assert!(err.to_string().contains("peer address must not be empty"));

    let trusted = state.trusted_records().await.expect("read trust");
    assert!(
        trusted
            .iter()
            .all(|record| record.machine_id != "remote-machine"),
        "invalid bundle import must not persist trust records"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn remove_peer_revokes_trust_and_reimport_resets_reconnect_generation() {
    let root = std::env::temp_dir().join(format!(
        "boundless-trust-lifecycle-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state = AppState::load_or_create_with_paths(config_path, security_root.clone())
        .expect("load state");

    let remote_paths = core_security::SecurityPaths::for_root(root.join("remote-security"));
    let remote_identity = core_security::ensure_device_identity(
        &remote_paths,
        "remote-machine",
        "remote",
        Some("127.0.0.1"),
    )
    .expect("remote identity");

    let initial_bundle = core_security::TrustBundle {
        machine_id: "remote-machine".to_string(),
        display_name: "remote".to_string(),
        network_address: "10.10.0.5:15100".to_string(),
        ca_cert_pem: remote_identity.ca_cert_pem.clone(),
    };

    state
        .import_trust_bundle(initial_bundle, Some("remote-alpha".to_string()))
        .await
        .expect("import trust bundle");
    assert!(
        state.get_peer("remote-machine").await.is_some(),
        "import must create peer entry keyed by machine id"
    );
    assert!(
        state
            .trusted_records()
            .await
            .expect("read trust")
            .iter()
            .any(|record| record.machine_id == "remote-machine"),
        "import must persist trust record"
    );

    state.request_peer_reconnect("remote-machine").await;
    state.request_peer_reconnect("remote-machine").await;
    assert_eq!(
        state.peer_reconnect_generation("remote-machine").await,
        2,
        "generation should increment while peer is present"
    );

    let removed = state
        .remove_peer("remote-machine")
        .await
        .expect("remove peer");
    assert!(removed, "existing peer should be removed");
    assert!(
        state.get_peer("remote-machine").await.is_none(),
        "remove should delete peer config"
    );
    assert!(
        state
            .trusted_records()
            .await
            .expect("read trust")
            .iter()
            .all(|record| record.machine_id != "remote-machine"),
        "remove should revoke trust record"
    );
    assert_eq!(
        state.peer_reconnect_generation("remote-machine").await,
        0,
        "remove should clear reconnect generation state"
    );
    assert!(
        !state
            .remove_peer("remote-machine")
            .await
            .expect("remove missing peer"),
        "second remove should be deterministic no-op"
    );

    let reimport_bundle = core_security::TrustBundle {
        machine_id: "remote-machine".to_string(),
        display_name: "remote".to_string(),
        network_address: "10.10.0.77:15100".to_string(),
        ca_cert_pem: remote_identity.ca_cert_pem,
    };
    state
        .import_trust_bundle(reimport_bundle, Some("remote-beta".to_string()))
        .await
        .expect("re-import trust bundle");

    let reimported_peer = state
        .get_peer("remote-machine")
        .await
        .expect("peer after re-import");
    assert_eq!(reimported_peer.display_name, "remote-beta");
    assert_eq!(reimported_peer.address, "10.10.0.77:15100");
    assert_eq!(
        state.request_peer_reconnect("remote-machine").await,
        1,
        "reconnect generation should restart after re-import"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn diagnostics_dump_reports_nonce_challenge_rejections() {
    let root = std::env::temp_dir().join(format!(
        "boundless-pairing-diagnostics-dump-test-{}",
        uuid::Uuid::new_v4()
    ));
    let config_path = root.join("config.json");
    let security_root = root.join("security");
    let state =
        AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

    let requester_paths = core_security::SecurityPaths::for_root(root.join("requester-security"));
    let requester_identity = core_security::ensure_device_identity(
        &requester_paths,
        "requester-machine",
        "requester",
        Some("10.10.0.5"),
    )
    .expect("requester identity");
    let requester_bundle = core_security::TrustBundle {
        machine_id: "requester-machine".to_string(),
        display_name: "requester".to_string(),
        network_address: "10.10.0.5:15100".to_string(),
        ca_cert_pem: requester_identity.ca_cert_pem,
    };

    let challenge = state
        .queue_nearby_pairing_code_challenge(requester_bundle, None, 120)
        .await
        .expect("queue challenge");
    let request_id = challenge.request_id.clone();
    let verification_code = challenge
        .verification_code
        .clone()
        .expect("verification code");

    for _ in 0..5 {
        let _ = state
            .submit_nearby_pairing_code(&request_id, &verification_code, "wrong-nonce", None)
            .await;
    }
    assert!(
        matches!(
            state.nearby_pairing_status(&request_id).await,
            NearbyPairingStatus::Rejected { .. }
        ),
        "nonce failures should reject the request after max attempts"
    );

    let output_dir = root.join("diagnostics");
    let dump_path = state
        .diagnostics_dump(Some(output_dir.to_string_lossy().to_string()))
        .await
        .expect("diagnostics dump path");
    let dump_content = std::fs::read_to_string(&dump_path).expect("read diagnostics dump");
    assert!(
        dump_content.contains("Pairing Diagnostics"),
        "diagnostics dump should include pairing diagnostics section"
    );
    assert!(
        dump_content.contains("pairing_decisions_rejected=1"),
        "diagnostics should include one rejected decision"
    );
    assert!(
        dump_content.contains("pairing_rejections_nonce_attempts=1"),
        "diagnostics should classify nonce challenge rejections"
    );

    let _ = std::fs::remove_dir_all(&root);
}
