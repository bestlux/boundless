use super::dashboard_test_support::{
    sample_discovered_peer, sample_guided_flow, sample_pairing_challenge, test_app,
};
use super::*;

#[test]
fn guided_flow_from_discovered_peer_builds_the_correct_guided_pairing_flow() {
    let peer = sample_discovered_peer();

    let flow = guided_flow_from_discovered_peer(&peer).expect("discovered peer should parse");

    assert_eq!(flow.dialog_title, "Pair with Office Desktop");
    assert_eq!(flow.host, "10.0.0.25");
    assert_eq!(flow.pairing_port, 15200);
    assert_eq!(flow.default_alias, "Office Desktop");
    assert_eq!(flow.orientation_selector_fallback, "Office Desktop");
}

#[test]
fn guided_flow_from_manual_input_accepts_a_valid_host_and_port() {
    let flow = guided_flow_from_manual_input("desk.local", "15200")
        .expect("manual input should accept a valid host and port");

    assert_eq!(flow.dialog_title, "Manual Pair desk.local");
    assert_eq!(flow.host, "desk.local");
    assert_eq!(flow.pairing_port, 15200);
    assert!(
        flow.default_alias.is_empty(),
        "manual pairing should not prefill an alias"
    );
    assert_eq!(flow.orientation_selector_fallback, "desk.local");
}

#[test]
fn guided_flow_from_manual_input_rejects_invalid_ports() {
    let zero_port =
        guided_flow_from_manual_input("desk.local", "0").expect_err("port zero must be rejected");
    assert!(
        zero_port.to_string().contains("1..=65535"),
        "zero port error should explain the valid range"
    );

    let non_numeric = guided_flow_from_manual_input("desk.local", "not-a-number")
        .expect_err("non-numeric input must be rejected");
    assert!(
        non_numeric.to_string().contains("number"),
        "non-numeric port error should explain the parsing failure"
    );
}

#[test]
fn begin_pairing_flow_sets_pairing_request_state() {
    let mut app = test_app();
    let flow = sample_guided_flow();

    app.pairing_challenge = Some(sample_pairing_challenge());
    app.pairing_code = "123456".to_string();
    app.pairing_alias = "Old Alias".to_string();
    app.pairing_retry_available = true;
    app.pairing_last_error = Some("previous failure".to_string());

    app.begin_pairing_flow(flow.clone());

    let active_flow = app
        .pairing_flow
        .as_ref()
        .expect("begin_pairing_flow should store the active flow");
    assert!(app.pairing_in_progress);
    assert_eq!(active_flow.dialog_title, flow.dialog_title);
    assert_eq!(active_flow.host, flow.host);
    assert_eq!(active_flow.pairing_port, flow.pairing_port);
    assert_eq!(active_flow.default_alias, flow.default_alias);
    assert_eq!(
        active_flow.orientation_selector_fallback,
        flow.orientation_selector_fallback
    );
    assert!(app.pairing_challenge.is_none());
    assert!(app.pairing_code.is_empty());
    assert_eq!(app.pairing_alias, "Office Desktop");
    assert!(!app.pairing_retry_available);
    assert!(app.pairing_last_error.is_none());
}

#[test]
fn cancel_pairing_flow_after_request_stage_start_clears_pairing_state() {
    let mut app = test_app();

    app.begin_pairing_flow(sample_guided_flow());
    app.pairing_challenge = Some(sample_pairing_challenge());
    app.pairing_retry_available = true;

    app.cancel_pairing_flow();

    assert!(!app.pairing_in_progress);
    assert!(app.pairing_flow.is_none());
    assert!(app.pairing_challenge.is_none());
    assert!(!app.pairing_retry_available);
}

#[test]
fn cancel_pairing_flow_does_not_change_selected_tab() {
    let mut app = test_app();
    app.selected_tab = Tab::Settings;
    app.begin_pairing_flow(sample_guided_flow());

    app.cancel_pairing_flow();

    assert!(app.selected_tab == Tab::Settings);
}

#[test]
fn cancel_pairing_flow_does_not_emit_success_state() {
    let mut app = test_app();

    app.begin_pairing_flow(sample_guided_flow());
    app.pairing_last_error = Some("still waiting for pairing".to_string());
    app.push_toast("existing toast".to_string(), true);

    app.cancel_pairing_flow();

    assert_eq!(
        app.pairing_last_error.as_deref(),
        Some("still waiting for pairing")
    );
    assert!(
        !app.toasts.is_empty(),
        "cancel should not clear existing toasts"
    );
}

#[test]
fn filter_connectable_discovered_peers_hides_self_and_existing_pairings() {
    let discovered = vec![
        UiDiscoveredPeer {
            machine_id: "local-machine-1234".to_string(),
            display_name: "This PC".to_string(),
            endpoint: "10.0.0.1:15100".to_string(),
        },
        UiDiscoveredPeer {
            machine_id: "peer-machine-1234".to_string(),
            display_name: "Different Alias".to_string(),
            endpoint: "10.0.0.25:15100".to_string(),
        },
        UiDiscoveredPeer {
            machine_id: "peer-machine-5678".to_string(),
            display_name: "Office Desktop".to_string(),
            endpoint: "10.0.0.26:15100".to_string(),
        },
    ];
    let paired = vec![UiPairedPeer {
        peer_id: "peer-machine-1234".to_string(),
        display_name: "Stored Alias".to_string(),
        address: "10.0.0.25:15100".to_string(),
        connected: true,
        health_state: "connected".to_string(),
        health_reason: "connected".to_string(),
    }];

    let filtered = filter_connectable_discovered_peers(discovered, "LOCAL-MACHINE-1234", &paired);

    assert_eq!(filtered.len(), 1, "only the new remote peer should remain");
    assert_eq!(filtered[0].machine_id, "peer-machine-5678");
    assert_eq!(filtered[0].display_name, "Office Desktop");
}
