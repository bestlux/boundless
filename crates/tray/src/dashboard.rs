use eframe::egui;
use std::collections::HashMap;
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
    PairingChallenge {
        attempt_id: u64,
        challenge: PairingChallengeState,
    },
    PairingComplete {
        attempt_id: u64,
        result: GuidedPairingResult,
    },
    PairingFailed {
        attempt_id: u64,
        error: String,
    },
    ActionComplete(String),
    ActionFailed(String),
}

#[derive(Debug, PartialEq)]
enum Tab {
    Status,
    Layout,
    Settings,
}

const CANONICAL_LOCAL_LAYOUT_TOKEN: &str = "self";

fn is_local_layout_token(token: &str, local_machine_id: &str) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return false;
    }
    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "self" | "local" | "this" | "me"
    ) || trimmed.eq_ignore_ascii_case(local_machine_id)
}

fn count_local_layout_cells(
    layout_grid: &HashMap<(i32, i32), String>,
    local_machine_id: &str,
) -> usize {
    layout_grid
        .values()
        .filter(|id| id.eq_ignore_ascii_case(local_machine_id))
        .count()
}

fn validate_layout_before_apply(
    layout_grid: &HashMap<(i32, i32), String>,
    local_machine_id: &str,
) -> Result<()> {
    match count_local_layout_cells(layout_grid, local_machine_id) {
        1 => Ok(()),
        0 => anyhow::bail!("layout must include This PC exactly once before applying"),
        _ => anyhow::bail!("layout must include This PC exactly once before applying"),
    }
}

fn serialize_layout_matrix(layout_grid: &HashMap<(i32, i32), String>, local_machine_id: &str) -> String {
    let mut positions = layout_grid.keys();
    let Some(&(first_x, first_y)) = positions.next() else {
        return String::new();
    };

    let mut min_x = first_x;
    let mut max_x = first_x;
    let mut min_y = first_y;
    let mut max_y = first_y;

    for (x, y) in positions {
        if *x < min_x {
            min_x = *x;
        }
        if *x > max_x {
            max_x = *x;
        }
        if *y < min_y {
            min_y = *y;
        }
        if *y > max_y {
            max_y = *y;
        }
    }

    let mut rows = Vec::new();
    for y in min_y..=max_y {
        let mut cols = Vec::new();
        for x in min_x..=max_x {
            if let Some(id) = layout_grid.get(&(x, y)) {
                cols.push(if id.eq_ignore_ascii_case(local_machine_id) {
                    CANONICAL_LOCAL_LAYOUT_TOKEN.to_string()
                } else {
                    id.clone()
                });
            } else {
                cols.push(String::new());
            }
        }
        rows.push(cols.join(","));
    }

    rows.join(";")
}

fn validate_pairing_code(code: &str) -> Result<()> {
    if code.trim().is_empty() {
        anyhow::bail!("pairing code cannot be empty");
    }
    Ok(())
}

fn guided_flow_from_discovered_peer(peer: &UiDiscoveredPeer) -> Result<GuidedPairingFlow> {
    let Some((host, pairing_port)) = host_and_pairing_port_from_discovery_endpoint(&peer.endpoint)
    else {
        anyhow::bail!("Failed to parse peer endpoint");
    };

    Ok(GuidedPairingFlow {
        dialog_title: format!("Pair with {}", peer.display_name),
        host,
        pairing_port,
        default_alias: peer.display_name.clone(),
        orientation_selector_fallback: peer.display_name.clone(),
    })
}

fn guided_flow_from_manual_input(host: &str, port_text: &str) -> Result<GuidedPairingFlow> {
    Ok(GuidedPairingFlow {
        dialog_title: format!("Manual Pair {}", host),
        host: host.to_string(),
        pairing_port: parse_pairing_port(port_text)?,
        default_alias: String::new(),
        orientation_selector_fallback: host.to_string(),
    })
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
    pairing_attempt_seq: u64,
    active_pairing_attempt_id: Option<u64>,

    // Layout manager state
    layout_grid: HashMap<(i32, i32), String>,
    layout_unassigned: Vec<String>,
    layout_initialized: bool,
    dragging_peer: Option<(String, (i32, i32))>,
    last_layout_matrix: String,
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
            pairing_attempt_seq: 0,
            active_pairing_attempt_id: None,
            layout_grid: HashMap::new(),
            layout_unassigned: Vec::new(),
            layout_initialized: false,
            dragging_peer: None,
            last_layout_matrix: String::new(),
        }
    }

    fn begin_pairing_flow(&mut self, flow: GuidedPairingFlow) -> u64 {
        self.pairing_attempt_seq = self.pairing_attempt_seq.saturating_add(1);
        let attempt_id = self.pairing_attempt_seq;
        self.pairing_in_progress = true;
        self.pairing_flow = Some(flow.clone());
        self.pairing_challenge = None;
        self.pairing_code.clear();
        self.pairing_alias = flow.default_alias;
        self.last_error = None;
        self.pairing_retry_available = false;
        self.active_pairing_attempt_id = Some(attempt_id);
        attempt_id
    }

    fn cancel_pairing_flow(&mut self) {
        self.pairing_in_progress = false;
        self.pairing_challenge = None;
        self.pairing_flow = None;
        self.pairing_retry_available = false;
        self.active_pairing_attempt_id = None;
    }

    fn apply_app_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::SnapshotUpdated(snap) => {
                self.snapshot = snap;
            }
            AppMsg::SnapshotError(err) => {
                self.last_error = Some(err);
                self.last_message_is_error = true;
            }
            AppMsg::PairingChallenge {
                attempt_id,
                challenge,
            } => {
                if Some(attempt_id) != self.active_pairing_attempt_id {
                    return;
                }
                self.pairing_in_progress = false;
                self.pairing_challenge = Some(challenge);
            }
            AppMsg::PairingComplete { attempt_id, result } => {
                if Some(attempt_id) != self.active_pairing_attempt_id {
                    return;
                }
                self.pairing_in_progress = false;
                self.pairing_challenge = None;
                self.pairing_flow = None;
                self.active_pairing_attempt_id = None;
                self.selected_tab = Tab::Layout;
                self.last_error = Some(format!(
                    "Pairing successful with {} (selector: {})",
                    short_token(&result.peer_machine_id),
                    result.orientation_selector
                ));
                self.last_message_is_error = false;
                self.pairing_retry_available = false;
            }
            AppMsg::PairingFailed { attempt_id, error } => {
                if Some(attempt_id) != self.active_pairing_attempt_id {
                    return;
                }
                self.pairing_in_progress = false;
                let error = anyhow::anyhow!(error);
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

    fn start_pairing(&mut self, flow: GuidedPairingFlow, egui_ctx: egui::Context) {
        let attempt_id = self.begin_pairing_flow(flow.clone());

        let tx = self.tx.clone();
        let endpoint = self.ctx.endpoint.clone();
        std::thread::spawn(move || {
            match pair_nearby_request_code_blocking(&endpoint, flow.host.clone(), flow.pairing_port) {
                Ok(NearbyRequestCodeStart::CodeRequired { request_id, verification_nonce, expires_at }) => {
                    let challenge = PairingChallengeState { request_id, verification_nonce, expires_at };
                    let _ = tx.send(AppMsg::PairingChallenge { attempt_id, challenge });
                }
                Ok(NearbyRequestCodeStart::Unsupported { reason }) => {
                    let _ = tx.send(AppMsg::PairingFailed {
                        attempt_id,
                        error: format!("Target does not support guided pairing: {}", reason),
                    });
                }
                Err(e) => {
                    let _ = tx.send(AppMsg::PairingFailed {
                        attempt_id,
                        error: e.to_string(),
                    });
                }
            }
            egui_ctx.request_repaint();
        });
    }

    fn submit_pairing_code(&mut self, egui_ctx: egui::Context) {
        if let (Some(challenge), Some(flow), Some(attempt_id)) = (
            self.pairing_challenge.clone(),
            self.pairing_flow.clone(),
            self.active_pairing_attempt_id,
        ) {
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
                        let _ = tx.send(AppMsg::PairingComplete {
                            attempt_id,
                            result: GuidedPairingResult {
                                peer_machine_id,
                                orientation_selector: alias.unwrap_or(fallback_alias),
                            },
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(AppMsg::PairingFailed {
                            attempt_id,
                            error: e.to_string(),
                        });
                    }
                }
                egui_ctx.request_repaint();
            });
        }
    }

    fn confirm_pairing_code(&mut self, egui_ctx: egui::Context) {
        if let Err(error) = validate_pairing_code(&self.pairing_code) {
            self.last_error = Some(error.to_string());
            self.last_message_is_error = true;
            return;
        }

        self.submit_pairing_code(egui_ctx);
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
            self.apply_app_msg(msg);
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
                                && let Ok(flow) = guided_flow_from_manual_input(
                                    &self.manual_host,
                                    &self.manual_port,
                                )
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
                    if (!self.layout_initialized && !self.snapshot.machine_id.is_empty()) || 
                       (self.layout_initialized && self.snapshot.layout_matrix != self.last_layout_matrix && self.dragging_peer.is_none()) {
                        
                        self.layout_initialized = true;
                        self.last_layout_matrix = self.snapshot.layout_matrix.clone();
                        self.layout_grid.clear();
                        self.layout_unassigned.clear();
                        
                        let matrix_str = &self.snapshot.layout_matrix;
                        let peers = &self.snapshot.paired_peers;
                        let local_id = &self.snapshot.machine_id;
                        
                        if matrix_str.trim().is_empty() {
                            self.layout_grid.insert((3, 3), local_id.clone());
                            let mut left_x = 2;
                            let mut right_x = 4;
                            let mut toggle = true;
                            for p in peers {
                                if toggle && left_x >= 0 {
                                    self.layout_grid.insert((left_x, 3), p.peer_id.clone());
                                    left_x -= 1;
                                } else if right_x < 7 {
                                    self.layout_grid.insert((right_x, 3), p.peer_id.clone());
                                    right_x += 1;
                                } else if left_x >= 0 {
                                    self.layout_grid.insert((left_x, 3), p.peer_id.clone());
                                    left_x -= 1;
                                }
                                toggle = !toggle;
                            }
                        } else {
                            let rows: Vec<Vec<String>> = matrix_str.split(';').map(|r| r.split(',').map(|s| s.trim().to_string()).collect()).collect();
                            let h = rows.len() as i32;
                            let w = rows.iter().map(|r| r.len()).max().unwrap_or(0) as i32;
                            let offset_x = (7 - w) / 2;
                            let offset_y = (7 - h) / 2;
                            
                            for (y, row) in rows.iter().enumerate() {
                                for (x, token) in row.iter().enumerate() {
                                    if token.is_empty() { continue; }
                                    let peer_id = if is_local_layout_token(token, local_id) {
                                        local_id.clone()
                                    } else if let Some(p) = peers.iter().find(|p| p.display_name == *token || p.peer_id == *token) {
                                        p.peer_id.clone()
                                    } else {
                                        token.clone()
                                    };
                                    
                                    let gx = x as i32 + offset_x;
                                    let gy = y as i32 + offset_y;
                                    if (0..7).contains(&gx) && (0..7).contains(&gy) {
                                        self.layout_grid.insert((gx, gy), peer_id.clone());
                                    } else {
                                        self.layout_unassigned.push(peer_id.clone());
                                    }
                                }
                            }
                        }
                        
                        let all_placed: Vec<String> = self.layout_grid.values().cloned().collect();
                        for p in peers {
                            if !all_placed.contains(&p.peer_id) && !self.layout_unassigned.contains(&p.peer_id) {
                                self.layout_unassigned.push(p.peer_id.clone());
                            }
                        }
                        if !all_placed.contains(local_id) && !self.layout_unassigned.contains(local_id) {
                            self.layout_unassigned.push(local_id.clone());
                        }
                    }

                    let get_display_name = |id: &str| -> String {
                        if id == self.snapshot.machine_id {
                            return "This PC".to_string();
                        }
                        if let Some(p) = self.snapshot.paired_peers.iter().find(|p| p.peer_id == id) {
                            return p.display_name.clone();
                        }
                        short_token(id).to_string()
                    };

                    ui.heading("Visual Layout Manager");
                    ui.label("Drag and drop devices onto the grid to configure your layout.");
                    ui.add_space(8.0);

                    let mut drag_stopped = false;
                    let mut pointer_pos_at_drop = None;
                    let mut cell_rects = Vec::new();
                    let mut unassigned_rects = Vec::new();

                    let cell_size = egui::vec2(90.0, 60.0);
                    let mut new_grid = self.layout_grid.clone();
                    let mut new_unassigned = self.layout_unassigned.clone();

                    ui.group(|ui| {
                        ui.label("Unassigned Devices");
                        ui.horizontal_wrapped(|ui| {
                            if self.layout_unassigned.is_empty() {
                                ui.label(egui::RichText::new("None").italics());
                            }
                            for (i, peer_id) in self.layout_unassigned.iter().enumerate() {
                                let (rect, response) = ui.allocate_exact_size(cell_size, egui::Sense::click_and_drag());
                                unassigned_rects.push((rect, i));
                                
                                let is_being_dragged = self.dragging_peer.is_some() && response.dragged();
                                
                                if response.drag_started() {
                                    self.dragging_peer = Some((peer_id.clone(), (-1, i as i32)));
                                    new_unassigned.remove(i);
                                }
                                
                                if response.drag_stopped() {
                                    drag_stopped = true;
                                    pointer_pos_at_drop = ctx.pointer_interact_pos();
                                }
                                
                                let painter = ui.painter();
                                if !is_being_dragged {
                                    painter.rect_filled(rect.shrink(4.0), 6.0, egui::Color32::from_rgb(50, 60, 70));
                                    painter.rect_stroke(rect.shrink(4.0), 6.0, egui::Stroke::new(1.0, egui::Color32::DARK_GRAY));
                                    let text = get_display_name(peer_id);
                                    let color = if peer_id == &self.snapshot.machine_id { egui::Color32::LIGHT_BLUE } else { egui::Color32::WHITE };
                                    let mut job = egui::text::LayoutJob::simple(text, egui::FontId::proportional(12.0), color, rect.width() - 8.0);
                                    job.halign = egui::Align::Center;
                                    let galley = ctx.fonts(|f| f.layout_job(job));
                                    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
                                }
                            }
                        });
                    });

                    ui.add_space(16.0);

                    ui.vertical_centered(|ui| {
                        egui::Frame::canvas(ui.style()).fill(egui::Color32::from_rgb(25, 30, 35)).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                            for y in 0..7 {
                                ui.horizontal(|ui| {
                                for x in 0..7 {
                                    let (rect, response) = ui.allocate_exact_size(cell_size, egui::Sense::click_and_drag());
                                    cell_rects.push((rect, x, y));
                                    
                                    let is_hovered = response.hovered() && self.dragging_peer.is_some();
                                    let is_being_dragged = self.dragging_peer.is_some() && response.dragged();
                                    
                                    if response.drag_started()
                                        && let Some(peer_id) = self.layout_grid.get(&(x, y))
                                    {
                                        self.dragging_peer = Some((peer_id.clone(), (x, y)));
                                        new_grid.remove(&(x, y));
                                    }
                                    
                                    if response.drag_stopped() {
                                        drag_stopped = true;
                                        pointer_pos_at_drop = ctx.pointer_interact_pos();
                                    }
                                    
                                    let painter = ui.painter();
                                    if is_hovered {
                                        painter.rect_filled(rect.shrink(2.0), 4.0, egui::Color32::from_rgb(40, 50, 60));
                                    }
                                    painter.rect_stroke(rect.shrink(2.0), 4.0, egui::Stroke::new(1.0, egui::Color32::from_rgb(45, 50, 55)));
                                    
                                    if let Some(peer_id) = self.layout_grid.get(&(x, y))
                                        && !is_being_dragged
                                    {
                                        let is_local = peer_id == &self.snapshot.machine_id;
                                        let bg_color = if is_local { egui::Color32::from_rgb(30, 70, 110) } else { egui::Color32::from_rgb(50, 60, 70) };
                                        painter.rect_filled(rect.shrink(4.0), 6.0, bg_color);
                                        let border_color = if is_local { egui::Color32::LIGHT_BLUE } else { egui::Color32::DARK_GRAY };
                                        painter.rect_stroke(rect.shrink(4.0), 6.0, egui::Stroke::new(1.5, border_color));
                                        let text = get_display_name(peer_id);
                                        let mut job = egui::text::LayoutJob::simple(text, egui::FontId::proportional(12.0), egui::Color32::WHITE, rect.width() - 8.0);
                                        job.halign = egui::Align::Center;
                                        let galley = ctx.fonts(|f| f.layout_job(job));
                                        painter.galley(rect.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
                                    }
                                }
                            });
                        }
                    });
                    });

                    if drag_stopped
                        && let Some((peer_id, old_pos)) = self.dragging_peer.take()
                    {
                        if let Some(pos) = pointer_pos_at_drop {
                            let mut dropped_in_cell = None;
                            for (rect, x, y) in &cell_rects {
                                if rect.contains(pos) { dropped_in_cell = Some((*x, *y)); break; }
                            }
                            
                            if let Some(new_pos) = dropped_in_cell {
                                if let Some(occupant) = self.layout_grid.get(&new_pos).cloned() {
                                    if old_pos.0 == -1 {
                                        new_unassigned.insert(old_pos.1 as usize, occupant);
                                    } else {
                                        new_grid.insert(old_pos, occupant);
                                    }
                                } else if old_pos.0 == -1 {
                                    // Removed from unassigned already above
                                }
                                new_grid.insert(new_pos, peer_id);
                            } else if old_pos.0 != -1 {
                                new_unassigned.push(peer_id);
                            } else {
                                new_unassigned.insert(old_pos.1 as usize, peer_id);
                            }
                        } else if old_pos.0 != -1 {
                            new_grid.insert(old_pos, peer_id);
                        } else {
                            new_unassigned.insert(old_pos.1 as usize, peer_id);
                        }
                    }

                    self.layout_grid = new_grid;
                    self.layout_unassigned = new_unassigned;

                    if let Some((peer_id, _)) = &self.dragging_peer
                        && let Some(pos) = ctx.pointer_hover_pos()
                    {
                        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drag_layer")));
                        let rect = egui::Rect::from_center_size(pos, cell_size);
                        let is_local = peer_id == &self.snapshot.machine_id;
                        let bg_color = if is_local { egui::Color32::from_rgb(40, 90, 140) } else { egui::Color32::from_rgb(70, 80, 90) };
                        painter.rect_filled(rect.shrink(4.0), 6.0, bg_color);
                        painter.rect_stroke(rect.shrink(4.0), 6.0, egui::Stroke::new(2.0, egui::Color32::WHITE));
                        let text = get_display_name(peer_id);
                        let mut job = egui::text::LayoutJob::simple(text, egui::FontId::proportional(12.0), egui::Color32::WHITE, rect.width() - 8.0);
                        job.halign = egui::Align::Center;
                        let galley = ctx.fonts(|f| f.layout_job(job));
                        painter.galley(rect.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
                    }

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if ui.button("Apply Layout").clicked() {
                            match validate_layout_before_apply(&self.layout_grid, &self.snapshot.machine_id) {
                                Ok(()) => {
                                    let matrix_str = serialize_layout_matrix(&self.layout_grid, &self.snapshot.machine_id);
                                    let args = vec!["layout".to_string(), "set".to_string(), matrix_str];
                                    let ctx_clone = self.ctx.clone();
                                    let tx = self.tx.clone();
                                    std::thread::spawn(move || {
                                        match run_boundlessctl(&ctx_clone, &args) {
                                            Ok(msg) => { let _ = tx.send(AppMsg::ActionComplete(format!("Layout applied: {}", msg))); }
                                            Err(e) => { let _ = tx.send(AppMsg::ActionFailed(format!("Layout failed: {}", e))); }
                                        }
                                    });
                                }
                                Err(error) => {
                                    self.last_error = Some(error.to_string());
                                    self.last_message_is_error = true;
                                }
                            }
                        }
                        
                        if ui.button("Reset Layout").clicked() {
                            self.layout_initialized = false;
                            self.snapshot.layout_matrix = String::new();
                        }
                    });

                    ui.add_space(8.0);
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
