use super::*;

pub(super) struct DashboardTaskRunner;

pub(super) struct SubmitPairingCodeTask {
    pub(super) tx: Sender<AppMsg>,
    pub(super) endpoint: String,
    pub(super) attempt_id: u64,
    pub(super) challenge: PairingChallengeState,
    pub(super) flow: GuidedPairingFlow,
    pub(super) code: String,
    pub(super) alias: Option<String>,
    pub(super) egui_ctx: egui::Context,
}

impl DashboardTaskRunner {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn spawn<F>(job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        std::thread::spawn(job);
    }

    pub(super) fn spawn_with_repaint<F>(egui_ctx: egui::Context, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        Self::spawn(move || {
            job();
            egui_ctx.request_repaint();
        });
    }

    pub(super) fn spawn_snapshot_watch(
        app_ctx: Arc<AppContext>,
        tx: Sender<AppMsg>,
        egui_ctx: egui::Context,
    ) {
        Self::spawn(move || {
            let mut next_start_attempt = Instant::now();
            let mut start_backoff = Duration::from_secs(2);
            loop {
                match watch_ui_snapshots_blocking(&app_ctx.endpoint, |snapshot| {
                    next_start_attempt = Instant::now();
                    start_backoff = Duration::from_secs(2);
                    let _ = tx.send(AppMsg::SnapshotUpdated(Box::new(snapshot)));
                    egui_ctx.request_repaint();
                    Ok(())
                }) {
                    Ok(()) => {}
                    Err(e) => {
                        let mut message = e.to_string();
                        if app_ctx.start_daemon && Instant::now() >= next_start_attempt {
                            match ensure_daemon_available_blocking(&app_ctx) {
                                Ok(Some(path)) => {
                                    next_start_attempt = Instant::now() + Duration::from_secs(8);
                                    start_backoff = Duration::from_secs(2);
                                    let _ = tx.send(AppMsg::ActionComplete(format!(
                                        "Boundless daemon is available via `{path}`"
                                    )));
                                    egui_ctx.request_repaint();
                                    continue;
                                }
                                Ok(None) => {
                                    next_start_attempt = Instant::now() + Duration::from_secs(8);
                                    start_backoff = Duration::from_secs(2);
                                    continue;
                                }
                                Err(start_error) => {
                                    message =
                                        format!("{message}\nstart-daemon failed: {start_error}");
                                    next_start_attempt = Instant::now() + start_backoff;
                                    let next_seconds =
                                        (start_backoff.as_secs().saturating_mul(2)).clamp(2, 60);
                                    start_backoff = Duration::from_secs(next_seconds);
                                }
                            }
                        }
                        let _ = tx.send(AppMsg::SnapshotError(message));
                        egui_ctx.request_repaint();
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    pub(super) fn request_pairing_code(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        attempt_id: u64,
        flow: GuidedPairingFlow,
        egui_ctx: egui::Context,
    ) {
        Self::spawn_with_repaint(egui_ctx, move || {
            match pair_nearby_request_code_blocking(&endpoint, flow.host.clone(), flow.pairing_port)
            {
                Ok(NearbyRequestCodeStart::CodeRequired {
                    request_id,
                    verification_nonce,
                    expires_at,
                }) => {
                    let challenge = PairingChallengeState {
                        request_id,
                        verification_nonce,
                        expires_at,
                    };
                    let _ = tx.send(AppMsg::PairingChallenge {
                        attempt_id,
                        challenge,
                    });
                }
                Ok(NearbyRequestCodeStart::Unsupported { reason }) => {
                    let _ = tx.send(AppMsg::PairingFailed {
                        attempt_id,
                        error: format!("Target does not support guided pairing: {}", reason),
                    });
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::PairingFailed {
                        attempt_id,
                        error: error.to_string(),
                    });
                }
            }
        });
    }

    pub(super) fn submit_pairing_code(&self, task: SubmitPairingCodeTask) {
        let SubmitPairingCodeTask {
            tx,
            endpoint,
            attempt_id,
            challenge,
            flow,
            code,
            alias,
            egui_ctx,
        } = task;
        let fallback_alias = flow.orientation_selector_fallback.clone();
        Self::spawn_with_repaint(egui_ctx, move || {
            match pair_nearby_submit_code_blocking(
                &endpoint,
                challenge.request_id,
                code,
                challenge.verification_nonce,
                flow.host,
                flow.pairing_port,
                alias.clone(),
            ) {
                Ok(submit_result) => {
                    let _ = tx.send(AppMsg::PairingComplete {
                        attempt_id,
                        result: GuidedPairingResult {
                            peer_machine_id: submit_result.peer_machine_id,
                            orientation_selector: alias.unwrap_or(fallback_alias),
                            message: submit_result.message,
                        },
                    });
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::PairingFailed {
                        attempt_id,
                        error: error.to_string(),
                    });
                }
            }
        });
    }

    pub(super) fn approve_request(&self, tx: Sender<AppMsg>, endpoint: String, request_id: String) {
        Self::spawn(move || match approve_nearby_pairing_request_blocking(&endpoint, &request_id) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn reject_request(&self, tx: Sender<AppMsg>, endpoint: String, request_id: String) {
        Self::spawn(move || match reject_nearby_pairing_request_blocking(&endpoint, &request_id) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn apply_layout(&self, tx: Sender<AppMsg>, endpoint: String, matrix_spec: String) {
        Self::spawn(move || match layout_set_blocking(&endpoint, matrix_spec) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(format!("Layout failed: {}", error)));
            }
        });
    }

    pub(super) fn set_anti_idle_config(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        enabled: bool,
        recent_activity_window_secs: u32,
        allow_on_battery: bool,
        keep_display_on: bool,
    ) {
        Self::spawn(move || {
            match set_anti_idle_config_blocking(
                &endpoint,
                enabled,
                recent_activity_window_secs,
                allow_on_battery,
                keep_display_on,
            ) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            }
        });
    }

    pub(super) fn set_file_transfer_config(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        receive_dir: String,
        organize_by_peer: bool,
        auto_accept_trusted_peers: bool,
        max_file_bytes: u64,
    ) {
        Self::spawn(move || {
            match set_file_transfer_config_blocking(
                &endpoint,
                receive_dir,
                organize_by_peer,
                auto_accept_trusted_peers,
                max_file_bytes,
            ) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            }
        });
    }

    pub(super) fn set_feature(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        name: String,
        enabled: bool,
    ) {
        Self::spawn(move || match set_feature_blocking(&endpoint, name, enabled) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn set_input_handoff_config(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        config: UiInputHandoffConfig,
    ) {
        Self::spawn(move || {
            match set_input_handoff_config_blocking(
                &endpoint,
                config.block_screen_corners,
                config.corner_block_px,
                config.relative_mouse,
                config.hide_cursor_at_edge,
                config.draw_cursor_marker,
            ) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            }
        });
    }

    pub(super) fn set_hotkey(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        action: String,
        combo: String,
    ) {
        Self::spawn(move || match set_hotkey_blocking(&endpoint, action, combo) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn safe_reset(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        network_only: bool,
        all: bool,
        confirm: String,
    ) {
        Self::spawn(move || match safe_reset_blocking(&endpoint, network_only, all, confirm) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn open_receive_folder(&self, tx: Sender<AppMsg>, receive_dir: String) {
        Self::spawn(move || {
            let result = ProcessCommand::new("explorer")
                .arg(&receive_dir)
                .spawn()
                .map(|_| format!("Opened receive folder: {receive_dir}"))
                .map_err(|error| format!("Failed to open receive folder: {error}"));
            let _ = tx.send(match result {
                Ok(message) => AppMsg::ActionComplete(message),
                Err(error) => AppMsg::ActionFailed(error),
            });
        });
    }

    pub(super) fn send_files_to_peer(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        peer_id: String,
        paths: Vec<String>,
    ) {
        Self::spawn(move || match send_files_to_peer_blocking(&endpoint, peer_id, paths) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn reconnect_all_peers(&self, tx: Sender<AppMsg>, endpoint: String) {
        Self::spawn(move || match trigger_hotkey_action_blocking(&endpoint, "reconnect") {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }

    pub(super) fn remove_peer(&self, tx: Sender<AppMsg>, endpoint: String, peer_id: String) {
        Self::spawn(move || match remove_peer_blocking(&endpoint, peer_id) {
            Ok(msg) => {
                let _ = tx.send(AppMsg::ActionComplete(msg));
            }
            Err(error) => {
                let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
            }
        });
    }
}
