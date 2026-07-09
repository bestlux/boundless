use super::*;

pub(super) enum AppMsg {
    SnapshotUpdated(Box<UiSnapshot>),
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
    TransferCenter,
    Settings,
}

// ── Toast notification system ──────────────────────────────────────────
pub(super) struct Toast {
    pub(super) id: u64,
    pub(super) message: String,
    pub(super) is_error: bool,
    pub(super) created_at: Instant,
}

pub(super) const TOAST_SUCCESS_SECS: u64 = 4;
pub(super) const TOAST_ERROR_SECS: u64 = 12;

pub(super) struct DashboardApp {
    pub(super) ctx: Arc<AppContext>,
    pub(super) _tray_icon: Option<TrayIcon>,
    pub(super) snapshot: UiSnapshot,
    pub(super) tx: Sender<AppMsg>,
    pub(super) rx: Receiver<AppMsg>,

    // Toast notifications (replaces old inline last_error banner)
    pub(super) toasts: Vec<Toast>,
    pub(super) toast_seq: u64,

    // Pairing-specific error state (shown in pairing dialog context)
    pub(super) pairing_last_error: Option<String>,
    pub(super) pairing_retry_available: bool,
    pub(super) pairing_role_reversal_available: bool,
    pub(super) pairing_role_reversal_attempt_id: Option<String>,
    pub(super) pairing_role_reversal_message: Option<String>,

    pub(super) selected_tab: Tab,
    pub(super) manual_host: String,
    pub(super) manual_port: String,

    pub(super) pairing_flow: Option<GuidedPairingFlow>,
    pub(super) pairing_challenge: Option<PairingChallengeState>,
    pub(super) pairing_code: String,
    pub(super) pairing_alias: String,
    pub(super) pairing_in_progress: bool,
    pub(super) pairing_attempt_seq: u64,
    pub(super) active_pairing_attempt_id: Option<u64>,
    pub(super) pending_onboarding_focus: bool,
    pub(super) onboarding_focus_shown: bool,
    pub(super) exit_requested: bool,
    pub(super) exit_requested_signal: Arc<AtomicBool>,
    pub(super) native_window_handle: Option<isize>,
    pub(super) activation_requested: Arc<AtomicBool>,
    pub(super) _single_instance_guard: Option<SingleInstanceGuard>,

    pub(super) layout_grid: HashMap<(i32, i32), String>,
    pub(super) layout_unassigned: Vec<String>,
    pub(super) layout_initialized: bool,
    pub(super) dragging_peer: Option<(String, (i32, i32))>,
    pub(super) last_layout_matrix: String,
    pub(super) last_layout_peer_ids: Vec<String>,
    pub(super) file_receive_dir_edit: String,
    pub(super) file_receive_dir_last_snapshot: String,
    pub(super) hotkey_edits: BTreeMap<String, String>,
    pub(super) hotkey_last_snapshot: BTreeMap<String, String>,

    // Undo: stash previous layout state before each drag/action
    pub(super) prev_layout_grid: Option<HashMap<(i32, i32), String>>,
    pub(super) prev_layout_unassigned: Option<Vec<String>>,

    // Apply confirmation dialog
    pub(super) confirm_apply_pending: bool,
    pub(super) confirm_network_reset_pending: bool,
    pub(super) confirm_safe_reset_pending: bool,
}

impl DashboardApp {
    pub(super) fn new(
        cc: &eframe::CreationContext<'_>,
        app_ctx: Arc<AppContext>,
        mut single_instance_guard: SingleInstanceGuard,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let exit_requested_signal = Arc::new(AtomicBool::new(false));
        let native_window_handle = native_window_handle_from_creation_context(cc);
        let activation_requested = Arc::new(AtomicBool::new(false));
        let activation_requested_signal = activation_requested.clone();
        let activation_ctx = cc.egui_ctx.clone();
        single_instance_guard.start_activation_listener(move || {
            activation_requested_signal.store(true, Ordering::SeqCst);
            activation_ctx.request_repaint();
        })?;

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

        let mut toasts = Vec::new();
        let mut toast_seq = 0u64;
        if let Some(err) = tray_init_error {
            toast_seq += 1;
            toasts.push(Toast {
                id: toast_seq,
                message: err,
                is_error: true,
                created_at: Instant::now(),
            });
        }

        Ok(Self {
            ctx: app_ctx,
            _tray_icon: tray_icon,
            snapshot: UiSnapshot::default(),
            tx,
            rx,
            toasts,
            toast_seq,
            pairing_last_error: None,
            pairing_retry_available: false,
            pairing_role_reversal_available: false,
            pairing_role_reversal_attempt_id: None,
            pairing_role_reversal_message: None,
            selected_tab: Tab::Status,
            manual_host: String::new(),
            manual_port: "15200".to_string(),
            pairing_flow: None,
            pairing_challenge: None,
            pairing_code: String::new(),
            pairing_alias: String::new(),
            pairing_in_progress: false,
            pairing_attempt_seq: 0,
            active_pairing_attempt_id: None,
            pending_onboarding_focus: false,
            onboarding_focus_shown: false,
            exit_requested: false,
            exit_requested_signal,
            native_window_handle,
            activation_requested,
            _single_instance_guard: Some(single_instance_guard),
            layout_grid: HashMap::new(),
            layout_unassigned: Vec::new(),
            layout_initialized: false,
            dragging_peer: None,
            last_layout_matrix: String::new(),
            last_layout_peer_ids: Vec::new(),
            file_receive_dir_edit: String::new(),
            file_receive_dir_last_snapshot: String::new(),
            hotkey_edits: BTreeMap::new(),
            hotkey_last_snapshot: BTreeMap::new(),
            prev_layout_grid: None,
            prev_layout_unassigned: None,
            confirm_apply_pending: false,
            confirm_network_reset_pending: false,
            confirm_safe_reset_pending: false,
        })
    }

    pub(super) fn begin_pairing_flow(&mut self, flow: GuidedPairingFlow) -> u64 {
        self.pairing_attempt_seq = self.pairing_attempt_seq.saturating_add(1);
        let attempt_id = self.pairing_attempt_seq;
        self.pairing_in_progress = true;
        self.pairing_flow = Some(flow.clone());
        self.pairing_challenge = None;
        self.pairing_code.clear();
        self.pairing_alias = flow.default_alias;
        self.pairing_last_error = None;
        self.pairing_retry_available = false;
        self.pairing_role_reversal_available = false;
        self.pairing_role_reversal_attempt_id = None;
        self.pairing_role_reversal_message = None;
        self.active_pairing_attempt_id = Some(attempt_id);
        attempt_id
    }

    pub(super) fn cancel_pairing_flow(&mut self) {
        self.pairing_in_progress = false;
        self.pairing_challenge = None;
        self.pairing_flow = None;
        self.pairing_retry_available = false;
        self.pairing_role_reversal_available = false;
        self.pairing_role_reversal_attempt_id = None;
        self.pairing_role_reversal_message = None;
        self.active_pairing_attempt_id = None;
    }

    // ── Toast helpers ──────────────────────────────────────────────────
    pub(super) fn push_toast(&mut self, message: String, is_error: bool) {
        self.toast_seq += 1;
        self.toasts.push(Toast {
            id: self.toast_seq,
            message,
            is_error,
            created_at: Instant::now(),
        });
    }

    pub(super) fn tick_toasts(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| {
            let max_age = if t.is_error {
                Duration::from_secs(TOAST_ERROR_SECS)
            } else {
                Duration::from_secs(TOAST_SUCCESS_SECS)
            };
            now.duration_since(t.created_at) < max_age
        });
    }

    pub(super) fn dismiss_toast(&mut self, id: u64) {
        self.toasts.retain(|t| t.id != id);
    }

    pub(super) fn has_active_toasts(&self) -> bool {
        !self.toasts.is_empty()
    }

    // ── Layout undo helpers ────────────────────────────────────────────
    pub(super) fn stash_layout_for_undo(&mut self) {
        self.prev_layout_grid = Some(self.layout_grid.clone());
        self.prev_layout_unassigned = Some(self.layout_unassigned.clone());
    }

    pub(super) fn undo_layout(&mut self) {
        if let Some(grid) = self.prev_layout_grid.take() {
            let unassigned = self.prev_layout_unassigned.take().unwrap_or_default();
            // Stash current as new undo target (allows redo-like toggle)
            self.prev_layout_grid = Some(self.layout_grid.clone());
            self.prev_layout_unassigned = Some(self.layout_unassigned.clone());
            self.layout_grid = grid;
            self.layout_unassigned = unassigned;
        }
    }

    pub(super) fn apply_app_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::SnapshotUpdated(snap) => {
                let snap = *snap;
                let receive_dir = snap.file_transfer_config.receive_dir.clone();
                if self.file_receive_dir_edit.trim().is_empty()
                    || self.file_receive_dir_edit == self.file_receive_dir_last_snapshot
                {
                    self.file_receive_dir_edit = receive_dir.clone();
                }
                self.file_receive_dir_last_snapshot = receive_dir;
                if self.hotkey_edits.is_empty() || self.hotkey_edits == self.hotkey_last_snapshot {
                    self.hotkey_edits = snap.hotkeys.clone();
                }
                self.hotkey_last_snapshot = snap.hotkeys.clone();
                self.snapshot = snap;
                if should_offer_first_run_onboarding(&self.snapshot) && !self.onboarding_focus_shown
                {
                    self.pending_onboarding_focus = true;
                }
            }
            AppMsg::SnapshotError(err) => {
                // Deduplicate: if the most recent toast is also a snapshot
                // error, replace it instead of stacking (the snapshot watcher
                // retries every ~1s, so without this we'd flood the overlay).
                if let Some(last) = self.toasts.last_mut()
                    && last.is_error
                {
                    last.message = err;
                    last.created_at = Instant::now();
                    return;
                }
                self.push_toast(err, true);
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
                self.push_toast(
                    format!(
                        "Pairing successful with {} (selector: {}): {}",
                        short_token(&result.peer_machine_id),
                        result.orientation_selector,
                        result.message
                    ),
                    false,
                );
                self.pairing_retry_available = false;
                self.pairing_role_reversal_available = false;
                self.pairing_role_reversal_attempt_id = None;
                self.pairing_role_reversal_message = None;
                self.pairing_last_error = None;
            }
            AppMsg::PairingFailed { attempt_id, error } => {
                if Some(attempt_id) != self.active_pairing_attempt_id {
                    return;
                }
                self.pairing_in_progress = false;
                let error = anyhow::anyhow!(error);
                self.pairing_last_error = Some(format_error_for_dialog(&error));
                self.pairing_retry_available = should_offer_new_request_retry(&error);
                self.pairing_role_reversal_available = self
                    .pairing_flow
                    .as_ref()
                    .is_some_and(|flow| !flow.host.trim().is_empty())
                    && should_offer_role_reversal(&error);
                self.pairing_role_reversal_attempt_id =
                    self.pairing_flow.as_ref().and_then(|flow| {
                        self.pairing_role_reversal_available
                            .then(|| role_reversal_attempt_id(flow, attempt_id))
                    });
                self.pairing_role_reversal_message = self.pairing_flow.as_ref().and_then(|flow| {
                    self.pairing_role_reversal_available.then(|| {
                        role_reversal_next_action_message(
                            flow,
                            self.pairing_role_reversal_attempt_id.as_deref(),
                            self.pairing_challenge.is_some(),
                        )
                    })
                });
            }
            AppMsg::ActionComplete(msg) => {
                self.push_toast(msg, false);
            }
            AppMsg::ActionFailed(err) => {
                self.push_toast(err, true);
            }
        }
    }
}
