use super::*;

impl UiFileTransfer {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            "completed" | "failed" | "cancelled"
        )
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
    pub(super) fn render_transfer_center_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let completed_count = self
                .snapshot
                .file_transfers
                .iter()
                .filter(|transfer| transfer.state == "completed")
                .count();

            ui.horizontal(|ui| {
                ui.heading("Transfer Center");
                ui.separator();
                ui.label(egui::RichText::new(transfer_summary(&self.snapshot.file_transfers)).weak());
                let clear = ui.add_enabled(
                    completed_count > 0,
                    egui::Button::new("Clear Completed"),
                );
                let clear_clicked = clear.clicked();
                clear.on_hover_text("Remove completed transfer entries from this session");
                if clear_clicked {
                    self.task_runner().clear_completed_file_transfers(
                        self.tx.clone(),
                        self.ctx.endpoint.clone(),
                    );
                }
            });
            ui.add_space(8.0);

            if self.snapshot.file_transfers.is_empty() {
                ui.label(egui::RichText::new("No recent file transfers.").italics());
                return;
            }

            let mut transfers = self.snapshot.file_transfers.clone();
            transfers.reverse();

            egui::Grid::new("transfer_center_grid")
                .striped(true)
                .num_columns(8)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("File").strong());
                    ui.label(egui::RichText::new("Peer").strong());
                    ui.label(egui::RichText::new("Direction").strong());
                    ui.label(egui::RichText::new("State").strong());
                    ui.label(egui::RichText::new("Progress").strong());
                    ui.label(egui::RichText::new("Updated").strong());
                    ui.label(egui::RichText::new("Reason").strong());
                    ui.label("");
                    ui.end_row();

                    for transfer in transfers {
                        self.render_transfer_row(ui, &transfer);
                        ui.end_row();
                    }
                });
        });
    }

    fn render_transfer_row(&mut self, ui: &mut egui::Ui, transfer: &UiFileTransfer) {
        let file_label = if transfer.file_name.trim().is_empty() {
            "(unnamed)"
        } else {
            &transfer.file_name
        };
        ui.label(file_label).on_hover_text(format!(
            "Transfer ID: {}\nPrevious attempt: {}\nQueued: {}",
            transfer.transfer_id,
            empty_as_none(&transfer.previous_transfer_id),
            format_timestamp(&transfer.queued_at)
        ));

        ui.label(peer_transfer_label(&transfer.peer_id, &self.snapshot.paired_peers))
            .on_hover_text(&transfer.peer_id);
        ui.label(transfer.direction.replace('_', " "));
        ui.label(
            egui::RichText::new(transfer.state.replace('_', " "))
                .color(transfer_state_color(&transfer.state)),
        );
        ui.add(
            egui::ProgressBar::new(transfer.progress_fraction())
                .desired_width(130.0)
                .text(format_transfer_progress(transfer)),
        );
        ui.label(format_timestamp(&transfer.updated_at));
        ui.label(failure_reason_label(&transfer.failure_reason))
            .on_hover_text(&transfer.failure_reason);

        ui.horizontal(|ui| {
            let cancel = ui.add_enabled(transfer.can_cancel(), egui::Button::new("Cancel"));
            let cancel_clicked = cancel.clicked();
            cancel.on_hover_text("Cancel this outgoing transfer attempt");
            if cancel_clicked {
                self.task_runner().cancel_file_transfer(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    transfer.transfer_id.clone(),
                );
            }

            let retry = ui.add_enabled(transfer.can_retry(), egui::Button::new("Retry"));
            let retry_clicked = retry.clicked();
            retry.on_hover_text(if transfer.source_path.trim().is_empty() {
                "Retry source path is unavailable"
            } else {
                "Retry this outgoing transfer from the beginning"
            });
            if retry_clicked {
                self.task_runner().retry_file_transfer(
                    self.tx.clone(),
                    self.ctx.endpoint.clone(),
                    transfer.transfer_id.clone(),
                );
            }

            let open = ui.add_enabled(transfer.can_open_location(), egui::Button::new("Open"));
            let open_clicked = open.clicked();
            open.on_hover_text("Open the received file location");
            if open_clicked {
                self.task_runner().open_received_file_location(
                    self.tx.clone(),
                    transfer.final_path.clone(),
                );
            }
        });
    }
}

pub(super) fn transfer_summary(transfers: &[UiFileTransfer]) -> String {
    let queued = transfers
        .iter()
        .filter(|transfer| transfer.state == "queued")
        .count();
    let active = transfers
        .iter()
        .filter(|transfer| transfer.state == "active")
        .count();
    let completed = transfers
        .iter()
        .filter(|transfer| transfer.state == "completed")
        .count();
    let failed = transfers
        .iter()
        .filter(|transfer| transfer.state == "failed")
        .count();
    let cancelled = transfers
        .iter()
        .filter(|transfer| transfer.state == "cancelled")
        .count();

    format!(
        "queued {queued} | active {active} | completed {completed} | failed {failed} | cancelled {cancelled}"
    )
}

fn peer_transfer_label(peer_id: &str, peers: &[UiPairedPeer]) -> String {
    peers
        .iter()
        .find(|peer| peer.peer_id == peer_id)
        .map(|peer| peer.display_name.clone())
        .unwrap_or_else(|| short_token(peer_id).to_string())
}

fn transfer_state_color(state: &str) -> egui::Color32 {
    match state {
        "queued" => egui::Color32::LIGHT_YELLOW,
        "active" => egui::Color32::LIGHT_BLUE,
        "completed" => egui::Color32::LIGHT_GREEN,
        "failed" => egui::Color32::LIGHT_RED,
        "cancelled" => egui::Color32::GRAY,
        _ => egui::Color32::WHITE,
    }
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
