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
            self.last_error = Some(error.to_string());
            self.last_message_is_error = true;
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
            egui::Window::new(title).collapsible(false).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Requesting pairing challenge from target...");
                });
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
            egui::Window::new(title).collapsible(false).show(ctx, |ui| {
                ui.label(format!("Request ID: {}", short_token(&challenge.request_id)));
                ui.label(format!("Expires at: {}", challenge.expires_at));
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label("Code:");
                    ui.text_edit_singleline(&mut self.pairing_code);
                });
                ui.horizontal(|ui| {
                    ui.label("Alias (optional):");
                    ui.text_edit_singleline(&mut self.pairing_alias);
                });

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
                        for peer in self.snapshot.discovered_peers.clone() {
                            ui.label(&peer.display_name);
                            ui.label(short_token(&peer.machine_id));
                            ui.label(&peer.endpoint);
                            if ui.button("Connect").clicked() {
                                match guided_flow_from_discovered_peer(&peer) {
                                    Ok(flow) => self.start_pairing(flow, ctx.clone()),
                                    Err(error) => {
                                        self.last_error = Some(error.to_string());
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
            }

            ui.add_space(16.0);
            ui.heading("Manual Setup");
            ui.horizontal(|ui| {
                ui.label("Host/IP:");
                ui.text_edit_singleline(&mut self.manual_host);
                ui.label("Port:");
                ui.text_edit_singleline(&mut self.manual_port);
                if ui.button("Connect").clicked()
                    && let Ok(flow) =
                        guided_flow_from_manual_input(&self.manual_host, &self.manual_port)
                {
                    self.start_pairing(flow, ctx.clone());
                }
            });

            ui.add_space(16.0);
            ui.heading("Paired Peers");
            if self.snapshot.paired_peers.is_empty() {
                ui.label(egui::RichText::new("No paired peers.").italics());
            } else {
                egui::Grid::new("paired_peers").striped(true).show(ui, |ui| {
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
        ui.heading("Settings & Diagnostics");
        ui.label(format!("Machine ID: {}", self.snapshot.machine_id));
        ui.label(format!(
            "Daemon Status: {}",
            if self.snapshot.daemon_online {
                "Online"
            } else {
                "Offline"
            }
        ));
        ui.label(format!("API Endpoint: {}", self.ctx.endpoint));
        ui.label(format!("Snapshot Timestamp: {}", self.snapshot.generated_at));

        ui.add_space(16.0);
        if ui.button("Reconnect All Peers").clicked() {
            self.task_runner()
                .reconnect_all_peers(self.tx.clone(), self.ctx.endpoint.clone());
        }
    }
}
