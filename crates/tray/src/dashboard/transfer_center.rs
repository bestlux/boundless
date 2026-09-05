use super::*;

impl UiFileTransfer {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "completed" | "failed" | "cancelled")
    }

    pub(super) fn can_cancel(&self) -> bool {
        self.direction == "outgoing" && matches!(self.state.as_str(), "queued" | "active")
    }

    pub(super) fn can_retry(&self) -> bool {
        self.direction == "outgoing"
            && self.is_terminal()
            && matches!(self.state.as_str(), "failed" | "cancelled")
            && !self.source_path.trim().is_empty()
    }

    pub(super) fn can_open_location(&self) -> bool {
        self.direction == "incoming"
            && self.state == "completed"
            && !self.final_path.trim().is_empty()
    }

    pub(super) fn progress_fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            if self.state == "completed" { 1.0 } else { 0.0 }
        } else {
            (self.transferred_bytes as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
        }
    }
}

impl DashboardApp {
    fn render_file_send(&mut self, ui: &mut egui::Ui) {
        let sharing_enabled = self
            .snapshot
            .features
            .get("transfer_file")
            .copied()
            .unwrap_or(false);
        let connected = self
            .snapshot
            .paired_peers
            .iter()
            .filter(|peer| peer.connected)
            .cloned()
            .collect::<Vec<_>>();
        if !connected
            .iter()
            .any(|peer| peer.peer_id == self.file_send_peer)
        {
            self.file_send_peer = connected
                .first()
                .map(|peer| peer.peer_id.clone())
                .unwrap_or_default();
        }
        let name = connected
            .iter()
            .find(|peer| peer.peer_id == self.file_send_peer)
            .map(|peer| peer.display_name.as_str())
            .unwrap_or("No connected PC");
        ui.horizontal_wrapped(|ui| {
            let destination_label = ui.label("Send to");
            egui::ComboBox::from_id_salt("file_send_peer")
                .selected_text(name)
                .show_ui(ui, |ui| {
                    for peer in &connected {
                        ui.selectable_value(
                            &mut self.file_send_peer,
                            peer.peer_id.clone(),
                            &peer.display_name,
                        );
                    }
                })
                .response
                .labelled_by(destination_label.id);
            if ui
                .add_enabled(
                    sharing_enabled && !self.file_send_peer.is_empty() && !self.file_send_pending,
                    egui::Button::new("Send file..."),
                )
                .clicked()
            {
                self.file_send_pending = true;
                self.task_runner().choose_and_send_file(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    self.file_send_peer.clone(),
                    self.native_window_handle,
                );
            }
        });
        if !sharing_enabled {
            ui.label("File sharing is off. Turn on Share files in Sharing to send or receive.");
            if ui.button("Open sharing settings").clicked() {
                self.selected_tab = Tab::Settings;
            }
        } else if self.file_send_pending {
            ui.label("Complete the file selection to send.");
        } else if connected.is_empty() {
            ui.label("Connect a paired PC before sending a file.");
        } else {
            ui.label("Choose a file using the Windows file picker. Receiving must be allowed on the other PC.");
        }
    }

    pub(super) fn render_transfer_center_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let completed_count = self
                .snapshot
                .file_transfers
                .iter()
                .filter(|transfer| transfer.state == "completed")
                .count();

            ui.horizontal_wrapped(|ui| {
                ui.heading("Files");
                ui.separator();
                ui.label(
                    egui::RichText::new(transfer_summary(&self.snapshot.file_transfers)).weak(),
                );
                if completed_count > 0
                    && ui
                        .button("Clear completed")
                        .on_hover_text("Remove completed transfers from this list")
                        .clicked()
                {
                    self.task_runner()
                        .clear_completed_file_transfers(self.tx.clone(), self.ctx.endpoint.clone());
                }
            });
            ui.add_space(8.0);

            self.render_file_send(ui);
            ui.add_space(16.0);

            if self.snapshot.file_transfers.is_empty() {
                ui.label("No files sent or received yet.");
                ui.label(
                    if !self
                        .snapshot
                        .features
                        .get("transfer_file")
                        .copied()
                        .unwrap_or(false)
                    {
                        "File sharing is off."
                    } else if self.snapshot.file_transfer_config.auto_accept_trusted_peers {
                        "Incoming files from trusted PCs are allowed."
                    } else {
                        "Incoming files are blocked. Allow trusted PCs in Sharing before receiving."
                    },
                );
                return;
            }

            let mut transfers = self.snapshot.file_transfers.clone();
            transfers.reverse();

            for transfer in transfers {
                ui.separator();
                self.render_transfer_row(ui, &transfer);
                ui.add_space(8.0);
            }
        });
    }

    fn render_transfer_row(&mut self, ui: &mut egui::Ui, transfer: &UiFileTransfer) {
        let file_label = if transfer.file_name.trim().is_empty() {
            "(unnamed)"
        } else {
            &transfer.file_name
        };
        ui.horizontal_wrapped(|ui| {
            ui.strong(file_label);
            ui.label(format!(
                "{} / {}",
                peer_transfer_label(&transfer.peer_id, &self.snapshot.paired_peers),
                transfer.direction
            ));
            ui.label(egui::RichText::new(transfer.state.replace('_', " ")));
        });
        ui.add(
            egui::ProgressBar::new(transfer.progress_fraction())
                .desired_width(ui.available_width().min(360.0))
                .text(format_transfer_progress(transfer)),
        );
        if !transfer.failure_reason.trim().is_empty() {
            ui.label(failure_reason_label(&transfer.failure_reason));
        }
        ui.collapsing(format!("Transfer details: {}", file_label), |ui| {
            ui.label(format!("Queued: {}", format_timestamp(&transfer.queued_at)));
            ui.label(format!(
                "Updated: {}",
                format_timestamp(&transfer.updated_at)
            ));
            ui.label(format!("Transfer: {}", transfer.transfer_id));
            ui.label(format!(
                "Previous attempt: {}",
                empty_as_none(&transfer.previous_transfer_id)
            ));
        });

        ui.horizontal_wrapped(|ui| {
            if transfer.can_cancel() && ui.button("Cancel").clicked() {
                self.task_runner().cancel_file_transfer(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    transfer.transfer_id.clone(),
                );
            }
            if transfer.can_retry()
                && self
                    .snapshot
                    .features
                    .get("transfer_file")
                    .copied()
                    .unwrap_or(false)
                && ui.button("Retry").clicked()
            {
                self.task_runner().retry_file_transfer(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    transfer.transfer_id.clone(),
                );
            }
            if transfer.can_open_location() && ui.button("Open folder").clicked() {
                self.task_runner()
                    .open_received_file_location(self.tx.clone(), transfer.final_path.clone());
            }
        });
    }
}

pub(super) fn transfer_summary(transfers: &[UiFileTransfer]) -> String {
    let active = transfers
        .iter()
        .filter(|transfer| matches!(transfer.state.as_str(), "active" | "queued"))
        .count();
    let failed = transfers
        .iter()
        .filter(|transfer| transfer.state == "failed")
        .count();
    if failed > 0 {
        format!(
            "{active} in progress; {failed} {} attention",
            if failed == 1 { "needs" } else { "need" }
        )
    } else if active > 0 {
        format!("{active} in progress")
    } else {
        "No transfers in progress".to_string()
    }
}

fn peer_transfer_label(peer_id: &str, peers: &[UiPairedPeer]) -> String {
    peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.display_name.clone())
        .unwrap_or_else(|| short_token(peer_id).to_string())
}

fn format_transfer_progress(transfer: &UiFileTransfer) -> String {
    if transfer.total_bytes == 0 {
        return "0 B".to_string();
    }
    format!(
        "{} / {}",
        format_bytes(transfer.transferred_bytes.min(transfer.total_bytes)),
        format_bytes(transfer.total_bytes)
    )
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn failure_reason_label(reason: &str) -> String {
    if reason.trim().is_empty() {
        String::new()
    } else {
        reason.replace('_', " ")
    }
}
