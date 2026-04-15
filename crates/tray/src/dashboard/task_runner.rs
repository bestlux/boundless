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
                    let _ = tx.send(AppMsg::SnapshotUpdated(snapshot));
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
                Ok(peer_machine_id) => {
                    let _ = tx.send(AppMsg::PairingComplete {
                        attempt_id,
                        result: GuidedPairingResult {
                            peer_machine_id,
                            orientation_selector: alias.unwrap_or(fallback_alias),
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
}
