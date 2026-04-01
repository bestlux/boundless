use super::*;

pub(super) enum AppMsg {
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
pub(super) enum Tab {
    Status,
    Layout,
    Settings,
}

pub(super) struct DashboardApp {
    pub(super) ctx: Arc<AppContext>,
    pub(super) _tray_icon: Option<TrayIcon>,
    pub(super) snapshot: UiSnapshot,
    pub(super) last_error: Option<String>,
    pub(super) last_message_is_error: bool,
    pub(super) tx: Sender<AppMsg>,
    pub(super) rx: Receiver<AppMsg>,

    pub(super) selected_tab: Tab,
    pub(super) manual_host: String,
    pub(super) manual_port: String,

    pub(super) pairing_flow: Option<GuidedPairingFlow>,
    pub(super) pairing_challenge: Option<PairingChallengeState>,
    pub(super) pairing_code: String,
    pub(super) pairing_alias: String,
    pub(super) pairing_in_progress: bool,
    pub(super) pairing_retry_available: bool,
    pub(super) pairing_attempt_seq: u64,
    pub(super) active_pairing_attempt_id: Option<u64>,
    pub(super) pending_onboarding_focus: bool,
    pub(super) onboarding_focus_shown: bool,
    pub(super) exit_requested: bool,
    pub(super) exit_requested_signal: Arc<AtomicBool>,
    pub(super) native_window_handle: Option<isize>,

    pub(super) layout_grid: HashMap<(i32, i32), String>,
    pub(super) layout_unassigned: Vec<String>,
    pub(super) layout_initialized: bool,
    pub(super) dragging_peer: Option<(String, (i32, i32))>,
    pub(super) last_layout_matrix: String,
}

impl DashboardApp {
    pub(super) fn new(cc: &eframe::CreationContext<'_>, app_ctx: Arc<AppContext>) -> Self {
        let (tx, rx) = mpsc::channel();
        let exit_requested_signal = Arc::new(AtomicBool::new(false));
        let native_window_handle = native_window_handle_from_creation_context(cc);

        DashboardTaskRunner::spawn_snapshot_watch(
            app_ctx.clone(),
            tx.clone(),
            cc.egui_ctx.clone(),
        );

        let menu_ctx = cc.egui_ctx.clone();
        let menu_exit_requested = exit_requested_signal.clone();
        let menu_window_handle = native_window_handle;
        tray_icon::menu::MenuEvent::set_event_handler(Some(
            move |event: tray_icon::menu::MenuEvent| {
                if event.id.as_ref() == ACTION_DASHBOARD {
                    show_dashboard_window(menu_window_handle, &menu_ctx);
                } else if event.id.as_ref() == ACTION_QUIT {
                    request_dashboard_exit(menu_window_handle, &menu_ctx, &menu_exit_requested);
                }
            },
        ));

        let (tray_icon, tray_init_error) = match build_dashboard_tray_icon() {
            Ok(tray) => (Some(tray), None),
            Err(error) => (None, Some(format!("tray initialization failed: {error}"))),
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
            pending_onboarding_focus: false,
            onboarding_focus_shown: false,
            exit_requested: false,
            exit_requested_signal,
            native_window_handle,
            layout_grid: HashMap::new(),
            layout_unassigned: Vec::new(),
            layout_initialized: false,
            dragging_peer: None,
            last_layout_matrix: String::new(),
        }
    }

    pub(super) fn begin_pairing_flow(&mut self, flow: GuidedPairingFlow) -> u64 {
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

    pub(super) fn cancel_pairing_flow(&mut self) {
        self.pairing_in_progress = false;
        self.pairing_challenge = None;
        self.pairing_flow = None;
        self.pairing_retry_available = false;
        self.active_pairing_attempt_id = None;
    }

    pub(super) fn apply_app_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::SnapshotUpdated(snap) => {
                self.snapshot = snap;
                self.last_error = None;
                self.last_message_is_error = false;
                if should_offer_first_run_onboarding(&self.snapshot) && !self.onboarding_focus_shown
                {
                    self.pending_onboarding_focus = true;
                }
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
}
