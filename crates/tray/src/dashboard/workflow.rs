use super::*;

impl DashboardApp {
    pub(super) fn task_runner(&self) -> DashboardTaskRunner {
        self.task_runner.clone()
    }

    pub(super) fn start_pairing(&mut self, flow: GuidedPairingFlow, egui_ctx: egui::Context) {
        let attempt_id = self.begin_pairing_flow(flow.clone());
        self.task_runner().request_pairing_code(
            self.tx.clone(),
            self.ctx.endpoint.clone(),
            attempt_id,
            flow,
            egui_ctx,
        );
    }

    fn submit_pairing_code(&mut self, egui_ctx: egui::Context) {
        if let (Some(challenge), Some(flow), Some(attempt_id)) = (
            self.pairing_challenge.clone(),
            self.pairing_flow.clone(),
            self.active_pairing_attempt_id,
        ) {
            let code = self.pairing_code.clone();
            let alias = if self.pairing_alias.trim().is_empty() {
                None
            } else {
                Some(self.pairing_alias.clone())
            };

            self.pairing_in_progress = true;
            self.task_runner()
                .submit_pairing_code(SubmitPairingCodeTask {
                    tx: self.tx.clone(),
                    endpoint: self.ctx.endpoint.clone(),
                    attempt_id,
                    challenge,
                    flow,
                    code,
                    alias,
                    egui_ctx,
                });
        }
    }

    fn start_role_reversal_pairing(&mut self, egui_ctx: egui::Context) {
        if let Err(error) = validate_pairing_code(&self.pairing_code) {
            self.pairing_last_error = Some(error.to_string());
            return;
        }

        if let (Some(flow), Some(attempt_id), Some(role_reversal_attempt_id)) = (
            self.pairing_flow.clone(),
            self.active_pairing_attempt_id,
            self.pairing_role_reversal_attempt_id.clone(),
        ) {
            let code = self.pairing_code.clone();
            let alias = if self.pairing_alias.trim().is_empty() {
                None
            } else {
                Some(self.pairing_alias.clone())
            };

            self.pairing_in_progress = true;
            self.pairing_last_error = Some(role_reversal_next_action_message(
                &flow,
                Some(&role_reversal_attempt_id),
                true,
            ));
            self.task_runner()
                .start_role_reversal_pairing(StartRoleReversalPairingTask {
                    tx: self.tx.clone(),
                    endpoint: self.ctx.endpoint.clone(),
                    attempt_id,
                    flow,
                    code,
                    alias,
                    role_reversal_attempt_id,
                    egui_ctx,
                });
        }
    }

    pub(super) fn confirm_pairing_code(&mut self, egui_ctx: egui::Context) {
        if let Err(error) = validate_pairing_code(&self.pairing_code) {
            self.pairing_last_error = Some(error.to_string());
            return;
        }

        self.submit_pairing_code(egui_ctx);
    }

    pub(super) fn render_pairing_dialog(&mut self, ctx: &egui::Context) {
        if self.pairing_in_progress && self.pairing_challenge.is_none() {
            let title = self
                .pairing_flow
                .as_ref()
                .map(|flow| flow.dialog_title.as_str())
                .unwrap_or("Pairing...");
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .min_width(360.0)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Requesting pairing challenge from target...");
                    });
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        self.cancel_pairing_flow();
                    }
                });
        } else if self.pairing_flow.is_some()
            && self.pairing_challenge.is_none()
            && self.pairing_last_error.is_some()
        {
            let title = self
                .pairing_flow
                .as_ref()
                .map(|flow| flow.dialog_title.as_str())
                .unwrap_or("Pairing failed");
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .min_width(420.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    self.render_pairing_error(ui, ctx);
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() {
                            self.cancel_pairing_flow();
                        }
                    });
                });
        } else if let Some(challenge) = self.pairing_challenge.clone() {
            let title = self
                .pairing_flow
                .as_ref()
                .map(|flow| flow.dialog_title.as_str())
                .unwrap_or("Enter Pairing Code");
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .min_width(360.0)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(format!(
                        "Request ID: {}",
                        short_token(&challenge.request_id)
                    ));
                    ui.label(format!(
                        "Expires at: {}",
                        format_timestamp(&challenge.expires_at)
                    ));
                    ui.add_space(8.0);

                    self.render_pairing_error(ui, ctx);

                    ui.horizontal(|ui| {
                        ui.label("Code:");
                        ui.text_edit_singleline(&mut self.pairing_code);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Alias (optional):");
                        ui.text_edit_singleline(&mut self.pairing_alias);
                    });

                    ui.add_space(8.0);
                    if self.pairing_in_progress {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            if self.pairing_role_reversal_available {
                                ui.label("Waiting for reverse pairing approval...");
                            } else {
                                ui.label("Verifying code...");
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            if ui.button("Confirm").clicked() {
                                self.confirm_pairing_code(ctx.clone());
                            }
                            if ui.button("Cancel").clicked() {
                                self.cancel_pairing_flow();
                            }
                        });
                    }
                });
        }
    }

    pub(super) fn render_pairing_error(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(err) = self.pairing_last_error.clone() else {
            return;
        };

        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(err)
                        .color(egui::Color32::LIGHT_RED)
                        .size(12.0),
                ));
                if self.pairing_retry_available
                    && !self.pairing_in_progress
                    && let Some(flow) = self.pairing_flow.clone()
                    && ui.button("Retry Pairing Request").clicked()
                {
                    self.start_pairing(flow, ctx.clone());
                }
                if self.pairing_role_reversal_available && !self.pairing_in_progress {
                    if let Some(message) = self.pairing_role_reversal_message.clone() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(message)
                                .color(egui::Color32::LIGHT_YELLOW)
                                .size(12.0),
                        );
                    }
                    if self.pairing_challenge.is_some()
                        && ui.button("Start Reverse Pairing Request").clicked()
                    {
                        self.start_role_reversal_pairing(ctx.clone());
                    }
                }
                if ui.small_button("Dismiss").clicked() {
                    self.pairing_last_error = None;
                    self.pairing_retry_available = false;
                    self.pairing_role_reversal_message = None;
                }
            });
        });
        ui.add_space(8.0);
    }

    pub(super) fn render_pairing_setup(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(
            "Open Boundless on both PCs. Choose the other PC, then enter the code displayed there.",
        );
        ui.add_space(8.0);
        if self.snapshot.discovered_peers.is_empty() {
            ui.label("No nearby PCs found yet. You can also connect by name or IP address.");
        } else {
            for peer in self.snapshot.discovered_peers.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(&peer.display_name);
                    if ui
                        .button("Connect")
                        .on_hover_text(format!(
                            "PC identity: {}\nAddress: {}",
                            peer.machine_id, peer.endpoint
                        ))
                        .clicked()
                    {
                        match guided_flow_from_discovered_peer(&peer) {
                            Ok(flow) => self.start_pairing(flow, ctx.clone()),
                            Err(error) => self.push_toast(error.to_string(), true),
                        }
                    }
                });
            }
        }
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Connect by address")
            .default_open(self.snapshot.discovered_peers.is_empty())
            .show(ui, |ui| {
                ui.label("PC name or IP address");
                ui.add(
                    egui::TextEdit::singleline(&mut self.manual_host)
                        .desired_width(ui.available_width().min(360.0)),
                );
                ui.collapsing("Port", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.manual_port).desired_width(90.0));
                    ui.label("Use 15200 unless you changed the pairing port on the other PC.");
                });
                let port_valid = self
                    .manual_port
                    .trim()
                    .parse::<u16>()
                    .is_ok_and(|port| port > 0);
                if !port_valid {
                    ui.label("Enter a port from 1 to 65535.");
                }
                if ui
                    .add_enabled(
                        !self.manual_host.trim().is_empty() && port_valid,
                        egui::Button::new("Connect by address"),
                    )
                    .clicked()
                {
                    match guided_flow_from_manual_input(&self.manual_host, &self.manual_port) {
                        Ok(flow) => self.start_pairing(flow, ctx.clone()),
                        Err(error) => self.push_toast(error.to_string(), true),
                    }
                }
            });
        if !self.snapshot.pending_requests.is_empty() {
            ui.strong("Pairing requests");

            for req in &self.snapshot.pending_requests {
                ui.group(|ui| {
                    ui.label(format!("From: {}", req.requester_display_name));
                    ui.label(format!(
                        "Requester: {}",
                        short_token(&req.requester_machine_id)
                    ));
                    ui.label(format!("ID: {}", short_token(&req.request_id)));
                    if req.requires_verification_code {
                        if req.verification_code.trim().is_empty() {
                            ui.label("Code: hidden on this endpoint");
                            ui.label("Expires: hidden on this endpoint");
                        } else {
                            ui.label(format!("Code: {}", req.verification_code));
                            ui.label(format!("Expires: {}", req.verification_expires_at));
                        }
                    }
                    ui.horizontal(|ui| {
                        if !req.requires_verification_code && ui.button("Approve").clicked() {
                            self.task_runner().approve_request(
                                self.tx.clone(),
                                self.ctx.endpoint.clone(),
                                req.request_id.clone(),
                            );
                        }
                        if ui
                            .button(if req.requires_verification_code {
                                "Cancel"
                            } else {
                                "Reject"
                            })
                            .clicked()
                        {
                            self.task_runner().reject_request(
                                self.tx.clone(),
                                self.ctx.endpoint.clone(),
                                req.request_id.clone(),
                            );
                        }
                    });
                });
            }
        }
    }

    pub(super) fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Sharing");
            ui.add_space(4.0);
            ui.label("Choose what you share with your trusted PCs.");
            for (name, label) in [
                ("share_input", "Share keyboard and mouse"),
                ("share_clipboard", "Share clipboard"),
                ("transfer_file", "Share files"),
                ("easy_mouse", "Switch at screen edge"),
                ("wrap_mouse", "Wrap across layout edges"),
            ] {
                let enabled = self.snapshot.features.get(name).copied().unwrap_or(false)
                    && (name != "share_input" || !self.input_pause_requested);
                let mut next = enabled;
                let response = ui.add_enabled(name != "share_input" || !self.input_change_pending, egui::Checkbox::new(&mut next, label));
                if response.clicked() && name == "share_input" {
                    if enabled {
                        self.pause_input();
                    } else {
                        self.resume_input();
                    }
                } else if response.clicked() {
                    self.task_runner().set_feature(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        name.to_string(),
                        !enabled,
                    );
                }
            }

            ui.add_space(16.0);
            ui.separator();

            // ── File Transfer ──────────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("File Transfer");
            ui.add_space(4.0);
            let file_transfer_config = self.snapshot.file_transfer_config.clone();
            ui.label(egui::RichText::new("Received files are saved to this folder.").weak());
            ui.horizontal(|ui| {
                ui.label("Receive folder:");
                ui.text_edit_singleline(&mut self.file_receive_dir_edit);
            });
            let mut organize_by_peer = file_transfer_config.organize_by_peer;
            if ui
                .checkbox(&mut organize_by_peer, "Organize received files by sender")
                .clicked()
            {
                self.task_runner().set_file_transfer_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    file_transfer_config.receive_dir.clone(),
                    !file_transfer_config.organize_by_peer,
                    file_transfer_config.auto_accept_trusted_peers,
                    file_transfer_config.max_file_bytes,
                );
            }
            let mut auto_accept_trusted = file_transfer_config.auto_accept_trusted_peers;
            if ui
                .checkbox(&mut auto_accept_trusted, "Auto-accept files from trusted peers")
                .clicked()
            {
                self.task_runner().set_file_transfer_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    file_transfer_config.receive_dir.clone(),
                    file_transfer_config.organize_by_peer,
                    !file_transfer_config.auto_accept_trusted_peers,
                    file_transfer_config.max_file_bytes,
                );
            }
            ui.label(egui::RichText::new(format!(
                "Limit: {} MB",
                file_transfer_config.max_file_bytes / (1024 * 1024)
            )).weak());
            ui.horizontal(|ui| {
                let receive_dir_changed =
                    self.file_receive_dir_edit != file_transfer_config.receive_dir;
                if ui
                    .add_enabled(receive_dir_changed, egui::Button::new("Save Folder"))
                    .on_hover_text("Persist this receive folder for future incoming files")
                    .clicked()
                {
                    self.task_runner().set_file_transfer_config(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        self.file_receive_dir_edit.clone(),
                        file_transfer_config.organize_by_peer,
                        file_transfer_config.auto_accept_trusted_peers,
                        file_transfer_config.max_file_bytes,
                    );
                }
                if ui
                    .button("Open Folder")
                    .on_hover_text("Open the current receive folder in Explorer")
                    .clicked()
                {
                    self.task_runner().open_receive_folder(
                        self.tx.clone(),
                        file_transfer_config.receive_dir.clone(),
                    );
                }
            });

            ui.add_space(16.0);
            ui.separator();

            // ── Input Handoff ─────────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("Keyboard and mouse");
            ui.add_space(4.0);
            let handoff = self.snapshot.input_handoff_config.clone();
            let runtime = self.snapshot.input_runtime.clone();
            ui.collapsing("Input details and administrator apps", |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "Capture: {}  |  Backend: {}  |  Queue: {}/{}",
                    if runtime.lock_active {
                        "active"
                    } else {
                        "idle"
                    },
                    if runtime.capture_backend_mode.is_empty() {
                        "unknown"
                    } else {
                        &runtime.capture_backend_mode
                    },
                    runtime.pending_inject_frames,
                    runtime.pending_inject_high_water,
                ))
                .weak(),
            );
            let input_health = input_sharing_health(&runtime.capture_backend_mode);
            let clipboard_health =
                clipboard_sharing_health(&self.snapshot.clipboard_runtime.backend_mode);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Sharing health:").strong());
                render_sharing_health(ui, "Input", input_health);
                ui.label("|");
                render_sharing_health(ui, "Clipboard", clipboard_health);
            });
            if !runtime.owner_peer_id.is_empty()
                || !runtime.configured_capture_target_peer_id.is_empty()
                || !runtime.active_capture_target_peer_id.is_empty()
            {
                ui.label(
                    egui::RichText::new(format!(
                        "Owner: {}  |  Configured target: {}  |  Active target: {}",
                        empty_as_none(&runtime.owner_peer_id),
                        empty_as_none(&runtime.configured_capture_target_peer_id),
                        empty_as_none(&runtime.active_capture_target_peer_id),
                    ))
                    .weak()
                    .size(12.0),
                );
            }
            if runtime.capture_backend_mode == "service_session_unsupported" {
                ui.label(
                    egui::RichText::new(
                        "Service session cannot capture or inject desktop input; \
                         waiting for the tray user-session input broker to attach.",
                    )
                    .weak()
                    .italics(),
                );
            } else if runtime.capture_backend_mode == "user_session_broker" {
                let elevated_status = self
                    .elevated_input_controller
                    .as_ref()
                    .map(ElevatedInputController::status)
                    .unwrap_or_default();
                let elevated_active = matches!(
                    elevated_status.state,
                    platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle
                        | platform_windows::elevated_input::InputInjectorState::Active
                );
                ui.label(
                    egui::RichText::new(
                        if elevated_active {
                            "Input runs through the tray broker with administrator-app control enabled. Lock screen and the UAC consent screen remain unavailable."
                        } else {
                            "Input runs through the tray user-session broker. Administrator-launched app windows require the explicit control below; lock screen and the UAC consent screen remain unavailable."
                        },
                    )
                    .weak()
                    .italics(),
                );
            }
            let elevated_status = self
                .elevated_input_controller
                .as_ref()
                .map(ElevatedInputController::status)
                .unwrap_or_default();
            let (elevated_state, elevated_reason, elevated_trust) =
                elevated_status.telemetry_fields();
            let elevated_backend_supported = matches!(
                runtime.capture_backend_mode.as_str(),
                "service_session_unsupported" | "user_session_broker"
            );
            ui.add_space(8.0);
            ui.group(|ui| {
                ui.label(egui::RichText::new("Administrator app control").strong());
                ui.label(
                    egui::RichText::new(format!(
                        "State: {}  |  Reason: {}  |  Trust: {}{}",
                        elevated_state.replace('_', " "),
                        elevated_reason.replace('_', " "),
                        elevated_trust.replace('_', " "),
                        if elevated_status.helper_version.is_empty() {
                            String::new()
                        } else {
                            format!("  |  Helper: {}", elevated_status.helper_version)
                        }
                    ))
                    .weak(),
                );
                if !elevated_backend_supported {
                    ui.label(
                        egui::RichText::new(
                            "Available only with the installed service and tray input broker; the direct daemon path does not use this helper.",
                        )
                        .color(egui::Color32::LIGHT_YELLOW),
                    );
                }
                ui.label(
                    egui::RichText::new(
                        "Enable only when you need to control an administrator-launched app. Windows asks each time you enable it for a tray session; the current dogfood build is unsigned, so the prompt identifies an unknown publisher.",
                    )
                    .weak()
                    .size(12.0),
                );
                if elevated_status.signature_trust
                    == platform_windows::elevated_input::InputInjectorSignatureTrust::UnsignedDogfood
                {
                    ui.label(
                        egui::RichText::new("Unsigned dogfood helper active")
                            .color(egui::Color32::LIGHT_YELLOW),
                    );
                }

                let can_enable = elevated_backend_supported
                    && elevated_status.direct_fallback_safe
                    && matches!(
                        elevated_status.state,
                        platform_windows::elevated_input::InputInjectorState::Off
                            | platform_windows::elevated_input::InputInjectorState::Unavailable
                    );
                let can_disable = matches!(
                    elevated_status.state,
                    platform_windows::elevated_input::InputInjectorState::Prompting
                        | platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle
                        | platform_windows::elevated_input::InputInjectorState::Active
                );
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            can_enable,
                            egui::Button::new("Enable administrator-app control"),
                        )
                        .clicked()
                    {
                        let started = self
                            .elevated_input_controller
                            .as_ref()
                            .is_some_and(ElevatedInputController::request_enable);
                        if started {
                            self.push_toast(
                                "Approve the Windows permission prompt on this PC".to_string(),
                                false,
                            );
                        } else {
                            self.push_toast(
                                "Administrator-app control could not start; check its status and restart the tray if shutdown is incomplete".to_string(),
                                true,
                            );
                        }
                    }
                    if ui
                        .add_enabled(
                            can_disable,
                            egui::Button::new("Disable administrator-app control"),
                        )
                        .clicked()
                    {
                        let requested = self
                            .elevated_input_controller
                            .as_ref()
                            .is_some_and(ElevatedInputController::request_disable);
                        if requested {
                            self.push_toast(
                                "Administrator-app control is stopping and releasing held input"
                                    .to_string(),
                                false,
                            );
                        }
                    }
                });
                if !elevated_status.direct_fallback_safe {
                    ui.label(
                        egui::RichText::new(
                            "Direct input fallback is blocked until elevated cleanup is confirmed. Quit and relaunch Boundless if this state persists.",
                        )
                        .color(egui::Color32::LIGHT_RED),
                    );
                }
            });
            if !runtime.lock_supported {
                ui.label(
                    egui::RichText::new("Input locking is unavailable on this platform")
                        .weak()
                        .italics(),
                );
            }
            });
            ui.collapsing("Pointer behavior", |ui| {
            let mut block_screen_corners = handoff.block_screen_corners;
            if ui
                .checkbox(&mut block_screen_corners, "Block screen corners")
                .clicked()
            {
                self.task_runner().set_input_handoff_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    UiInputHandoffConfig {
                        block_screen_corners: !handoff.block_screen_corners,
                        ..handoff.clone()
                    },
                );
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("Corner block:");
                for px in [8_u32, 16, 24, 32, 48] {
                    if ui
                        .selectable_label(handoff.corner_block_px == px, format!("{px}px"))
                        .clicked()
                    {
                        self.task_runner().set_input_handoff_config(
                            self.tx.clone(),
                            self.ctx.endpoint.clone(),
                            UiInputHandoffConfig {
                                corner_block_px: px,
                                ..handoff.clone()
                            },
                        );
                    }
                }
            });
            let mut relative_mouse = handoff.relative_mouse;
            if ui
                .checkbox(&mut relative_mouse, "Use relative mouse movement")
                .clicked()
            {
                self.task_runner().set_input_handoff_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    UiInputHandoffConfig {
                        relative_mouse: !handoff.relative_mouse,
                        ..handoff.clone()
                    },
                );
            }
            let mut hide_cursor_at_edge = handoff.hide_cursor_at_edge;
            if ui
                .checkbox(&mut hide_cursor_at_edge, "Hide cursor at edge")
                .clicked()
            {
                self.task_runner().set_input_handoff_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    UiInputHandoffConfig {
                        hide_cursor_at_edge: !handoff.hide_cursor_at_edge,
                        ..handoff.clone()
                    },
                );
            }
            let mut draw_cursor_marker = handoff.draw_cursor_marker;
            if ui
                .checkbox(&mut draw_cursor_marker, "Draw cursor marker")
                .clicked()
            {
                self.task_runner().set_input_handoff_config(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    UiInputHandoffConfig {
                        draw_cursor_marker: !handoff.draw_cursor_marker,
                        ..handoff.clone()
                    },
                );
            }

            ui.add_space(16.0);
            ui.separator();

            // ── Peer Availability ─────────────────────────────────────
            ui.add_space(8.0);
            });
            ui.collapsing("Keep PCs awake", |ui| {
            ui.add_space(4.0);
            let anti_idle_config = self.snapshot.anti_idle_config.clone();
            let anti_idle_status = self.snapshot.anti_idle_status.clone();
            let anti_idle_status_text = if !anti_idle_status.supported {
                "Unsupported on this platform"
            } else if anti_idle_status.active {
                "Active now"
            } else {
                "Inactive"
            };
            ui.label(
                egui::RichText::new(format!(
                    "Status: {}{}",
                    anti_idle_status_text,
                    if anti_idle_status.reason == "none" {
                        if anti_idle_status.supported {
                            format!(
                                " (enabled={} display_required={})",
                                anti_idle_status.enabled, anti_idle_status.display_required
                            )
                        } else {
                            String::new()
                        }
                    } else {
                        format!(
                            " ({}; enabled={} display_required={})",
                            anti_idle_status.reason.replace('_', " "),
                            anti_idle_status.enabled,
                            anti_idle_status.display_required
                        )
                    }
                ))
                .weak(),
            );
            if anti_idle_status.supported {
                let mut anti_idle_enabled = anti_idle_config.enabled;
                if ui
                    .checkbox(
                        &mut anti_idle_enabled,
                        "Keep connected peers awake",
                    )
                    .clicked()
                {
                    self.task_runner().set_anti_idle_config(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        !anti_idle_config.enabled,
                        anti_idle_config.recent_activity_window_secs,
                        anti_idle_config.allow_on_battery,
                        anti_idle_config.keep_display_on,
                    );
                }

                ui.add_space(8.0);
                ui.label("Recent activity window");
                ui.horizontal_wrapped(|ui| {
                    for minutes in [1_u32, 5, 10, 15, 30] {
                        let selected = anti_idle_config.recent_activity_window_secs == minutes * 60;
                        if ui.selectable_label(selected, format!("{minutes} min")).clicked() {
                            self.task_runner().set_anti_idle_config(
                                self.tx.clone(),
                                self.ctx.endpoint.clone(),
                                anti_idle_config.enabled,
                                minutes * 60,
                                anti_idle_config.allow_on_battery,
                                anti_idle_config.keep_display_on,
                            );
                        }
                    }
                });

                let mut allow_on_battery = anti_idle_config.allow_on_battery;
                if ui
                    .checkbox(
                        &mut allow_on_battery,
                        "Allow on battery",
                    )
                    .clicked()
                {
                    self.task_runner().set_anti_idle_config(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        anti_idle_config.enabled,
                        anti_idle_config.recent_activity_window_secs,
                        !anti_idle_config.allow_on_battery,
                        anti_idle_config.keep_display_on,
                    );
                }
                let mut keep_display_on = anti_idle_config.keep_display_on;
                if ui
                    .checkbox(
                        &mut keep_display_on,
                        "Keep display on",
                    )
                    .clicked()
                {
                    self.task_runner().set_anti_idle_config(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        anti_idle_config.enabled,
                        anti_idle_config.recent_activity_window_secs,
                        anti_idle_config.allow_on_battery,
                        !anti_idle_config.keep_display_on,
                    );
                }
            }

            ui.add_space(16.0);
            ui.separator();

            // ── Hotkeys ───────────────────────────────────────────────
            ui.add_space(8.0);
            });
            ui.collapsing("Keyboard shortcuts", |ui| {
            ui.add_space(4.0);
            for (action, label) in [
                ("toggle_easy_mouse", "Toggle edge switching"),
                ("lock_machine", "Lock machines"),
                ("switch_all", "Switch target"),
                ("reconnect", "Reconnect peers"),
            ] {
                let current = self.snapshot.hotkeys.get(action).cloned().unwrap_or_default();
                let mut edit = self
                    .hotkey_edits
                    .get(action)
                    .cloned()
                    .unwrap_or_else(|| current.clone());
                let mut save_clicked = false;
                ui.horizontal(|ui| {
                    ui.label(label);
                    ui.text_edit_singleline(&mut edit);
                    let changed = current != edit;
                    save_clicked = ui
                        .add_enabled(changed, egui::Button::new("Save"))
                        .on_hover_text("Persist this hotkey combo")
                        .clicked();
                });
                self.hotkey_edits.insert(action.to_string(), edit.clone());
                if save_clicked {
                    self.task_runner().set_hotkey(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        action.to_string(),
                        edit,
                    );
                }
            }

            ui.add_space(16.0);
            ui.separator();

            });
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharingHealth {
    Healthy,
    Degraded,
    Unknown,
}

fn input_sharing_health(backend_mode: &str) -> SharingHealth {
    match backend_mode {
        "windows_hooks" | "user_session_broker" | "direct" => SharingHealth::Healthy,
        "service_session_unsupported" => SharingHealth::Degraded,
        _ => SharingHealth::Unknown,
    }
}

fn clipboard_sharing_health(backend_mode: &str) -> SharingHealth {
    match backend_mode {
        "user_session_broker" | "direct" => SharingHealth::Healthy,
        "broker_unavailable" => SharingHealth::Degraded,
        _ => SharingHealth::Unknown,
    }
}

fn render_sharing_health(ui: &mut egui::Ui, label: &str, health: SharingHealth) {
    let (status, color) = match health {
        SharingHealth::Healthy => ("available", egui::Color32::LIGHT_GREEN),
        SharingHealth::Degraded => ("degraded", egui::Color32::LIGHT_RED),
        SharingHealth::Unknown => ("unknown", egui::Color32::GRAY),
    };
    ui.label(egui::RichText::new(format!("{label} backend {status}")).color(color));
}

#[cfg(test)]
mod sharing_health_tests {
    use super::*;

    #[test]
    fn sharing_health_distinguishes_healthy_degraded_and_unknown_backends() {
        assert_eq!(
            input_sharing_health("user_session_broker"),
            SharingHealth::Healthy
        );
        assert_eq!(
            input_sharing_health("service_session_unsupported"),
            SharingHealth::Degraded
        );
        assert_eq!(
            clipboard_sharing_health("broker_unavailable"),
            SharingHealth::Degraded
        );
        assert_eq!(clipboard_sharing_health(""), SharingHealth::Unknown);
    }
}
