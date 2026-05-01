use super::*;

impl DashboardApp {
    pub(super) fn task_runner(&self) -> DashboardTaskRunner {
        DashboardTaskRunner::new()
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
            self.task_runner().submit_pairing_code(SubmitPairingCodeTask {
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
                    ui.label(format!("Request ID: {}", short_token(&challenge.request_id)));
                    ui.label(format!("Expires at: {}", format_timestamp(&challenge.expires_at)));
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
                            ui.label("Verifying code...");
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
                if ui.small_button("Dismiss").clicked() {
                    self.pairing_last_error = None;
                    self.pairing_retry_available = false;
                }
            });
        });
        ui.add_space(8.0);
    }

    pub(super) fn render_status_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if should_offer_first_run_onboarding(&self.snapshot) {
                ui.group(|ui| {
                    ui.heading("Get Started");
                    ui.label("Boundless is ready for first-run setup on this machine.");
                    if self.snapshot.discovered_peers.is_empty() {
                        ui.label("Waiting for a peer on the local network. If discovery stays empty, use Manual Setup with the other machine's host/IP.");
                    } else {
                        ui.label("Choose a discovered peer below to begin guided pairing.");
                    }
                    ui.label("Next steps: pair with the other machine, approve the verification code there, then arrange the layout in Layout Manager.");
                });
                ui.add_space(16.0);
            }

            ui.heading("Discovered Peers");
            if self.snapshot.discovered_peers.is_empty() {
                ui.label(egui::RichText::new("No peers discovered on local network.").italics());
            } else {
                egui::Grid::new("discovered_peers")
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Name").strong());
                        ui.label(egui::RichText::new("ID").strong());
                        ui.label(egui::RichText::new("Address").strong());
                        ui.label("");
                        ui.end_row();
                        for peer in self.snapshot.discovered_peers.clone() {
                            ui.label(&peer.display_name);
                            ui.label(short_token(&peer.machine_id));
                            ui.label(&peer.endpoint);
                            let btn = ui.button("Connect");
                            if btn.clicked() {
                                match guided_flow_from_discovered_peer(&peer) {
                                    Ok(flow) => self.start_pairing(flow, ctx.clone()),
                                    Err(error) => {
                                        self.push_toast(error.to_string(), true);
                                    }
                                }
                            }
                            btn.on_hover_text("Start guided pairing with this peer");
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(16.0);
            ui.heading("Manual Setup");
            ui.horizontal(|ui| {
                ui.label("Host/IP:");
                let host_response = ui.text_edit_singleline(&mut self.manual_host);
                // Inline validation: red border on empty host when focused
                if self.manual_host.trim().is_empty() && host_response.lost_focus() {
                    ui.label(
                        egui::RichText::new("Required")
                            .color(egui::Color32::from_rgb(255, 120, 100))
                            .size(11.0),
                    );
                }

                ui.label("Port:");
                let port_response = ui.text_edit_singleline(&mut self.manual_port);
                // Inline validation: show error for non-numeric port
                let port_valid = self.manual_port.trim().parse::<u16>().is_ok_and(|p| p > 0);
                if !port_valid
                    && !self.manual_port.is_empty()
                    && port_response.lost_focus()
                {
                    ui.label(
                        egui::RichText::new("Invalid port")
                            .color(egui::Color32::from_rgb(255, 120, 100))
                            .size(11.0),
                    );
                }

                let connect_enabled =
                    !self.manual_host.trim().is_empty() && port_valid;
                let btn = ui.add_enabled(connect_enabled, egui::Button::new("Connect"));
                if btn.clicked()
                    && let Ok(flow) =
                        guided_flow_from_manual_input(&self.manual_host, &self.manual_port)
                {
                    self.start_pairing(flow, ctx.clone());
                }
                btn.on_hover_text("Start pairing with this host and port");
            });

            ui.add_space(16.0);
            ui.heading("Paired Peers");
            if self.snapshot.paired_peers.is_empty() {
                ui.label(egui::RichText::new("No paired peers.").italics());
            } else {
                egui::Grid::new("paired_peers").striped(true).show(ui, |ui| {
                    ui.label(egui::RichText::new("Name").strong());
                    ui.label(egui::RichText::new("ID").strong());
                    ui.label(egui::RichText::new("Address").strong());
                    ui.label(egui::RichText::new("Status").strong());
                    ui.end_row();
                    for peer in &self.snapshot.paired_peers {
                        let color = if peer.connected {
                            egui::Color32::LIGHT_GREEN
                        } else {
                            egui::Color32::DARK_GRAY
                        };
                        ui.label(egui::RichText::new(&peer.display_name).color(color));
                        ui.label(short_token(&peer.peer_id));
                        ui.label(&peer.address);
                        ui.label(if peer.connected { "Connected" } else { "Offline" });
                        ui.end_row();
                    }
                });
            }

            ui.add_space(16.0);
            ui.heading("Pending Requests");
            if self.snapshot.pending_requests.is_empty() {
                ui.label(egui::RichText::new("No pending requests.").italics());
            } else {
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
        });
    }

    pub(super) fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            // ── Identity ───────────────────────────────────────────────
            ui.heading("Identity");
            ui.add_space(4.0);
            egui::Grid::new("settings_identity")
                .num_columns(3)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Machine ID:").strong());
                    let id_short = short_token(&self.snapshot.machine_id);
                    ui.label(id_short)
                        .on_hover_text(&self.snapshot.machine_id);
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy full machine ID to clipboard")
                        .clicked()
                    {
                        ui.ctx().copy_text(self.snapshot.machine_id.clone());
                        self.push_toast("Machine ID copied to clipboard".to_string(), false);
                    }
                    ui.end_row();

                    ui.label(egui::RichText::new("PC Name:").strong());
                    let hostname = std::env::var("COMPUTERNAME")
                        .unwrap_or_else(|_| "Unknown".to_string());
                    ui.label(&hostname);
                    ui.label(""); // empty column
                    ui.end_row();
                });

            ui.add_space(16.0);
            ui.separator();

            // ── Daemon ─────────────────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("Daemon");
            ui.add_space(4.0);
            egui::Grid::new("settings_daemon")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Status:").strong());
                    if self.snapshot.daemon_online {
                        ui.label(
                            egui::RichText::new("Online")
                                .color(egui::Color32::LIGHT_GREEN),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("Offline")
                                .color(egui::Color32::LIGHT_RED),
                        );
                    }
                    ui.end_row();

                    ui.label(egui::RichText::new("API Endpoint:").strong());
                    ui.label(&self.ctx.endpoint);
                    ui.end_row();

                    ui.label(egui::RichText::new("Last Snapshot:").strong());
                    if self.snapshot.generated_at.is_empty() {
                        ui.label(
                            egui::RichText::new("No data yet").weak().italics(),
                        );
                    } else {
                        ui.label(format_timestamp(&self.snapshot.generated_at));
                    }
                    ui.end_row();
                });

            ui.add_space(16.0);
            ui.separator();

            // ── Product Controls ──────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("Product Controls");
            ui.add_space(4.0);
            ui.label(egui::RichText::new("These match the effective daemon feature flags.").weak());
            for (name, label, reason) in [
                ("share_input", "Share keyboard and mouse", None),
                ("share_clipboard", "Share clipboard", None),
                ("transfer_file", "Transfer files", None),
                ("easy_mouse", "Switch at screen edge", None),
                ("wrap_mouse", "Wrap across layout edges", None),
                (
                    "same_subnet_only",
                    "Same subnet only",
                    Some("Network policy enforcement is not implemented yet"),
                ),
                (
                    "validate_remote_ip",
                    "Validate remote IP",
                    Some("Reverse-DNS warning/enforcement is not implemented yet"),
                ),
            ] {
                let enabled = if reason.is_some() {
                    false
                } else {
                    self.snapshot.features.get(name).copied().unwrap_or(false)
                };
                let mut next = enabled;
                let response = ui.add_enabled(
                    reason.is_none(),
                    egui::Checkbox::new(&mut next, label),
                );
                if let Some(reason) = reason {
                    response.on_hover_text(reason);
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
                    self.file_receive_dir_edit.clone(),
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
                    self.file_receive_dir_edit.clone(),
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
            ui.heading("Input Handoff");
            ui.add_space(4.0);
            let handoff = self.snapshot.input_handoff_config.clone();
            let runtime = self.snapshot.input_runtime.clone();
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
            if !runtime.lock_supported {
                ui.label(
                    egui::RichText::new("Input locking is unavailable on this platform")
                        .weak()
                        .italics(),
                );
            }
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
            ui.heading("Peer Availability");
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
            ui.heading("Hotkeys");
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

            // ── Actions ────────────────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("Actions");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .button("Reconnect All Peers")
                    .on_hover_text(
                        "Trigger the daemon to reconnect all paired peers",
                    )
                    .clicked()
                {
                    self.task_runner()
                        .reconnect_all_peers(self.tx.clone(), self.ctx.endpoint.clone());
                }
                let network_reset_label = if self.confirm_network_reset_pending {
                    "Confirm Network Reset"
                } else {
                    "Reset Network"
                };
                if ui.button(network_reset_label).on_hover_text(
                    "Clear paired peers and runtime network state without deleting local identity",
                ).clicked() {
                    if !self.confirm_network_reset_pending {
                        self.confirm_network_reset_pending = true;
                        self.push_toast(
                            "Click Confirm Network Reset to clear peer/network state".to_string(),
                            false,
                        );
                        return;
                    }
                    self.confirm_network_reset_pending = false;
                    self.task_runner().safe_reset(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        true,
                        false,
                        format!("safe-reset-network:{}", self.snapshot.machine_id),
                    );
                }
                let safe_reset_label = if self.confirm_safe_reset_pending {
                    "Confirm Safe Reset"
                } else {
                    "Safe Reset"
                };
                if ui.button(safe_reset_label).on_hover_text(
                    "Reset daemon config/runtime state while preserving installed app files and local identity",
                ).clicked() {
                    if !self.confirm_safe_reset_pending {
                        self.confirm_safe_reset_pending = true;
                        self.push_toast(
                            "Click Confirm Safe Reset to reset daemon config/runtime state"
                                .to_string(),
                            false,
                        );
                        return;
                    }
                    self.confirm_safe_reset_pending = false;
                    self.task_runner().safe_reset(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                        false,
                        true,
                        format!("safe-reset-all:{}", self.snapshot.machine_id),
                    );
                }
            });
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "Service mode actions remain guarded until Windows IPC ACL validation is complete.",
                )
                .weak()
                .size(12.0),
            );

            ui.add_space(16.0);
            ui.separator();

            // ── Diagnostics ────────────────────────────────────────────
            ui.add_space(8.0);
            ui.heading("Diagnostics");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "Paired: {}  |  Discovered: {}  |  Pending: {}",
                    self.snapshot.paired_peers.len(),
                    self.snapshot.discovered_peers.len(),
                    self.snapshot.pending_requests.len(),
                ))
                .weak()
                .size(12.0),
            );
            ui.label(
                egui::RichText::new(format!(
                    "Layout matrix: {}",
                    if self.snapshot.layout_matrix.is_empty() {
                        "(none)"
                    } else {
                        &self.snapshot.layout_matrix
                    }
                ))
                .weak()
                .size(12.0),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!("Boundless v{}", env!("CARGO_PKG_VERSION")))
                    .weak()
                    .size(11.0),
            );
        });
    }
}
