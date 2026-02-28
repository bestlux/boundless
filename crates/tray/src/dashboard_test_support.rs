#![allow(dead_code)]

use super::*;

pub(super) fn test_app() -> DashboardApp {
    let (tx, rx) = mpsc::channel();

    DashboardApp {
        ctx: Arc::new(AppContext {
            endpoint: "npipe://./pipe/boundlessd-api".to_string(),
            start_daemon: false,
            ctl_candidates: Vec::new(),
            daemon_candidates: Vec::new(),
        }),
        _tray_icon: None,
        snapshot: UiSnapshot::default(),
        last_error: None,
        last_message_is_error: true,
        tx,
        rx,
        selected_tab: Tab::Status,
        manual_host: String::new(),
        manual_port: "15200".to_string(),
        pairing_flow: None,
        pairing_challenge: None,
        pairing_code: String::new(),
        pairing_alias: String::new(),
        pairing_in_progress: false,
        pairing_retry_available: false,
        pairing_attempt_seq: 0,
        active_pairing_attempt_id: None,
        layout_grid: HashMap::new(),
        layout_unassigned: Vec::new(),
        layout_initialized: false,
        dragging_peer: None,
        last_layout_matrix: String::new(),
    }
}

pub(super) fn sample_discovered_peer() -> UiDiscoveredPeer {
    UiDiscoveredPeer {
        machine_id: "peer-machine-1234".to_string(),
        display_name: "Office Desktop".to_string(),
        endpoint: "10.0.0.25:15100".to_string(),
    }
}

pub(super) fn sample_guided_flow() -> GuidedPairingFlow {
    GuidedPairingFlow {
        dialog_title: "Pair with Office Desktop".to_string(),
        host: "10.0.0.25".to_string(),
        pairing_port: 15200,
        default_alias: "Office Desktop".to_string(),
        orientation_selector_fallback: "Office Desktop".to_string(),
    }
}

pub(super) fn sample_pairing_challenge() -> PairingChallengeState {
    PairingChallengeState {
        request_id: "request-1234".to_string(),
        verification_nonce: "nonce-1234".to_string(),
        expires_at: "2026-02-27T12:00:00Z".to_string(),
    }
}

pub(super) fn sample_pairing_result() -> GuidedPairingResult {
    GuidedPairingResult {
        peer_machine_id: "peer-machine-1234".to_string(),
        orientation_selector: "Office Desktop".to_string(),
    }
}
