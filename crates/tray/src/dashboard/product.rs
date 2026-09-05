use super::*;

fn needs_arrangement(snapshot: &UiSnapshot) -> bool {
    snapshot.paired_peers.iter().any(|peer| peer.connected)
        && !snapshot.layout_matrix.split([';', ',']).any(|token| {
            dashboard_layout::resolve_layout_token_peer(token, &snapshot.paired_peers)
                .is_some_and(|peer| peer.connected)
        })
}

fn home_guidance(snapshot: &UiSnapshot, paused: bool) -> &'static str {
    if snapshot.paired_peers.is_empty() {
        return "Use one keyboard and mouse across your PCs.";
    }
    if !snapshot
        .features
        .get("share_input")
        .copied()
        .unwrap_or(false)
    {
        "Keyboard and mouse sharing is paused. Resume it when you are ready."
    } else if paused {
        "Input pause requested. The background runtime has not confirmed it yet."
    } else if !snapshot.paired_peers.iter().any(|peer| peer.connected) {
        "Ready when your other PC is. Offline PCs reconnect automatically."
    } else if needs_arrangement(snapshot) {
        "Your PC is connected. Arrange your PCs before switching at screen edges."
    } else if !matches!(
        snapshot.input_runtime.capture_backend_mode.as_str(),
        "windows_hooks" | "user_session_broker" | "direct"
    ) {
        "Your PCs are connected, but desktop input is not ready. Check input status."
    } else if !snapshot
        .features
        .get("easy_mouse")
        .copied()
        .unwrap_or(false)
    {
        "Your PCs are connected. Screen-edge switching is off in Sharing."
    } else {
        "Your PCs are connected and arranged. Move across a shared screen edge to switch."
    }
}

pub(super) fn configure_dashboard_style(ctx: &egui::Context) {
    // Follow Windows' theme; keep a quiet, legible utility without animation.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 32.0;
        style.animation_time = 0.0;
        style.visuals.override_text_color = Some(if style.visuals.dark_mode {
            egui::Color32::from_rgb(232, 234, 237)
        } else {
            egui::Color32::from_rgb(28, 32, 38)
        });
        style.visuals.weak_text_color = Some(if style.visuals.dark_mode {
            egui::Color32::from_rgb(188, 193, 199)
        } else {
            egui::Color32::from_rgb(77, 84, 93)
        });
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(23.0));
    });
    // Read the Windows UI font once at construction. Keep bundled fallbacks for
    // missing fonts or unpackaged test environments; no font is redistributed.
    if let Some(windows_dir) = std::env::var_os("WINDIR")
        && let Ok(font) = std::fs::read(
            std::path::Path::new(&windows_dir)
                .join("Fonts")
                .join("segoeui.ttf"),
        )
    {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "windows-ui".to_string(),
            egui::FontData::from_owned(font).into(),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "windows-ui".to_string());
        ctx.set_fonts(fonts);
    }
}

impl DashboardApp {
    pub(super) fn render_home(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(if self.snapshot.paired_peers.is_empty() { "Connect your first PC" } else { "Your PCs" });
            ui.add_space(4.0);
            if !self.snapshot.daemon_online {
                ui.strong("Boundless needs attention");
                ui.label(if self.snapshot_error.is_some() {
                    "Current status is unavailable. Your paired PCs are remembered; their connection status is unknown."
                } else {
                    "Waiting for Boundless to start on this PC."
                });
                if ui.button("Open Support").clicked() {
                    self.selected_tab = Tab::Support;
                }
            } else {
                ui.label(home_guidance(&self.snapshot, self.input_pause_requested));
                if needs_arrangement(&self.snapshot) && ui.button("Arrange connected PCs").clicked() {
                    self.selected_tab = Tab::Layout;
                }
                if !matches!(self.snapshot.input_runtime.capture_backend_mode.as_str(), "windows_hooks" | "user_session_broker" | "direct")
                    && ui.button("Check input status").clicked() {
                    self.selected_tab = Tab::Settings;
                }
            }
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                let input_enabled = self.snapshot.features.get("share_input").copied().unwrap_or(false);
                if self.input_pause_requested || self.local_input_paused() || (self.snapshot.daemon_online && !input_enabled) {
                    ui.strong(if !input_enabled && self.snapshot.daemon_online {
                        "Input sharing paused"
                    } else {
                        "Input pause requested"
                    });
                    if ui.add_enabled(self.snapshot.daemon_online && !self.input_change_pending, egui::Button::new("Resume input")).clicked() {
                        self.resume_input();
                    }
                } else if ui.button("Pause input").clicked() {
                    self.pause_input();
                }
                ui.label("To return control: press Ctrl twice on this keyboard.");
            });
            ui.add_space(16.0);
            let peers = self.snapshot.paired_peers.clone();
            for peer in peers {
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    ui.strong(&peer.display_name);
                    ui.label(if !self.snapshot.daemon_online {
                        "Status unavailable"
                    } else if peer.connected {
                        "Connected"
                    } else {
                        "Offline"
                    });
                    if peer.connected && ui.button("Arrange").clicked() {
                        self.selected_tab = Tab::Layout;
                    }
                });
                if !peer.connected && self.snapshot.daemon_online {
                    ui.label("Check that this PC is awake and Boundless is running on it.");
                    if ui.button(format!("Connection help for {}", peer.display_name)).clicked() {
                        self.selected_tab = Tab::Support;
                    }
                }
                egui::CollapsingHeader::new("PC details").id_salt(&peer.peer_id).show(ui, |ui| {
                    ui.label(format!("Trust: {}", peer.trust_state.replace('_', " ")));
                    ui.label(format!("Trusted since: {}", peer.trusted_since));
                    ui.label(&peer.health_reason);
                    ui.label(format!("Address: {}", peer.address));
                    ui.label(format!("Identity: {}", peer.device_identity));
                    ui.label(format!("Fingerprint: {}", peer.trust_fingerprint));
                    if ui.add_enabled(self.snapshot.daemon_online, egui::Button::new("Forget this PC...")).clicked() {
                        self.pending_peer_removal = Some(peer.peer_id.clone());
                    }
                });
            }
            ui.add_space(16.0);
            ui.separator();
            egui::CollapsingHeader::new("Add a PC")
                .default_open(self.snapshot.paired_peers.is_empty() || !self.snapshot.pending_requests.is_empty())
                .show(ui, |ui| {
                    ui.add_enabled_ui(self.snapshot.daemon_online, |ui| self.render_pairing_setup(ui, ctx));
                });
        });
        self.render_forget_confirmation(ctx);
    }

    fn local_input_paused(&self) -> bool {
        self._input_broker_supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.pause_control().is_paused())
    }

    pub(super) fn pause_input(&mut self) {
        // Local hook release comes first. The broker remains paused if IPC fails.
        if let Some(supervisor) = &self._input_broker_supervisor {
            supervisor.pause_control().pause();
        }
        self.input_pause_requested = true;
        self.input_change_pending = true;
        self.task_runner()
            .set_input_sharing(self.tx.clone(), self.ctx.endpoint.clone(), false);
    }

    pub(super) fn resume_input(&mut self) {
        if self.input_change_pending {
            return;
        }
        if let Some(supervisor) = &self._input_broker_supervisor {
            supervisor.pause_control().pause();
        }
        self.input_pause_requested = true;
        self.input_change_pending = true;
        self.task_runner()
            .set_input_sharing(self.tx.clone(), self.ctx.endpoint.clone(), true);
    }

    fn render_forget_confirmation(&mut self, ctx: &egui::Context) {
        let Some(peer_id) = self.pending_peer_removal.clone() else {
            return;
        };
        let Some(peer) = self
            .snapshot
            .paired_peers
            .iter()
            .find(|p| p.peer_id == peer_id)
        else {
            self.pending_peer_removal = None;
            return;
        };
        let name = peer.display_name.clone();
        egui::Modal::new(egui::Id::new("forget_pc")).show(ctx, |ui| {
            ui.heading(format!("Forget {name}?"));
            ui.label("This removes trust and its saved connection. You will need to pair these PCs again.");
            ui.label("An offline PC usually only needs to wake up or reconnect.");
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() { self.pending_peer_removal = None; }
                if ui.add_enabled(self.snapshot.daemon_online, egui::Button::new("Forget PC")).clicked() {
                    self.task_runner().remove_peer(self.tx.clone(), self.ctx.endpoint.clone(), peer_id.clone());
                    self.pending_peer_removal = None;
                }
            });
        });
    }

    pub(super) fn render_support(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Support");
            ui.label(format!("Boundless {}", env!("CARGO_PKG_VERSION")));
            if self.snapshot.daemon_online {
                ui.label(format!("Connected background runtime: {}", self.snapshot.daemon_version));
                if self.snapshot.daemon_version != env!("CARGO_PKG_VERSION") {
                    ui.strong("Versions differ. Repair or upgrade this PC's installation.");
                }
            } else {
                ui.label("Background runtime unavailable. Its current version could not be checked.");
            }
            ui.add_space(16.0);
            ui.heading("Connection help");
            ui.label("Make sure both PCs are awake, running Boundless, and on your trusted local network. An offline PC keeps its pairing.");
            if ui.add_enabled(self.snapshot.daemon_online, egui::Button::new("Retry connections")).clicked() {
                self.task_runner().reconnect_all_peers(self.tx.clone(), self.ctx.endpoint.clone());
            }
            ui.label("Retries all paired PCs without removing trust.");
            ui.add_space(16.0);
            ui.heading("Save a support report");
            ui.label("Save versions and redacted diagnostic events locally. Clipboard contents, secrets, peer addresses, filenames and local paths are excluded. Nothing is sent.");
            if ui.add_enabled(self.snapshot.daemon_online, egui::Button::new("Save redacted report")).clicked() {
                self.support_status = Some("Saving report...".to_string());
                self.task_runner().export_support(self.tx.clone(), self.ctx.endpoint.clone());
            }
            if !self.snapshot.daemon_online {
                ui.label("Report export needs the background runtime. If a Start service banner is shown, use it; otherwise repair the Boundless installation.");
            }
            if let Some(status) = &self.support_status { ui.add(egui::Label::new(status).selectable(true)); }
            ui.add_space(16.0);
            self.render_paired_testing_permission(ui);
            ui.add_space(16.0);
            ui.collapsing("Technical details", |ui| {
                ui.label(format!("Control endpoint: {}", self.ctx.endpoint));
                ui.label(format!("Last update: {}", self.snapshot.generated_at));
                ui.label(format!("Machine ID: {}", self.snapshot.machine_id));
                if let Some(error) = &self.snapshot_error { ui.label(error); }
                if ui.button("Copy details").clicked() {
                    ui.ctx().copy_text(format!("Boundless tray {}\nRuntime {}\nOnline {}\n{}", env!("CARGO_PKG_VERSION"), self.snapshot.daemon_version, self.snapshot.daemon_online, self.snapshot_error.as_deref().unwrap_or("")));
                }
            });
            ui.add_space(8.0);
            ui.collapsing("Reset connections or preferences", |ui| {
                ui.label("Use this only after checking connectivity. Ordinary reconnects do not require a reset.");
                ui.add_enabled_ui(self.snapshot.daemon_online, |ui| {
                    if ui.button("Reset connections...").clicked() { self.confirm_network_reset_pending = true; }
                    if self.confirm_network_reset_pending {
                        ui.strong("Remove every paired PC and clear connection state?");
                        ui.label("Your local identity is kept. Pair each PC again afterward.");
                        ui.horizontal(|ui| {
                            if ui.button("Cancel connection reset").clicked() { self.confirm_network_reset_pending = false; }
                            if ui.button("Remove all pairings").clicked() {
                                self.confirm_network_reset_pending = false;
                                self.task_runner().safe_reset(self.tx.clone(), self.ctx.endpoint.clone(), true, false, format!("safe-reset-network:{}", self.snapshot.machine_id));
                            }
                        });
                    }
                    if ui.button("Reset preferences...").clicked() { self.confirm_safe_reset_pending = true; }
                    if self.confirm_safe_reset_pending {
                        ui.strong("Reset Boundless configuration and runtime state?");
                        ui.label("Installed files and local identity are kept. Your settings and connections will need setup again.");
                        ui.horizontal(|ui| {
                            if ui.button("Cancel preference reset").clicked() { self.confirm_safe_reset_pending = false; }
                            if ui.button("Reset now").clicked() {
                                self.confirm_safe_reset_pending = false;
                                self.task_runner().safe_reset(self.tx.clone(), self.ctx.endpoint.clone(), false, true, format!("safe-reset-all:{}", self.snapshot.machine_id));
                            }
                        });
                    }
                });
            });
        });
    }
}

impl DashboardApp {
    fn render_paired_testing_permission(&mut self, ui: &mut egui::Ui) {
        ui.collapsing("Paired connection testing", |ui| {
            ui.label("Temporarily let one trusted PC measure this connection with bounded test data. This does not grant control of input, clipboard, or files.");
            if self.paired_testing_peer.is_empty() {
                self.paired_testing_peer = self.snapshot.paired_peers.first()
                    .map(|peer| peer.peer_id.clone()).unwrap_or_default();
            }
            let selected_name = self.snapshot.paired_peers.iter()
                .find(|peer| peer.peer_id == self.paired_testing_peer)
                .map(|peer| peer.display_name.as_str()).unwrap_or("Choose a paired PC");
            egui::ComboBox::from_label("Allow testing from")
                .selected_text(selected_name).show_ui(ui, |ui| {
                    for peer in &self.snapshot.paired_peers {
                        ui.selectable_value(&mut self.paired_testing_peer, peer.peer_id.clone(), &peer.display_name);
                    }
                });
            if let Some(status) = &self.paired_testing {
                let elapsed = self.paired_testing_updated_at.map(|time| time.elapsed().as_secs()).unwrap_or(0);
                let remaining = u64::from(status.remaining_seconds).saturating_sub(elapsed);
                if status.enabled && remaining > 0 {
                    let name = self.snapshot.paired_peers.iter().find(|peer| Some(&peer.peer_id) == status.peer_id.as_ref())
                        .map(|peer| peer.display_name.as_str()).unwrap_or("previously selected PC");
                    ui.strong(format!("Last reported: allowed for {name}; expires within {remaining}s."));
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                } else {
                    ui.label("Permission is off or has expired.");
                }
            } else {
                ui.label("Permission status has not been checked in this window.");
            }
            if let Some(error) = &self.paired_testing_error { ui.label(format!("Could not check or change permission: {error}")); }
            let available = self.snapshot.daemon_online && !self.paired_testing_pending;
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(available && !self.paired_testing_peer.is_empty(), egui::Button::new("Allow paired testing for 10 minutes")).clicked() {
                    self.change_paired_testing_permission(Some((self.paired_testing_peer.clone(), 600)));
                }
                if ui.add_enabled(available, egui::Button::new("Stop paired testing")).clicked() {
                    let peer_id = self.paired_testing.as_ref().and_then(|status| status.peer_id.clone()).unwrap_or_else(|| self.paired_testing_peer.clone());
                    self.change_paired_testing_permission(Some((peer_id, 0)));
                }
                if ui.add_enabled(available, egui::Button::new("Refresh permission status")).clicked() {
                    self.change_paired_testing_permission(None);
                }
            });
            if self.paired_testing_pending { ui.label("Checking permission..."); }
        });
    }

    fn change_paired_testing_permission(&mut self, change: Option<(String, u32)>) {
        self.paired_testing_pending = true;
        self.paired_testing_error = None;
        self.task_runner().paired_testing_permission(
            self.tx.clone(),
            self.ctx.endpoint.clone(),
            change,
        );
    }
}
