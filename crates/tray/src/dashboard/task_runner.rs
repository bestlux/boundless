use super::*;

#[cfg(test)]
type RecordedCommands = Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

#[derive(Clone, Default)]
pub(super) struct DashboardTaskRunner {
    #[cfg(test)]
    recording: Option<RecordedCommands>,
}

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

pub(super) struct StartRoleReversalPairingTask {
    pub(super) tx: Sender<AppMsg>,
    pub(super) endpoint: String,
    pub(super) attempt_id: u64,
    pub(super) flow: GuidedPairingFlow,
    pub(super) code: String,
    pub(super) alias: Option<String>,
    pub(super) role_reversal_attempt_id: String,
    pub(super) egui_ctx: egui::Context,
}

impl DashboardTaskRunner {
    pub(super) fn choose_and_send_file(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        peer_id: String,
        owner: Option<isize>,
    ) {
        if self.record(
            "choose_and_send_file",
            serde_json::json!({"peer_id": peer_id}),
        ) {
            return;
        }
        Self::spawn(move || {
            let result = dashboard_window::choose_file_to_send(owner).and_then(|selection| {
                selection
                    .map(|path| send_files_to_peer_blocking(&endpoint, peer_id, vec![path]))
                    .transpose()
            });
            let _ = tx.send(AppMsg::FileSendComplete(
                result.map_err(|error| error.to_string()),
            ));
        });
    }

    pub(super) fn paired_testing_permission(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        change: Option<(String, u32)>,
    ) {
        if self.record(
            "paired_testing_permission",
            serde_json::json!({"change": change}),
        ) {
            return;
        }
        Self::spawn(move || {
            let result = block_on_result(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let mut client = connect_control_plane(&endpoint).await?;
                    let reply = match change {
                        Some((peer_id, duration_seconds)) => {
                            client
                                .paired_test_consent(
                                    ipc_api::boundless::v1::PairedTestConsentRequest {
                                        peer_id,
                                        duration_seconds,
                                    },
                                )
                                .await?
                        }
                        None => client.get_paired_test_consent(Empty {}).await?,
                    }
                    .into_inner();
                    Ok(serde_json::from_str::<
                        app_services::paired_testing::PairedTestConsent,
                    >(&reply.json)?)
                })
                .await
                .context("Paired testing permission timed out")?
            });
            let _ = tx.send(AppMsg::PairedTestingUpdated(
                result.map_err(|error| error.to_string()),
            ));
        });
    }

    pub(super) fn set_input_sharing(&self, tx: Sender<AppMsg>, endpoint: String, enabled: bool) {
        if self.record("set_input_sharing", serde_json::json!({"enabled": enabled})) {
            return;
        }
        Self::spawn(move || {
            let result = block_on_result(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    set_feature(&endpoint, "share_input".to_string(), enabled).await?;
                    if !enabled {
                        connect_control_plane(&endpoint)
                            .await?
                            .clear_input_capture_target(Empty {})
                            .await?;
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await
                .context("Input settings timed out")?
            });
            let message = match result {
                Ok(()) => AppMsg::InputSharingComplete(enabled),
                Err(error) => AppMsg::InputSharingFailed(format!(
                    "Could not update input sharing: {error}. The local tray capture remains paused; a separate daemon needs to be reachable before its capture can be stopped."
                )),
            };
            let _ = tx.send(message);
        });
    }

    pub(super) fn export_support(&self, tx: Sender<AppMsg>, endpoint: String) {
        if self.record(
            "export_support",
            serde_json::json!({"include_filenames": false}),
        ) {
            return;
        }
        Self::spawn(move || {
            let result = block_on_result(async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    let response = connect_control_plane(&endpoint)
                        .await?
                        .dump_diagnostics(ipc_api::boundless::v1::DiagnosticsDumpRequest {
                            output_path: String::new(),
                            include_filenames: false,
                        })
                        .await?
                        .into_inner();
                    Ok::<String, anyhow::Error>(format!(
                        "Saved report: {}\nRedaction manifest: {}",
                        response.bundle_path, response.manifest_path
                    ))
                })
                .await
                .context("Report export timed out")?
            });
            let message = match result {
                Ok(message) => message,
                Err(error) => format!(
                    "Report could not be saved: {error}. Restore the background runtime and retry."
                ),
            };
            let _ = tx.send(AppMsg::SupportExportComplete(message));
        });
    }

    pub(super) fn new() -> Self {
        Self::default()
    }

    fn record(&self, name: &str, arguments: serde_json::Value) -> bool {
        #[cfg(test)]
        if let Some(recording) = &self.recording {
            recording
                .lock()
                .expect("fixture command sink")
                .push((name.to_string(), arguments));
            return true;
        }
        let _ = (name, arguments);
        false
    }

    #[cfg(test)]
    pub(super) fn recording() -> Self {
        Self {
            recording: Some(Arc::new(std::sync::Mutex::new(Vec::new()))),
        }
    }

    #[cfg(test)]
    pub(super) fn recorded_commands(&self) -> Vec<(String, serde_json::Value)> {
        self.recording
            .as_ref()
            .expect("recording fixture")
            .lock()
            .expect("fixture commands")
            .clone()
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
                        if app_ctx.start_daemon
                            && let Some(offer) = boundless_service_recovery_offer(&app_ctx.endpoint)
                        {
                            let _ = tx.send(AppMsg::ServiceRecoveryRequired(offer));
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
        if self.record("request_pairing_code", serde_json::json!({"endpoint": endpoint, "attempt_id": attempt_id, "flow": format!("{flow:?}")})) { return; }

        Self::spawn_with_repaint(egui_ctx, move || {
            match pair_nearby_request_code_blocking(
                &endpoint,
                flow.host.clone(),
                flow.pairing_port,
                flow.endpoint_candidates.clone(),
            ) {
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

    pub(super) fn recover_boundless_service(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        egui_ctx: egui::Context,
    ) {
        if self.record(
            "recover_boundless_service",
            serde_json::json!({"endpoint": endpoint}),
        ) {
            return;
        }

        Self::spawn_with_repaint(egui_ctx, move || {
            let message = match recover_boundless_service_blocking(&endpoint) {
                Ok(message) => AppMsg::ServiceRecoveryComplete(message),
                Err(error) => AppMsg::ServiceRecoveryFailed(format!(
                    "Could not start BoundlessService: {error}"
                )),
            };
            let _ = tx.send(message);
        });
    }

    pub(super) fn submit_pairing_code(&self, task: SubmitPairingCodeTask) {
        if self.record(
            "submit_pairing_code",
            serde_json::json!({"task": "pairing"}),
        ) {
            return;
        }

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
                NearbySubmitCode {
                    request_id: challenge.request_id,
                    code,
                    verification_nonce: challenge.verification_nonce,
                    host: flow.host,
                    port: flow.pairing_port,
                    alias: alias.clone(),
                    endpoint_candidates: flow.endpoint_candidates,
                },
            ) {
                Ok(submit_result) => {
                    let _ = tx.send(AppMsg::PairingComplete {
                        attempt_id,
                        result: GuidedPairingResult {
                            peer_machine_id: submit_result.peer_machine_id,
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

    pub(super) fn start_role_reversal_pairing(&self, task: StartRoleReversalPairingTask) {
        if self.record(
            "start_role_reversal_pairing",
            serde_json::json!({"task": "pairing"}),
        ) {
            return;
        }

        let StartRoleReversalPairingTask {
            tx,
            endpoint,
            attempt_id,
            flow,
            code,
            alias,
            role_reversal_attempt_id,
            egui_ctx,
        } = task;
        let fallback_alias = flow.orientation_selector_fallback.clone();
        Self::spawn_with_repaint(egui_ctx, move || {
            match pair_nearby_role_reversal_blocking(
                &endpoint,
                NearbyRoleReversalRequest {
                    code,
                    host: flow.host,
                    port: flow.pairing_port,
                    alias: alias.clone(),
                    endpoint_candidates: flow.endpoint_candidates,
                    attempt_id: role_reversal_attempt_id,
                },
            ) {
                Ok(result) => {
                    let _ = tx.send(AppMsg::PairingComplete {
                        attempt_id,
                        result: GuidedPairingResult {
                            peer_machine_id: result.peer_machine_id,
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
        if self.record(
            "approve_request",
            serde_json::json!({"endpoint": endpoint, "request_id": request_id}),
        ) {
            return;
        }

        Self::spawn(move || {
            match approve_nearby_pairing_request_blocking(&endpoint, &request_id) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            }
        });
    }

    pub(super) fn reject_request(&self, tx: Sender<AppMsg>, endpoint: String, request_id: String) {
        if self.record(
            "reject_request",
            serde_json::json!({"endpoint": endpoint, "request_id": request_id}),
        ) {
            return;
        }

        Self::spawn(
            move || match reject_nearby_pairing_request_blocking(&endpoint, &request_id) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn apply_layout(&self, tx: Sender<AppMsg>, endpoint: String, matrix_spec: String) {
        if self.record(
            "apply_layout",
            serde_json::json!({"endpoint": endpoint, "matrix_spec": matrix_spec}),
        ) {
            return;
        }

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
        if self.record("set_anti_idle_config", serde_json::json!({"endpoint": endpoint, "enabled": enabled, "recent_activity_window_secs": recent_activity_window_secs, "allow_on_battery": allow_on_battery, "keep_display_on": keep_display_on})) { return; }

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
        if self.record("set_file_transfer_config", serde_json::json!({"endpoint": endpoint, "receive_dir": receive_dir, "organize_by_peer": organize_by_peer, "auto_accept_trusted_peers": auto_accept_trusted_peers, "max_file_bytes": max_file_bytes})) { return; }

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
        if self.record(
            "set_feature",
            serde_json::json!({"endpoint": endpoint, "name": name, "enabled": enabled}),
        ) {
            return;
        }

        Self::spawn(
            move || match set_feature_blocking(&endpoint, name, enabled) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn set_input_handoff_config(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        config: UiInputHandoffConfig,
    ) {
        if self.record(
            "set_input_handoff_config",
            serde_json::json!({"endpoint": endpoint, "config": format!("{config:?}")}),
        ) {
            return;
        }

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
        if self.record(
            "set_hotkey",
            serde_json::json!({"endpoint": endpoint, "action": action, "combo": combo}),
        ) {
            return;
        }

        Self::spawn(
            move || match set_hotkey_blocking(&endpoint, action, combo) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn safe_reset(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        network_only: bool,
        all: bool,
        confirm: String,
    ) {
        if self.record("safe_reset", serde_json::json!({"endpoint": endpoint, "network_only": network_only, "all": all, "confirm": confirm})) { return; }

        Self::spawn(
            move || match safe_reset_blocking(&endpoint, network_only, all, confirm) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn open_receive_folder(&self, tx: Sender<AppMsg>, receive_dir: String) {
        if self.record(
            "open_receive_folder",
            serde_json::json!({"receive_dir": receive_dir}),
        ) {
            return;
        }

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

    pub(super) fn open_received_file_location(&self, tx: Sender<AppMsg>, final_path: String) {
        if self.record(
            "open_received_file_location",
            serde_json::json!({"final_path": final_path}),
        ) {
            return;
        }

        Self::spawn(move || {
            let result = ProcessCommand::new("explorer")
                .arg(format!("/select,{final_path}"))
                .spawn()
                .map(|_| "Opened received file location".to_string())
                .map_err(|error| format!("Failed to open received file location: {error}"));
            let _ = tx.send(match result {
                Ok(message) => AppMsg::ActionComplete(message),
                Err(error) => AppMsg::ActionFailed(error),
            });
        });
    }

    pub(super) fn cancel_file_transfer(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        transfer_id: String,
    ) {
        if self.record(
            "cancel_file_transfer",
            serde_json::json!({"endpoint": endpoint, "transfer_id": transfer_id}),
        ) {
            return;
        }

        Self::spawn(
            move || match cancel_file_transfer_blocking(&endpoint, transfer_id) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn retry_file_transfer(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        transfer_id: String,
    ) {
        if self.record(
            "retry_file_transfer",
            serde_json::json!({"endpoint": endpoint, "transfer_id": transfer_id}),
        ) {
            return;
        }

        Self::spawn(
            move || match retry_file_transfer_blocking(&endpoint, transfer_id) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn clear_completed_file_transfers(&self, tx: Sender<AppMsg>, endpoint: String) {
        if self.record(
            "clear_completed_file_transfers",
            serde_json::json!({"endpoint": endpoint}),
        ) {
            return;
        }

        Self::spawn(
            move || match clear_completed_file_transfers_blocking(&endpoint) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn send_files_to_peer(
        &self,
        tx: Sender<AppMsg>,
        endpoint: String,
        peer_id: String,
        paths: Vec<String>,
    ) {
        if self.record(
            "send_files_to_peer",
            serde_json::json!({"endpoint": endpoint, "peer_id": peer_id, "paths": paths}),
        ) {
            return;
        }

        Self::spawn(
            move || match send_files_to_peer_blocking(&endpoint, peer_id, paths) {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn reconnect_all_peers(&self, tx: Sender<AppMsg>, endpoint: String) {
        if self.record(
            "reconnect_all_peers",
            serde_json::json!({"endpoint": endpoint}),
        ) {
            return;
        }

        Self::spawn(
            move || match trigger_hotkey_action_blocking(&endpoint, "reconnect") {
                Ok(msg) => {
                    let _ = tx.send(AppMsg::ActionComplete(msg));
                }
                Err(error) => {
                    let _ = tx.send(AppMsg::ActionFailed(error.to_string()));
                }
            },
        );
    }

    pub(super) fn remove_peer(&self, tx: Sender<AppMsg>, endpoint: String, peer_id: String) {
        if self.record(
            "remove_peer",
            serde_json::json!({"endpoint": endpoint, "peer_id": peer_id}),
        ) {
            return;
        }

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
