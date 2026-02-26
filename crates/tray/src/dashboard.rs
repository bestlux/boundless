use eframe::egui;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

pub(super) fn run() -> Result<()> {
    let cli = Cli::parse();
    let ctx = Arc::new(AppContext {
        endpoint: cli.endpoint,
        start_daemon: cli.start_daemon,
        ctl_candidates: resolve_boundlessctl_candidates(),
        daemon_candidates: resolve_boundlessd_candidates(),
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(false) // hidden on start, shown via tray
            .with_inner_size([760.0, 560.0])
            .with_title("Boundless Dashboard"),
        ..Default::default()
    };

    eframe::run_native(
        "Boundless Dashboard",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(DashboardApp::new(cc, ctx)))
        }),
    ).map_err(|e| anyhow::anyhow!("eframe error: {:?}", e))
}

enum AppMsg {
    SnapshotUpdated(UiSnapshot),
    SnapshotError(String),
    PairingChallenge(PairingChallengeState),
    PairingComplete(GuidedPairingResult),
    PairingFailed(String),
    ActionComplete(String),
    ActionFailed(String),
}

#[derive(PartialEq)]
enum Tab {
    Status,
    Layout,
    Settings,
}

struct DashboardApp {
    ctx: Arc<AppContext>,
    _tray_icon: Option<TrayIcon>,
    snapshot: UiSnapshot,
    last_error: Option<String>,
    last_message_is_error: bool,
    tx: Sender<AppMsg>,
    rx: Receiver<AppMsg>,
    
    // UI state
    selected_tab: Tab,
    
    // Manual setup
    manual_host: String,
    manual_port: String,

    // Pairing flow state
    pairing_flow: Option<GuidedPairingFlow>,
    pairing_challenge: Option<PairingChallengeState>,
    pairing_code: String,
    pairing_alias: String,
    pairing_in_progress: bool,
    pairing_retry_available: bool,

    // Layout manager state
    layout_up: String,
    layout_down: String,
    layout_left: String,
    layout_right: String,
}

impl DashboardApp {
    fn new(cc: &eframe::CreationContext<'_>, app_ctx: Arc<AppContext>) -> Self {
        let (tx, rx) = mpsc::channel();

        let bg_ctx = app_ctx.clone();
        let bg_tx = tx.clone();
        let egui_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let mut next_start_attempt = Instant::now();
            let mut start_backoff = Duration::from_secs(2);
            loop {
                match fetch_ui_snapshot_blocking(&bg_ctx.endpoint) {
                    Ok(snapshot) => {
                        next_start_attempt = Instant::now();
                        start_backoff = Duration::from_secs(2);
                        let _ = bg_tx.send(AppMsg::SnapshotUpdated(snapshot));
                        egui_ctx.request_repaint();
                    }
                    Err(e) => {
                        let mut message = e.to_string();
                        if bg_ctx.start_daemon && Instant::now() >= next_start_attempt {
                            match ensure_daemon_available_blocking(&bg_ctx) {
                                Ok(Some(path)) => {
                                    message = format!("{message}\nstarted daemon via `{path}`");
                                    next_start_attempt = Instant::now() + Duration::from_secs(8);
                                    start_backoff = Duration::from_secs(2);
                                }
                                Ok(None) => {
                                    next_start_attempt = Instant::now() + Duration::from_secs(8);
                                    start_backoff = Duration::from_secs(2);
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
                        let _ = bg_tx.send(AppMsg::SnapshotError(message));
                        egui_ctx.request_repaint();
                    }
                }
                std::thread::sleep(Duration::from_secs(4));
            }
        });

        let (tray_icon, tray_init_error) = match build_dashboard_tray_icon() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (
                None,
                Some(format!("tray initialization failed: {error}")),
            ),
        };
        if tray_icon.is_none() {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }

        Self {
            ctx: app_ctx,
            _tray_icon: tray_icon,
            snapshot: UiSnapshot::default(),
            last_error: tray_init_error,
            last_message_is_error: true,
            tx,
            rx,
            selected_tab: Tab::Status,
            manual_host: String::new(),
            manual_port: "15200".to_string(),
            pairing_flow: None,
            pairing_challenge: None,
            pairing_code: String::new(),
            pairing_alias: String::new(),
            pairing_in_progress: false,
            pairing_retry_available: false,
            layout_up: String::new(),
            layout_down: String::new(),
            layout_left: String::new(),
            layout_right: String::new(),
        }
    }

    fn start_pairing(&mut self, flow: GuidedPairingFlow, egui_ctx: egui::Context) {
        self.pairing_in_progress = true;
        self.pairing_flow = Some(flow.clone());
        self.pairing_challenge = None;
        self.pairing_code.clear();
        self.pairing_alias = flow.default_alias.clone();
        self.last_error = None;
        self.pairing_retry_available = false;

        let tx = self.tx.clone();
        let endpoint = self.ctx.endpoint.clone();
        std::thread::spawn(move || {
            match pair_nearby_request_code_blocking(&endpoint, flow.host.clone(), flow.pairing_port) {
                Ok(NearbyRequestCodeStart::CodeRequired { request_id, verification_nonce, expires_at }) => {
                    let challenge = PairingChallengeState { request_id, verification_nonce, expires_at };
                    let _ = tx.send(AppMsg::PairingChallenge(challenge));
                }
                Ok(NearbyRequestCodeStart::Unsupported { reason }) => {
                    let _ = tx.send(AppMsg::PairingFailed(format!("Target does not support guided pairing: {}", reason)));
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::PairingFailed(e.to_string()));
                }
            }
            egui_ctx.request_repaint();
        });
    }

    fn submit_pairing_code(&mut self, egui_ctx: egui::Context) {
        if let (Some(challenge), Some(flow)) = (self.pairing_challenge.clone(), self.pairing_flow.clone()) {
            let tx = self.tx.clone();
            let endpoint = self.ctx.endpoint.clone();
            let code = self.pairing_code.clone();
            let alias = if self.pairing_alias.trim().is_empty() { None } else { Some(self.pairing_alias.clone()) };
            let fallback_alias = flow.orientation_selector_fallback.clone();
            
            self.pairing_in_progress = true;

            std::thread::spawn(move || {
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
                        let _ = tx.send(AppMsg::PairingComplete(GuidedPairingResult {
                            peer_machine_id,
                            orientation_selector: alias.unwrap_or(fallback_alias),
                        }));
                    }
                    Err(e) => {
                        let _ = tx.send(AppMsg::PairingFailed(e.to_string()));
                    }
                }
                egui_ctx.request_repaint();
            });
        }
    }

    fn render_pairing_dialog(&mut self, ctx: &egui::Context) {
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
                    self.pairing_in_progress = false;
                    self.pairing_flow = None;
                    self.pairing_retry_available = false;
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
                            if self.pairing_code.trim().is_empty() {
                                self.last_error = Some("pairing code cannot be empty".to_string());
                                self.last_message_is_error = true;
                            } else {
                                self.submit_pairing_code(ctx.clone());
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            self.pairing_in_progress = false;
                            self.pairing_challenge = None;
                            self.pairing_flow = None;
                            self.pairing_retry_available = false;
                        }
                    });
                }
            });
        }
    }
}

impl eframe::App for DashboardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll Tray events
        if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id.as_ref() == ACTION_DASHBOARD {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if event.id.as_ref() == ACTION_QUIT {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Poll messages from background threads
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMsg::SnapshotUpdated(snap) => {
                    self.snapshot = snap;
                }
                AppMsg::SnapshotError(err) => {
                    self.last_error = Some(err);
                    self.last_message_is_error = true;
                }
                AppMsg::PairingChallenge(challenge) => {
                    self.pairing_in_progress = false;
                    self.pairing_challenge = Some(challenge);
                }
                AppMsg::PairingComplete(result) => {
                    self.pairing_in_progress = false;
                    self.pairing_challenge = None;
                    self.pairing_flow = None;
                    self.selected_tab = Tab::Layout;
                    self.last_error = Some(format!(
                        "Pairing successful with {} (selector: {})",
                        short_token(&result.peer_machine_id),
                        result.orientation_selector
                    ));
                    self.last_message_is_error = false;
                    self.pairing_retry_available = false;
                }
                AppMsg::PairingFailed(err) => {
                    self.pairing_in_progress = false;
                    let error = anyhow::anyhow!(err);
                    self.last_error = Some(format_error_for_dialog(&error));
                    self.last_message_is_error = true;
                    self.pairing_retry_available = should_offer_new_request_retry(&error);
                }
                AppMsg::ActionComplete(msg) => {
                    self.last_error = Some(msg);
                    self.last_message_is_error = false;
                }
                AppMsg::ActionFailed(err) => {
                    self.last_error = Some(err);
                    self.last_message_is_error = true;
                }
            }
        }

        self.render_pairing_dialog(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Boundless");
                ui.separator();
                ui.selectable_value(&mut self.selected_tab, Tab::Status, "Status & Pairing");
                ui.selectable_value(&mut self.selected_tab, Tab::Layout, "Layout Manager");
                ui.selectable_value(&mut self.selected_tab, Tab::Settings, "Settings");
            });
            ui.separator();

            if let Some(err) = &self.last_error {
                let color = if self.last_message_is_error {
                    egui::Color32::LIGHT_RED
                } else {
                    egui::Color32::LIGHT_GREEN
                };
                ui.add(egui::Label::new(egui::RichText::new(err).color(color)));
                if self.pairing_retry_available
                    && !self.pairing_in_progress
                    && let Some(flow) = self.pairing_flow.clone()
                    && ui.button("Retry Pairing Request").clicked()
                {
                    self.start_pairing(flow, ctx.clone());
                }
                ui.separator();
            }

            match self.selected_tab {
                Tab::Status => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.heading("Discovered Peers");
                        if self.snapshot.discovered_peers.is_empty() {
                            ui.label(egui::RichText::new("No peers discovered on local network.").italics());
                        } else {
                            egui::Grid::new("discovered_peers").striped(true).show(ui, |ui| {
                                for peer in self.snapshot.discovered_peers.clone() {
                                    ui.label(&peer.display_name);
                                    ui.label(short_token(&peer.machine_id));
                                    ui.label(&peer.endpoint);
                                    if ui.button("Connect").clicked() {
                                        if let Some((host, port)) = host_and_pairing_port_from_discovery_endpoint(&peer.endpoint) {
                                            self.start_pairing(GuidedPairingFlow {
                                                dialog_title: format!("Pair with {}", peer.display_name),
                                                host,
                                                pairing_port: port,
                                                default_alias: peer.display_name.clone(),
                                                orientation_selector_fallback: peer.display_name.clone(),
                                            }, ctx.clone());
                                        } else {
                                            self.last_error = Some("Failed to parse peer endpoint".into());
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
                                && let Ok(port) = parse_pairing_port(&self.manual_port)
                            {
                                self.start_pairing(
                                    GuidedPairingFlow {
                                        dialog_title: format!("Manual Pair {}", self.manual_host),
                                        host: self.manual_host.clone(),
                                        pairing_port: port,
                                        default_alias: String::new(),
                                        orientation_selector_fallback: self.manual_host.clone(),
                                    },
                                    ctx.clone(),
                                );
                            }
                        });

                        ui.add_space(16.0);
                        ui.heading("Paired Peers");
                        if self.snapshot.paired_peers.is_empty() {
                            ui.label(egui::RichText::new("No paired peers.").italics());
                        } else {
                            egui::Grid::new("paired_peers").striped(true).show(ui, |ui| {
                                for peer in &self.snapshot.paired_peers {
                                    let color = if peer.connected { egui::Color32::LIGHT_GREEN } else { egui::Color32::DARK_GRAY };
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
                                        if !req.requires_verification_code
                                            && ui.button("Approve").clicked()
                                        {
                                            let tx = self.tx.clone();
                                            let endpoint = self.ctx.endpoint.clone();
                                            let rid = req.request_id.clone();
                                            std::thread::spawn(move || {
                                                match approve_nearby_pairing_request_blocking(&endpoint, &rid) {
                                                    Ok(msg) => { let _ = tx.send(AppMsg::ActionComplete(msg)); }
                                                    Err(e) => { let _ = tx.send(AppMsg::ActionFailed(e.to_string())); }
                                                }
                                            });
                                        }
                                        if ui.button(if req.requires_verification_code { "Cancel" } else { "Reject" }).clicked() {
                                            let tx = self.tx.clone();
                                            let endpoint = self.ctx.endpoint.clone();
                                            let rid = req.request_id.clone();
                                            std::thread::spawn(move || {
                                                match reject_nearby_pairing_request_blocking(&endpoint, &rid) {
                                                    Ok(msg) => { let _ = tx.send(AppMsg::ActionComplete(msg)); }
                                                    Err(e) => { let _ = tx.send(AppMsg::ActionFailed(e.to_string())); }
                                                }
                                            });
                                        }
                                    });
                                });
                            }
                        }
                    });
                }
                Tab::Layout => {
                    ui.heading("Visual Layout Manager");
                    ui.label("Configure the topology around This PC using aliases or IDs.");
                    ui.add_space(16.0);
                    
                    let pc_box = |ui: &mut egui::Ui, title: &str, field: &mut String| {
                        ui.group(|ui| {
                            ui.set_width(120.0);
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new(title).strong());
                                ui.text_edit_singleline(field);
                            });
                        });
                    };

                    ui.vertical_centered(|ui| {
                        pc_box(ui, "Up", &mut self.layout_up);
                        ui.horizontal(|ui| {
                            ui.add_space((ui.available_width() - 360.0) / 2.0);
                            pc_box(ui, "Left", &mut self.layout_left);
                            ui.group(|ui| {
                                ui.set_width(120.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(egui::RichText::new("This PC").strong().color(egui::Color32::LIGHT_BLUE));
                                    ui.label(short_token(&self.snapshot.machine_id));
                                });
                            });
                            pc_box(ui, "Right", &mut self.layout_right);
                        });
                        pc_box(ui, "Down", &mut self.layout_down);
                    });

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if ui.button("Apply Layout").clicked() {
                            let mut args = vec!["layout".to_string(), "orient".to_string()];
                            if !self.layout_left.trim().is_empty() {
                                args.push("--left".to_string()); args.push(self.layout_left.trim().to_string());
                            }
                            if !self.layout_right.trim().is_empty() {
                                args.push("--right".to_string()); args.push(self.layout_right.trim().to_string());
                            }
                            if !self.layout_up.trim().is_empty() {
                                args.push("--up".to_string()); args.push(self.layout_up.trim().to_string());
                            }
                            if !self.layout_down.trim().is_empty() {
                                args.push("--down".to_string()); args.push(self.layout_down.trim().to_string());
                            }
                            
                            let ctx_clone = self.ctx.clone();
                            let tx = self.tx.clone();
                            std::thread::spawn(move || {
                                match run_boundlessctl(&ctx_clone, &args) {
                                    Ok(msg) => { let _ = tx.send(AppMsg::ActionComplete(format!("Layout applied: {}", msg))); }
                                    Err(e) => { let _ = tx.send(AppMsg::ActionFailed(format!("Layout failed: {}", e))); }
                                }
                            });
                        }
                    });
                    
                    ui.add_space(16.0);
                    ui.label(format!("Current Matrix: {}", self.snapshot.layout_matrix));
                }
                Tab::Settings => {
                    ui.heading("Settings & Diagnostics");
                    ui.label(format!("Machine ID: {}", self.snapshot.machine_id));
                    ui.label(format!("Daemon Status: {}", if self.snapshot.daemon_online { "Online" } else { "Offline" }));
                    ui.label(format!("API Endpoint: {}", self.ctx.endpoint));
                    ui.label(format!("Snapshot Timestamp: {}", self.snapshot.generated_at));
                    
                    ui.add_space(16.0);
                    if ui.button("Reconnect All Peers").clicked() {
                        let endpoint = self.ctx.endpoint.clone();
                        let tx = self.tx.clone();
                        std::thread::spawn(move || {
                            match trigger_hotkey_action_blocking(&endpoint, "reconnect") {
                                Ok(msg) => { let _ = tx.send(AppMsg::ActionComplete(msg)); }
                                Err(e) => { let _ = tx.send(AppMsg::ActionFailed(e.to_string())); }
                            }
                        });
                    }
                }
            }
        });
    }
}

fn build_dashboard_tray_icon() -> Result<TrayIcon> {
    let menu = Menu::new();
    menu
        .append(&MenuItem::with_id(
            ACTION_DASHBOARD,
            "Dashboard",
            true,
            None,
        ))
        .context("add dashboard menu item")?;
    menu.append(&PredefinedMenuItem::separator())
        .context("add tray separator")?;
    menu
        .append(&MenuItem::with_id(ACTION_QUIT, "Quit", true, None))
        .context("add quit menu item")?;

    let icon = make_tray_icon().context("build tray icon image")?;
    TrayIconBuilder::new()
        .with_tooltip("Boundless")
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()
        .context("build tray icon")
}
