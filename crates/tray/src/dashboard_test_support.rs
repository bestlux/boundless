use super::*;

pub(super) fn test_app() -> DashboardApp {
    let (tx, rx) = mpsc::channel();

    DashboardApp {
        ctx: Arc::new(AppContext {
            endpoint: "fixture://disabled".to_string(),
            start_daemon: false,
            daemon_candidates: Vec::new(),
        }),
        _tray_icon: None,
        snapshot: UiSnapshot::default(),
        task_runner: DashboardTaskRunner::recording(),
        snapshot_error: None,
        pending_peer_removal: None,
        support_status: None,
        input_pause_requested: false,
        input_change_pending: false,
        paired_testing: None,
        paired_testing_updated_at: None,
        paired_testing_pending: false,
        paired_testing_error: None,
        paired_testing_peer: String::new(),
        file_send_peer: String::new(),
        file_send_pending: false,
        tx,
        rx,
        toasts: Vec::new(),
        toast_seq: 0,
        pairing_last_error: None,
        pairing_retry_available: false,
        pairing_role_reversal_available: false,
        pairing_role_reversal_attempt_id: None,
        pairing_role_reversal_message: None,
        selected_tab: Tab::Status,
        manual_host: String::new(),
        manual_port: app_services::desktop::DEFAULT_PAIRING_PORT.to_string(),
        pairing_flow: None,
        pairing_challenge: None,
        pairing_code: String::new(),
        pairing_alias: String::new(),
        pairing_in_progress: false,
        pairing_attempt_seq: 0,
        active_pairing_attempt_id: None,
        pending_onboarding_focus: false,
        onboarding_focus_shown: false,
        pending_service_recovery_focus: false,
        service_recovery: None,
        exit_requested: false,
        exit_requested_signal: Arc::new(AtomicBool::new(false)),
        native_window_handle: None,
        activation_requested: Arc::new(AtomicBool::new(false)),
        _shutdown_subclass: None,
        _single_instance_guard: None,
        _input_broker_supervisor: None,
        elevated_input_controller: None,
        layout_grid: HashMap::new(),
        layout_unassigned: Vec::new(),
        layout_initialized: false,
        layout_selected_peer: String::new(),
        dragging_peer: None,
        last_layout_matrix: String::new(),
        last_layout_peer_ids: Vec::new(),
        file_receive_dir_edit: String::new(),
        file_receive_dir_last_snapshot: String::new(),
        hotkey_edits: BTreeMap::new(),
        hotkey_last_snapshot: BTreeMap::new(),
        prev_layout_grid: None,
        prev_layout_unassigned: None,
        confirm_apply_pending: false,
        confirm_network_reset_pending: false,
        confirm_safe_reset_pending: false,
    }
}

pub(super) fn sample_discovered_peer() -> UiDiscoveredPeer {
    UiDiscoveredPeer {
        machine_id: "peer-machine-1234".to_string(),
        display_name: "Office Desktop".to_string(),
        endpoint: "10.0.0.25:15100".to_string(),
        endpoint_candidates: vec!["10.0.0.25:15100".to_string()],
    }
}

pub(super) fn sample_guided_flow() -> GuidedPairingFlow {
    GuidedPairingFlow {
        dialog_title: "Pair with Office Desktop".to_string(),
        host: "10.0.0.25".to_string(),
        pairing_port: 15200,
        default_alias: "Office Desktop".to_string(),
        orientation_selector_fallback: "Office Desktop".to_string(),
        endpoint_candidates: vec!["10.0.0.25:15100".to_string()],
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

pub(super) fn sample_first_run_snapshot() -> UiSnapshot {
    UiSnapshot {
        generated_at: "2026-03-03T18:00:00Z".to_string(),
        daemon_online: true,
        daemon_version: "5.0.16".to_string(),
        machine_id: "local-machine-1234".to_string(),
        layout_matrix: "self".to_string(),
        features: BTreeMap::from([
            ("share_input".to_string(), true),
            ("share_clipboard".to_string(), true),
            ("transfer_file".to_string(), true),
            ("easy_mouse".to_string(), true),
            ("wrap_mouse".to_string(), true),
        ]),
        hotkeys: BTreeMap::from([
            (
                "toggle_easy_mouse".to_string(),
                "Ctrl+Alt+Shift+E".to_string(),
            ),
            ("lock_machine".to_string(), "Ctrl+Alt+Shift+L".to_string()),
            ("switch_all".to_string(), "Disabled".to_string()),
            ("reconnect".to_string(), "Ctrl+Alt+Shift+R".to_string()),
        ]),
        discovered_peers: Vec::new(),
        paired_peers: Vec::new(),
        pending_requests: Vec::new(),
        anti_idle_config: UiAntiIdleConfig {
            enabled: true,
            recent_activity_window_secs: 300,
            allow_on_battery: false,
            keep_display_on: false,
        },
        anti_idle_status: UiAntiIdleStatus {
            supported: true,
            enabled: true,
            active: false,
            display_required: false,
            reason: "none".to_string(),
        },
        file_transfer_config: UiFileTransferConfig {
            receive_dir: r"C:\Users\Test\Downloads\Boundless".to_string(),
            organize_by_peer: false,
            auto_accept_trusted_peers: false,
            max_file_bytes: 100 * 1024 * 1024,
        },
        file_transfers: Vec::new(),
        input_handoff_config: UiInputHandoffConfig {
            block_screen_corners: true,
            corner_block_px: 24,
            relative_mouse: false,
            hide_cursor_at_edge: false,
            draw_cursor_marker: false,
        },
        input_runtime: UiInputRuntime {
            owner_peer_id: String::new(),
            configured_capture_target_peer_id: String::new(),
            active_capture_target_peer_id: String::new(),
            lock_active: false,
            lock_supported: true,
            capture_backend_mode: "windows_hooks".to_string(),
            pending_inject_frames: 0,
            pending_inject_high_water: 0,
        },
        clipboard_runtime: UiClipboardRuntime {
            backend_mode: "direct".to_string(),
        },
    }
}

pub(super) fn sample_paired_snapshot() -> UiSnapshot {
    UiSnapshot {
        paired_peers: vec![UiPairedPeer {
            peer_id: "peer-1234".to_string(),
            display_name: "Office Desktop".to_string(),
            address: "10.0.0.25:15100".to_string(),
            connected: false,
            health_state: "disconnected".to_string(),
            health_reason: "no recent peer event".to_string(),
            trust_state: "trusted".to_string(),
            trusted_since: "2026-03-03T18:00:00Z".to_string(),
            trust_fingerprint: "abcdef1234567890".to_string(),
            device_identity: "peer-1234".to_string(),
        }],
        ..sample_first_run_snapshot()
    }
}
