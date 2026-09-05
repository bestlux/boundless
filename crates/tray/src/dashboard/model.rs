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
    SupportExportComplete(String),
    InputSharingComplete(bool),
    InputSharingFailed(String),
    FileSendComplete(std::result::Result<Option<String>, String>),
    PairedTestingUpdated(
        std::result::Result<app_services::paired_testing::PairedTestConsent, String>,
    ),
    ServiceRecoveryRequired(ServiceRecoveryOffer),
    ServiceRecoveryComplete(String),
    ServiceRecoveryFailed(String),
}

#[derive(Debug, Clone)]
pub(super) struct ServiceRecoveryUiState {
    pub(super) offer: ServiceRecoveryOffer,
    pub(super) in_progress: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum Tab {
    Status,
    Layout,
    TransferCenter,
    Settings,
    Support,
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
    pub(super) task_runner: DashboardTaskRunner,
    pub(super) snapshot_error: Option<String>,
    pub(super) pending_peer_removal: Option<String>,
    pub(super) support_status: Option<String>,
    pub(super) input_pause_requested: bool,
    pub(super) input_change_pending: bool,
    pub(super) paired_testing: Option<app_services::paired_testing::PairedTestConsent>,
    pub(super) paired_testing_updated_at: Option<Instant>,
    pub(super) paired_testing_pending: bool,
    pub(super) paired_testing_error: Option<String>,
    pub(super) paired_testing_peer: String,
    pub(super) file_send_peer: String,
    pub(super) file_send_pending: bool,
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
    pub(super) pending_service_recovery_focus: bool,
    pub(super) service_recovery: Option<ServiceRecoveryUiState>,
    pub(super) exit_requested: bool,
    pub(super) exit_requested_signal: Arc<AtomicBool>,
    pub(super) native_window_handle: Option<isize>,
    pub(super) activation_requested: Arc<AtomicBool>,
    pub(super) _single_instance_guard: Option<SingleInstanceGuard>,
    pub(super) _input_broker_supervisor: Option<InputBrokerSupervisor>,
    pub(super) elevated_input_controller: Option<ElevatedInputController>,

    pub(super) layout_grid: HashMap<(i32, i32), String>,
    pub(super) layout_unassigned: Vec<String>,
    pub(super) layout_initialized: bool,
    pub(super) layout_selected_peer: String,
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

    // Must remain last: its Drop disarms the process-level shutdown deadline,
    // after every field that can perform blocking cleanup has been dropped.
    pub(super) _shutdown_subclass:
        Option<platform_windows::cooperative_shutdown::TrayShutdownSubclass>,
}

impl DashboardApp {
    pub(super) fn new(
        cc: &eframe::CreationContext<'_>,
        app_ctx: Arc<AppContext>,
        mut single_instance_guard: SingleInstanceGuard,
        exit_requested_signal: Arc<AtomicBool>,
    ) -> Result<Self> {
        // eframe has created winit's event loop before invoking the app
        // creator. Registering Raw Input here ensures the input broker is the
        // final owner after winit's generic mouse registration.
        let elevated_input_controller = ElevatedInputController::start()?;
        let input_broker_supervisor = spawn_input_broker_supervisor(
            app_ctx.endpoint.clone(),
            elevated_input_controller.clone(),
        )?;
        let input_broker_shutdown = input_broker_supervisor.shutdown_signal();
        let (tx, rx) = mpsc::channel();
        let native_window_handle = native_window_handle_from_creation_context(cc);
        let shutdown_subclass = native_window_handle
            .map(|hwnd| {
                platform_windows::cooperative_shutdown::TrayShutdownSubclass::attach(
                    hwnd,
                    exit_requested_signal.clone(),
                )
            })
            .transpose()?;
        let activation_requested = Arc::new(AtomicBool::new(false));
        let activation_requested_signal = activation_requested.clone();
        let activation_ctx = cc.egui_ctx.clone();
        let shutdown_broker = input_broker_shutdown.clone();
        let shutdown_exit_requested = exit_requested_signal.clone();
        let shutdown_ctx = cc.egui_ctx.clone();
        let shutdown_window_handle = native_window_handle;
        single_instance_guard.start_listener(
            move || {
                activation_requested_signal.store(true, Ordering::SeqCst);
                activation_ctx.request_repaint();
            },
            move || {
                shutdown_broker.request();
                request_dashboard_exit(
                    shutdown_window_handle,
                    &shutdown_ctx,
                    &shutdown_exit_requested,
                );
            },
        )?;

        DashboardTaskRunner::spawn_snapshot_watch(app_ctx.clone(), tx.clone(), cc.egui_ctx.clone());

        let menu_ctx = cc.egui_ctx.clone();
        let menu_exit_requested = exit_requested_signal.clone();
        let menu_window_handle = native_window_handle;
        tray_icon::menu::MenuEvent::set_event_handler(Some(
            move |event: tray_icon::menu::MenuEvent| {
                if event.id.as_ref() == ACTION_DASHBOARD {
                    show_dashboard_window(menu_window_handle, &menu_ctx);
                } else if event.id.as_ref() == ACTION_QUIT {
                    input_broker_shutdown.request();
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
            task_runner: DashboardTaskRunner::new(),
            snapshot_error: None,
            pending_peer_removal: None,
            support_status: None,
            input_pause_requested: false,
            input_change_pending: false,
            paired_testing: None,
            paired_testing_updated_at: None,
            paired_testing_pending: false,
            paired_testing_error: None,
            paired_testing_peer: String::new(),
            file_send_peer: String::new(),
            file_send_pending: false,
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
            pending_service_recovery_focus: false,
            service_recovery: None,
            exit_requested: false,
            exit_requested_signal,
            native_window_handle,
            activation_requested,
            _shutdown_subclass: shutdown_subclass,
            _single_instance_guard: Some(single_instance_guard),
            _input_broker_supervisor: Some(input_broker_supervisor),
            elevated_input_controller: Some(elevated_input_controller),
            layout_grid: HashMap::new(),
            layout_unassigned: Vec::new(),
            layout_initialized: false,
            layout_selected_peer: String::new(),
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
                if self.file_receive_dir_edit == self.file_receive_dir_last_snapshot {
                    self.file_receive_dir_edit = receive_dir.clone();
                }
                self.file_receive_dir_last_snapshot = receive_dir;
                if self.hotkey_edits.is_empty() || self.hotkey_edits == self.hotkey_last_snapshot {
                    self.hotkey_edits = snap.hotkeys.clone();
                }
                self.hotkey_last_snapshot = snap.hotkeys.clone();
                self.snapshot = snap;
                self.snapshot_error = None;
                self.service_recovery = None;
                if should_offer_first_run_onboarding(&self.snapshot) && !self.onboarding_focus_shown
                {
                    self.pending_onboarding_focus = true;
                }
            }
            AppMsg::SnapshotError(err) => {
                self.snapshot.daemon_online = false;
                for peer in &mut self.snapshot.paired_peers {
                    peer.connected = false;
                    peer.health_state = "unknown".to_string();
                    peer.health_reason = "Waiting for current status from this PC".to_string();
                }
                self.snapshot_error = Some(err);
            }
            AppMsg::PairedTestingUpdated(result) => {
                self.paired_testing_pending = false;
                match result {
                    Ok(status) => {
                        self.paired_testing = Some(status);
                        self.paired_testing_updated_at = Some(Instant::now());
                        self.paired_testing_error = None;
                    }
                    Err(error) => self.paired_testing_error = Some(error),
                }
            }
            AppMsg::FileSendComplete(result) => {
                self.file_send_pending = false;
                match result {
                    Ok(Some(message)) => self.push_toast(message, false),
                    Ok(None) => {}
                    Err(error) => self.push_toast(error, true),
                }
            }
            AppMsg::InputSharingComplete(enabled) => {
                self.input_change_pending = false;
                self.snapshot
                    .features
                    .insert("share_input".to_string(), enabled);
                self.input_pause_requested = !enabled;
                if enabled && let Some(supervisor) = &self._input_broker_supervisor {
                    supervisor.pause_control().resume();
                }
            }
            AppMsg::InputSharingFailed(error) => {
                self.input_change_pending = false;
                self.push_toast(error, true);
            }
            AppMsg::SupportExportComplete(message) => {
                self.support_status = Some(message);
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
                self.layout_selected_peer = result.peer_machine_id;
                self.push_toast(
                    format!(
                        "Paired with {}. Arrange your PCs to start sharing.",
                        result.orientation_selector
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
            AppMsg::ServiceRecoveryRequired(offer) => {
                let first_offer = self.service_recovery.is_none();
                if !self
                    .service_recovery
                    .as_ref()
                    .is_some_and(|recovery| recovery.in_progress)
                {
                    self.service_recovery = Some(ServiceRecoveryUiState {
                        offer,
                        in_progress: false,
                    });
                }
                if first_offer {
                    self.pending_service_recovery_focus = true;
                }
            }
            AppMsg::ServiceRecoveryComplete(message) => {
                if let Some(recovery) = self.service_recovery.as_mut() {
                    recovery.in_progress = false;
                }
                self.push_toast(message, false);
            }
            AppMsg::ServiceRecoveryFailed(error) => {
                if let Some(recovery) = self.service_recovery.as_mut() {
                    recovery.in_progress = false;
                    recovery.offer.message = error.clone();
                }
                self.push_toast(error, true);
            }
        }
    }
}
