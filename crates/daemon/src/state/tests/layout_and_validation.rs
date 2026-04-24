use super::*;

#[test]
fn resolve_capture_handoff_target_uses_layout_neighbors() {
    let config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: ",up,;left,self,right;,down,".to_string(),
        peers: vec![
            PeerConfig {
                peer_id: "peer-left".to_string(),
                display_name: "left".to_string(),
                address: "127.0.0.1:15100".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-up".to_string(),
                display_name: "up".to_string(),
                address: "127.0.0.1:15102".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-down".to_string(),
                display_name: "down".to_string(),
                address: "127.0.0.1:15103".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
        ],
        ..Default::default()
    };

    assert_eq!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Left),
        Some(CaptureHandoffTarget::Peer("peer-left".to_string()))
    );
    assert_eq!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Right),
        Some(CaptureHandoffTarget::Peer("peer-right".to_string()))
    );
    assert_eq!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Up),
        Some(CaptureHandoffTarget::Peer("peer-up".to_string()))
    );
    assert_eq!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Down),
        Some(CaptureHandoffTarget::Peer("peer-down".to_string()))
    );
}

#[test]
fn resolve_capture_handoff_target_ignores_disconnected_and_requires_single_local_cell() {
    let mut config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: "local,right".to_string(),
        peers: vec![PeerConfig {
            peer_id: "peer-right".to_string(),
            display_name: "right".to_string(),
            address: "127.0.0.1:15101".to_string(),
            connected: false,
            last_seen: Utc::now(),
        }],
        ..Default::default()
    };
    assert!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Right).is_none(),
        "disconnected neighbors should not be selected"
    );

    config.peers[0].connected = true;
    config.layout_matrix = "self,right;local,right".to_string();
    assert!(
        resolve_capture_handoff_target(&config, None, SwitchDirection::Right).is_none(),
        "multiple local cells should invalidate edge handoff resolution"
    );
}

#[test]
fn resolve_capture_handoff_target_supports_peer_chain_and_return_to_local() {
    let config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: "left,self,right".to_string(),
        peers: vec![
            PeerConfig {
                peer_id: "peer-left".to_string(),
                display_name: "left".to_string(),
                address: "127.0.0.1:15100".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
        ],
        ..Default::default()
    };

    assert_eq!(
        resolve_capture_handoff_target(&config, Some("peer-left"), SwitchDirection::Right),
        Some(CaptureHandoffTarget::Local)
    );
    assert_eq!(
        resolve_capture_handoff_target(&config, Some("peer-right"), SwitchDirection::Left),
        Some(CaptureHandoffTarget::Local)
    );
    assert_eq!(
        resolve_capture_handoff_target(&config, Some("peer-left"), SwitchDirection::Left),
        None
    );
}

#[test]
fn resolve_capture_handoff_target_with_fallback_switches_single_peer_when_layout_is_unusable() {
    let config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: "A,B;C,D".to_string(),
        peers: vec![PeerConfig {
            peer_id: "peer-right".to_string(),
            display_name: "right".to_string(),
            address: "127.0.0.1:15101".to_string(),
            connected: true,
            last_seen: Utc::now(),
        }],
        ..Default::default()
    };

    assert_eq!(
        resolve_capture_handoff_target_with_fallback(&config, None, SwitchDirection::Right),
        Some(CaptureHandoffTarget::Peer("peer-right".to_string()))
    );
    assert_eq!(
        resolve_capture_handoff_target_with_fallback(
            &config,
            Some("peer-right"),
            SwitchDirection::Left
        ),
        Some(CaptureHandoffTarget::Local)
    );
}

#[test]
fn resolve_capture_handoff_target_with_fallback_respects_actionable_layout_edges() {
    let config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: "self,right".to_string(),
        peers: vec![PeerConfig {
            peer_id: "peer-right".to_string(),
            display_name: "right".to_string(),
            address: "127.0.0.1:15101".to_string(),
            connected: true,
            last_seen: Utc::now(),
        }],
        ..Default::default()
    };

    assert_eq!(
        resolve_capture_handoff_target_with_fallback(
            &config,
            Some("peer-right"),
            SwitchDirection::Right
        ),
        None,
        "with actionable layout, pushing deeper into the same edge should stay unresolved"
    );
}

#[test]
fn resolve_switch_all_target_order_prefers_layout_then_connected_remainder() {
    let config = RuntimeConfig {
        machine_id: "local-id".to_string(),
        device_name: "local-device".to_string(),
        layout_matrix: "right,self,left".to_string(),
        peers: vec![
            PeerConfig {
                peer_id: "peer-left".to_string(),
                display_name: "left".to_string(),
                address: "127.0.0.1:15100".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-right".to_string(),
                display_name: "right".to_string(),
                address: "127.0.0.1:15101".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
            PeerConfig {
                peer_id: "peer-zeta".to_string(),
                display_name: "zeta".to_string(),
                address: "127.0.0.1:15102".to_string(),
                connected: true,
                last_seen: Utc::now(),
            },
        ],
        ..Default::default()
    };

    let order = resolve_switch_all_target_order(&config);
    assert_eq!(
        order,
        vec![
            "peer-right".to_string(),
            "peer-left".to_string(),
            "peer-zeta".to_string()
        ]
    );
}

#[test]
fn validate_bind_address_rejects_invalid_input() {
    let err = validate_bind_address("not-an-addr").expect_err("must fail");
    assert!(err.to_string().contains("invalid bind address"));
}

#[test]
fn validate_bind_address_accepts_socket_addr() {
    validate_bind_address("127.0.0.1:50051").expect("valid bind");
}

#[test]
fn normalize_peer_address_adds_default_port_for_ip() {
    let normalized = normalize_peer_address("127.0.0.1", 15100).expect("normalize");
    assert_eq!(normalized, "127.0.0.1:15100");
}

#[test]
fn normalize_peer_address_keeps_hostname_with_port() {
    let normalized = normalize_peer_address("node-a.local:15100", 15100).expect("normalize");
    assert_eq!(normalized, "node-a.local:15100");
}

#[test]
fn normalize_peer_address_rejects_empty_input() {
    let err = normalize_peer_address("   ", 15100).expect_err("must fail");
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn validate_pipe_name_rejects_empty() {
    let err = validate_pipe_name("   ").expect_err("must fail");
    assert!(err.to_string().contains("must not be empty"));
}

#[test]
fn validate_pipe_name_rejects_path_separators() {
    let err = validate_pipe_name("bad/name").expect_err("must fail");
    assert!(err.to_string().contains("path separators"));
}

#[test]
fn validate_pipe_name_accepts_plain_name() {
    validate_pipe_name("boundlessd-api").expect("must accept");
}

#[test]
fn pairing_code_validation_consumes_valid_code() {
    let now = Utc::now();
    let mut codes = HashMap::from([("ABC-123".to_string(), now + Duration::minutes(5))]);

    validate_and_consume_pairing_code(&mut codes, "ABC-123", now).expect("must pass");
    assert!(codes.is_empty(), "valid code should be consumed");
}

#[test]
fn pairing_code_validation_rejects_unknown_code() {
    let now = Utc::now();
    let mut codes = HashMap::new();

    let err =
        validate_and_consume_pairing_code(&mut codes, "MISSING", now).expect_err("must reject");
    assert!(err.to_string().contains("invalid or was already used"));
}

#[test]
fn pairing_code_validation_rejects_and_consumes_expired_code() {
    let now = Utc::now();
    let mut codes = HashMap::from([("ABC-123".to_string(), now - Duration::minutes(1))]);

    let err =
        validate_and_consume_pairing_code(&mut codes, "ABC-123", now).expect_err("must reject");
    assert!(err.to_string().contains("has expired"));
    assert!(codes.is_empty(), "expired code should be consumed");
}

#[test]
fn ca_pem_validation_rejects_invalid_pem() {
    let err = validate_ca_cert_pem("not-pem").expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("certificate") || message.contains("PEM"));
}

#[test]
fn ca_pem_validation_accepts_generated_ca() {
    let root = std::env::temp_dir().join(format!("boundless-ca-test-{}", uuid::Uuid::new_v4()));
    let paths = core_security::SecurityPaths::for_root(root.join("security"));
    let identity =
        core_security::ensure_device_identity(&paths, "m1", "machine", None).expect("identity");
    validate_ca_cert_pem(&identity.ca_cert_pem).expect("must accept");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn sanitize_incoming_file_name_rejects_paths() {
    let err = sanitize_incoming_file_name("../evil.txt").expect_err("must reject");
    assert!(err.to_string().contains("path separators"));
}

#[test]
fn sanitize_incoming_file_name_accepts_plain_name() {
    let name = sanitize_incoming_file_name("report.txt").expect("must accept");
    assert_eq!(name, "report.txt");
}
