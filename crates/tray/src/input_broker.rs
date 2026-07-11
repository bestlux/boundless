// User-session input broker host.
//
// When the local control plane is owned by the LocalSystem service (which
// truthfully reports `service_session_unsupported` for interactive input),
// this broker runs in the tray's interactive user session, captures local
// input with the shared hook pump, relays it to the service over the existing
// allowed-user named pipe, and injects authenticated incoming frames with
// SendInput. The service remains the trust/network/routing authority; this
// covers the normal unlocked desktop only (no lock screen, secure desktop,
// UAC prompts, or elevated apps).

use ipc_api::boundless::v1::{
    ClipboardBrokerApplyReport, ClipboardBrokerExchangeRequest, ClipboardBrokerPayload,
    ClipboardBrokerLocalPayloadDisposition, InputBrokerAttachReply, InputBrokerAttachRequest,
    InputBrokerDetachRequest, InputBrokerExchangeRequest,
    clipboard_broker_payload, control_plane_service_client::ControlPlaneServiceClient,
};
use ipc_api::broker_events::{broker_events_from_input_events, input_events_from_broker_events};
use platform_windows::clipboard_backend::WindowsClipboardBackend;
use platform_windows::input::{
    HookControlAction, HookInputPump, InputSendOutcome, WindowsInputState, WindowsNumLockState,
    current_process_can_use_interactive_input, virtual_screen_bounds,
};
use tonic::transport::Channel;

const INPUT_BROKER_SERVICE_UNSUPPORTED_MODE: &str = "service_session_unsupported";
const INPUT_BROKER_SUPERVISOR_RETRY: Duration = Duration::from_secs(3);
const INPUT_BROKER_ACTIVE_POLL: Duration = Duration::from_millis(8);
const INPUT_BROKER_IDLE_POLL: Duration = Duration::from_millis(40);
const INPUT_BROKER_LOCK_LEASE: Duration = Duration::from_secs(2);
const INPUT_BROKER_CAPTURE_STAGING_CAP: usize = 4096;
const INPUT_BROKER_INJECT_BATCH_CAP: usize = 64;
const CLIPBOARD_BROKER_POLL: Duration = Duration::from_millis(200);
const CLIPBOARD_BROKER_RETRY: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
struct SafetyUnlockReconciler {
    pending_escape_count: u32,
    pending_lease_expired_count: u32,
    pending_detector_unavailable_count: u32,
    waiting_for_daemon_release: bool,
}

#[derive(Debug, Default)]
struct BrokerCaptureForwardingGate {
    daemon_authorized: bool,
    staged_events: Vec<core_input::InputEvent>,
    dropped_event_count: u64,
}

#[derive(Debug, Default)]
struct BrokerCaptureBatch {
    captured_events: Vec<core_input::InputEvent>,
    handoff_probe: Option<(i32, i32)>,
}

#[derive(Debug, Default)]
struct ClipboardBrokerState {
    last_sequence: Option<u64>,
    pending_local_payload: Option<PendingLocalClipboardPayload>,
    unread_newer_sequence: Option<u64>,
    apply_report: Option<ClipboardBrokerApplyReport>,
}

#[derive(Debug, Clone)]
struct PendingLocalClipboardPayload {
    sequence: u64,
    payload: core_clipboard::ClipboardPayload,
}

#[derive(Debug, Default)]
struct ClipboardPollOutcome {
    suppress_remote_apply: bool,
    error: Option<anyhow::Error>,
}

#[derive(Debug)]
struct InjectedInputState {
    windows_input: WindowsInputState,
    pressed_keys: Vec<(u16, core_input::KeySemantics)>,
    pressed_buttons: Vec<core_input::MouseButton>,
}

#[derive(Debug, Default)]
struct BrokerInjectBatchState {
    active_batch_id: Option<u64>,
    frames: std::collections::VecDeque<Vec<core_input::InputEvent>>,
    last_completed_batch_id: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BrokerInjectProgress {
    completed_frames: u32,
    failed_attempts: u32,
}

impl BrokerInjectBatchState {
    fn backpressure_active(&self) -> bool {
        self.active_batch_id.is_some()
    }

    fn acked_batch_id(&self) -> u64 {
        self.last_completed_batch_id
    }

    fn accept_batch(
        &mut self,
        batch_id: u64,
        frames: Vec<Vec<core_input::InputEvent>>,
    ) -> Result<()> {
        if batch_id == 0 {
            if frames.is_empty() {
                return Ok(());
            }
            bail!("input broker inject reply carried frames without a batch id");
        }
        if frames.len() > INPUT_BROKER_INJECT_BATCH_CAP {
            bail!(
                "input broker inject batch {batch_id} exceeded bounded frame cap: {} > {}",
                frames.len(),
                INPUT_BROKER_INJECT_BATCH_CAP
            );
        }
        if self.last_completed_batch_id == batch_id {
            // A lost acknowledgement response can replay a completed ID. The
            // server retains IDs, so acknowledge again without reinjection.
            return Ok(());
        }
        if let Some(active_batch_id) = self.active_batch_id {
            if active_batch_id != batch_id {
                bail!(
                    "input broker inject batch advanced while {active_batch_id} remained pending: received {batch_id}"
                );
            }
            if !frames.is_empty() {
                bail!("input broker replayed active batch {batch_id} while backpressure was set");
            }
            return Ok(());
        }
        if frames.is_empty() {
            bail!("input broker announced new inject batch {batch_id} without frames");
        }
        self.active_batch_id = Some(batch_id);
        self.frames.extend(frames);
        Ok(())
    }

    fn process(&mut self, injected_state: &mut InjectedInputState) -> BrokerInjectProgress {
        self.process_with(injected_state, apply_injected_input_events)
    }

    fn process_with<F>(
        &mut self,
        injected_state: &mut InjectedInputState,
        mut apply: F,
    ) -> BrokerInjectProgress
    where
        F: FnMut(&[core_input::InputEvent], &mut InjectedInputState) -> InputSendOutcome,
    {
        let mut progress = BrokerInjectProgress::default();
        while let Some(events) = self.frames.front().cloned() {
            let outcome = apply(&events, injected_state);
            if outcome.error.is_none() {
                self.frames.pop_front();
                progress.completed_frames = progress.completed_frames.saturating_add(1);
                continue;
            }
            self.frames
                .front_mut()
                .expect("front frame remains present")
                .clone_from(&outcome.remaining_events);
            progress.failed_attempts = progress.failed_attempts.saturating_add(1);
            break;
        }
        if self.frames.is_empty()
            && let Some(batch_id) = self.active_batch_id.take()
        {
            self.last_completed_batch_id = batch_id;
        }
        progress
    }
}

impl InjectedInputState {
    fn new(num_lock_state: WindowsNumLockState) -> Self {
        Self {
            windows_input: WindowsInputState::new(num_lock_state),
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
        }
    }

    fn observe(&mut self, events: &[core_input::InputEvent]) {
        for event in events {
            match event {
                core_input::InputEvent::Key {
                    scan_code,
                    state,
                    semantics,
                } => match state {
                    core_input::KeyState::Down => {
                        if !self
                            .pressed_keys
                            .iter()
                            .any(|(pressed_scan_code, _)| pressed_scan_code == scan_code)
                        {
                            self.pressed_keys.push((*scan_code, *semantics));
                        }
                    }
                    core_input::KeyState::Up => {
                        self.pressed_keys
                            .retain(|(pressed_scan_code, _)| pressed_scan_code != scan_code);
                    }
                },
                core_input::InputEvent::MouseButton { button, state } => match state {
                    core_input::KeyState::Down => {
                        if !self.pressed_buttons.contains(button) {
                            self.pressed_buttons.push(*button);
                        }
                    }
                    core_input::KeyState::Up => {
                        self.pressed_buttons.retain(|pressed| pressed != button);
                    }
                },
                core_input::InputEvent::MouseMove { .. }
                | core_input::InputEvent::MouseMoveAbsolute { .. }
                | core_input::InputEvent::MouseWheel { .. } => {}
            }
        }
    }

    fn drain_release_events(&mut self) -> Vec<core_input::InputEvent> {
        self.pressed_buttons.sort_by_key(|button| match button {
            core_input::MouseButton::Left => 0,
            core_input::MouseButton::Right => 1,
            core_input::MouseButton::Middle => 2,
            core_input::MouseButton::X1 => 3,
            core_input::MouseButton::X2 => 4,
        });
        self.pressed_keys
            .sort_unstable_by_key(|(scan_code, _)| *scan_code);
        let mut releases = self
            .pressed_buttons
            .drain(..)
            .map(|button| core_input::InputEvent::MouseButton {
                button,
                state: core_input::KeyState::Up,
            })
            .collect::<Vec<_>>();
        releases.extend(
            self.pressed_keys
                .drain(..)
                .map(|(scan_code, semantics)| core_input::InputEvent::Key {
                    scan_code,
                    state: core_input::KeyState::Up,
                    semantics,
                }),
        );
        releases
    }

    fn release_local(&mut self) -> Result<()> {
        let releases = self.drain_release_events();
        self.windows_input
            .send_events(&releases)
            .into_result()
            .map(|_| ())
    }
}

impl ClipboardBrokerState {
    fn should_read_sequence(&self, sequence: u64) -> bool {
        self.last_sequence != Some(sequence)
            && self
                .pending_local_payload
                .as_ref()
                .is_none_or(|pending| pending.sequence != sequence)
    }

    fn stage_local_read(
        &mut self,
        sequence: u64,
        payload: Option<core_clipboard::ClipboardPayload>,
    ) -> Option<core_clipboard::ClipboardPolicyError> {
        self.unread_newer_sequence = None;
        let Some(payload) = payload else {
            self.pending_local_payload = None;
            self.last_sequence = Some(sequence);
            return None;
        };

        if let Err(error) =
            core_clipboard::validate_payload(core_clipboard::ClipboardPolicy::default(), &payload)
        {
            // Policy rejection is deterministic for this clipboard sequence.
            // Consume it locally so an oversized payload cannot become a
            // ResourceExhausted retry loop at the tonic boundary.
            self.pending_local_payload = None;
            self.last_sequence = Some(sequence);
            return Some(error);
        }

        self.pending_local_payload = Some(PendingLocalClipboardPayload { sequence, payload });
        None
    }

    fn stage_local_read_result(
        &mut self,
        sequence: u64,
        result: Result<Option<core_clipboard::ClipboardPayload>>,
    ) -> Result<Option<core_clipboard::ClipboardPolicyError>> {
        match result {
            Ok(payload) => Ok(self.stage_local_read(sequence, payload)),
            Err(error) => {
                self.unread_newer_sequence = Some(sequence);
                Err(error)
            }
        }
    }

    fn local_payload_for_request(&self) -> Option<core_clipboard::ClipboardPayload> {
        if self.unread_newer_sequence.is_some() {
            return None;
        }
        self.pending_local_payload
            .as_ref()
            .map(|pending| pending.payload.clone())
    }

    fn local_sequence_for_request(&self) -> Option<u64> {
        if self.unread_newer_sequence.is_some() {
            return None;
        }
        self.pending_local_payload
            .as_ref()
            .map(|pending| pending.sequence)
    }

    fn apply_report_for_request(&self) -> Option<ClipboardBrokerApplyReport> {
        self.apply_report.clone()
    }

    fn mark_apply_report_accepted(&mut self) {
        self.apply_report = None;
    }

    fn mark_local_payload_consumed(&mut self) {
        if let Some(pending) = self.pending_local_payload.take() {
            self.last_sequence = Some(pending.sequence);
        }
    }

    fn stage_apply_report(&mut self, report: ClipboardBrokerApplyReport) {
        self.apply_report = Some(report);
    }
}

impl SafetyUnlockReconciler {
    fn observe(&mut self, actions: Vec<HookControlAction>) {
        for action in actions {
            match action {
                HookControlAction::EscapeUnlock => {
                    self.pending_escape_count = self.pending_escape_count.saturating_add(1);
                }
                HookControlAction::LeaseExpiredUnlock => {
                    self.pending_lease_expired_count =
                        self.pending_lease_expired_count.saturating_add(1);
                }
                HookControlAction::DetectorUnavailableUnlock => {
                    self.pending_detector_unavailable_count = self
                        .pending_detector_unavailable_count
                        .saturating_add(1);
                }
            }
            self.waiting_for_daemon_release = true;
        }
    }

    fn observe_lock_update(
        &mut self,
        requested_active: bool,
        result: &Result<bool>,
        followup_actions: Vec<HookControlAction>,
    ) {
        let observed_action = !followup_actions.is_empty();
        self.observe(followup_actions);
        if requested_active && !matches!(result, Ok(true)) && !observed_action {
            // An unavailable detector can refuse activation while the hook is
            // already unlocked, so there is no unlock transition for the
            // platform runtime to publish. Synthesize the same reconciliation
            // boundary here and keep local events suppressed until the daemon
            // confirms capture ownership has been cleared.
            self.pending_detector_unavailable_count =
                self.pending_detector_unavailable_count.saturating_add(1);
            self.waiting_for_daemon_release = true;
        }
    }

    fn report_counts(&self) -> (u32, u32, u32) {
        (
            self.pending_escape_count,
            self.pending_lease_expired_count,
            self.pending_detector_unavailable_count,
        )
    }

    fn mark_report_delivered(&mut self) {
        self.pending_escape_count = 0;
        self.pending_lease_expired_count = 0;
        self.pending_detector_unavailable_count = 0;
    }

    fn should_forward_captured_events(&self) -> bool {
        !self.waiting_for_daemon_release
    }

    fn lock_should_be_active(&mut self, daemon_lock: bool, daemon_capture: bool) -> bool {
        if self.waiting_for_daemon_release {
            if !daemon_lock && !daemon_capture {
                self.waiting_for_daemon_release = false;
            }
            return false;
        }
        daemon_lock && daemon_capture
    }
}

impl BrokerCaptureForwardingGate {
    fn prepare_batch(
        &mut self,
        observed_events: Vec<core_input::InputEvent>,
        lock_active: bool,
        safety_allows_forwarding: bool,
    ) -> BrokerCaptureBatch {
        if !safety_allows_forwarding {
            self.daemon_authorized = false;
            self.staged_events.clear();
            return BrokerCaptureBatch::default();
        }

        if !lock_active {
            self.daemon_authorized = false;
            self.staged_events.clear();
            let (dx, dy) = observed_events.iter().fold((0i32, 0i32), |(dx, dy), event| {
                if let core_input::InputEvent::MouseMove {
                    dx: event_dx,
                    dy: event_dy,
                } = event
                {
                    (dx.saturating_add(*event_dx), dy.saturating_add(*event_dy))
                } else {
                    (dx, dy)
                }
            });
            return BrokerCaptureBatch {
                captured_events: Vec::new(),
                handoff_probe: (dx != 0 || dy != 0).then_some((dx, dy)),
            };
        }

        let observed_count = observed_events.len();
        let available = INPUT_BROKER_CAPTURE_STAGING_CAP.saturating_sub(self.staged_events.len());
        let accepted = observed_count.min(available);
        self.staged_events
            .extend(observed_events.into_iter().take(accepted));
        self.dropped_event_count = self
            .dropped_event_count
            .saturating_add((observed_count.saturating_sub(accepted)) as u64);

        if !self.daemon_authorized {
            return BrokerCaptureBatch::default();
        }

        BrokerCaptureBatch {
            captured_events: std::mem::take(&mut self.staged_events),
            handoff_probe: None,
        }
    }

    fn observe_daemon_authorization(
        &mut self,
        daemon_authorized: bool,
        lock_active: bool,
        safety_allows_forwarding: bool,
    ) {
        self.daemon_authorized =
            daemon_authorized && lock_active && safety_allows_forwarding;
        if !lock_active || !safety_allows_forwarding {
            self.staged_events.clear();
        }
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        std::mem::take(&mut self.dropped_event_count)
    }
}

enum BrokerSessionEnd {
    NotNeeded,
    Detached,
    Shutdown,
}

fn validate_input_broker_attach_revision(attach: &InputBrokerAttachReply) -> Result<()> {
    if attach.protocol_revision == ipc_api::INPUT_BROKER_PROTOCOL_REVISION {
        return Ok(());
    }
    bail!(
        "input broker protocol mismatch: daemon={} expected={}",
        attach.protocol_revision,
        ipc_api::INPUT_BROKER_PROTOCOL_REVISION
    )
}

fn mismatched_attach_cleanup_token(attach: &InputBrokerAttachReply) -> Option<&str> {
    (attach.protocol_revision != ipc_api::INPUT_BROKER_PROTOCOL_REVISION
        && attach.accepted
        && !attach.broker_token.is_empty())
    .then_some(attach.broker_token.as_str())
}

const INPUT_BROKER_CLEANUP_RPC_TIMEOUT: Duration = Duration::from_millis(500);
const INPUT_BROKER_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct InputBrokerShutdownSignal {
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl InputBrokerShutdownSignal {
    pub(super) fn request(&self) {
        // This is intentionally first and IPC-independent. The rest of the
        // supervisor may be stalled, but local input must already be free.
        let _ = platform_windows::input::release_active_hook_lock();
        self.shutdown_tx.send_replace(true);
    }
}

pub(super) struct InputBrokerSupervisor {
    shutdown: InputBrokerShutdownSignal,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputBrokerSupervisor {
    pub(super) fn shutdown_signal(&self) -> InputBrokerShutdownSignal {
        self.shutdown.clone()
    }

    pub(super) fn shutdown(&mut self) {
        self.shutdown.request();
        let Some(thread) = self.thread.take() else {
            return;
        };

        let deadline = Instant::now() + INPUT_BROKER_SHUTDOWN_JOIN_TIMEOUT;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            eprintln!(
                "boundless_input_broker_shutdown=join_timeout timeout_ms={}",
                INPUT_BROKER_SHUTDOWN_JOIN_TIMEOUT.as_millis()
            );
            // Dropping the handle detaches the still-cancelled thread. The
            // tray process is already exiting; never wedge UI shutdown here.
        }
    }
}

impl Drop for InputBrokerSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(super) fn spawn_input_broker_supervisor(
    endpoint: String,
) -> Result<InputBrokerSupervisor> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let thread = std::thread::Builder::new()
        .name("boundless-input-broker".to_string())
        .spawn(move || input_broker_supervisor_loop(endpoint, shutdown_rx))
        .context("spawn input broker supervisor")?;
    Ok(InputBrokerSupervisor {
        shutdown: InputBrokerShutdownSignal { shutdown_tx },
        thread: Some(thread),
    })
}

fn input_broker_supervisor_loop(
    endpoint: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("boundless input broker runtime creation failed: {error:#}");
            return;
        }
    };
    runtime.block_on(async move {
        loop {
            match run_input_broker_session(&endpoint, shutdown_rx.clone()).await {
                Ok(BrokerSessionEnd::Shutdown) => break,
                Ok(BrokerSessionEnd::NotNeeded) | Ok(BrokerSessionEnd::Detached) => {}
                Err(error) => eprintln!("boundless input broker session ended: {error:#}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(INPUT_BROKER_SUPERVISOR_RETRY) => {}
                _ = wait_for_broker_shutdown(&mut shutdown_rx) => break,
            }
        }
    });
}

async fn wait_for_broker_shutdown(shutdown_rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *shutdown_rx.borrow() {
        return;
    }
    while shutdown_rx.changed().await.is_ok() {
        if *shutdown_rx.borrow() {
            return;
        }
    }
}

async fn run_input_broker_session(
    endpoint: &str,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<BrokerSessionEnd> {
    // Fail closed: never broker interactive input from a non-interactive
    // (session 0) process, even if a daemon would accept it.
    if !current_process_can_use_interactive_input().unwrap_or(false) {
        return Ok(BrokerSessionEnd::NotNeeded);
    }

    let mut client = tokio::select! {
        result = connect_control_plane(endpoint) => result?,
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => return Ok(BrokerSessionEnd::Shutdown),
    };
    let backend_mode = tokio::select! {
        result = client.get_ui_snapshot(Empty {}) => result?,
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => return Ok(BrokerSessionEnd::Shutdown),
    }
    .into_inner()
    .input_runtime
    .map(|runtime| runtime.capture_backend_mode)
    .unwrap_or_default();
    if backend_mode != INPUT_BROKER_SERVICE_UNSUPPORTED_MODE {
        // A user-session daemon owns capture/injection directly; a broker
        // would double-capture. Stay detached and re-check later.
        return Ok(BrokerSessionEnd::NotNeeded);
    }

    let mut pump =
        HookInputPump::start(|_source| {}).context("install user-session capture hooks")?;
    pump.enable_lock_lease(INPUT_BROKER_LOCK_LEASE)
        .context("enable fail-open input broker lock lease")?;

    // The daemon authorizes this attach against the verified pipe client
    // identity (our process token SID and session), not anything we send.
    let attach = tokio::select! {
        result = client.attach_input_broker(InputBrokerAttachRequest {
                broker_version: env!("CARGO_PKG_VERSION").to_string(),
                lock_supported: true,
                protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
            }) => result?,
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => return Ok(BrokerSessionEnd::Shutdown),
    }
    .into_inner();
    if validate_input_broker_attach_revision(&attach).is_err() {
        if let Some(stale_token) = mismatched_attach_cleanup_token(&attach) {
            // An older daemon ignores the request revision and may issue a
            // token in an unversioned reply. Clean that token before refusing
            // to enter the lock/exchange loop.
            let _ = tokio::time::timeout(
                INPUT_BROKER_CLEANUP_RPC_TIMEOUT,
                client.detach_input_broker(InputBrokerDetachRequest {
                    broker_token: stale_token.to_string(),
                }),
            )
            .await;
        }
        validate_input_broker_attach_revision(&attach)?;
    }
    if !attach.accepted {
        eprintln!("boundless input broker attach rejected: {}", attach.message);
        return Ok(BrokerSessionEnd::NotNeeded);
    }
    let broker_token = attach.broker_token;

    let mut input_client = client.clone();
    let clipboard_task = tokio::spawn(clipboard_broker_supervisor_loop(
        client.clone(),
        broker_token.clone(),
    ));
    let mut injected_state = InjectedInputState::new(pump.num_lock_state());
    let (loop_result, session_end) = tokio::select! {
        result = input_broker_exchange_loop(
            &mut input_client,
            &broker_token,
            &mut pump,
            &mut injected_state,
        ) => (result, BrokerSessionEnd::Detached),
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => (Ok(()), BrokerSessionEnd::Shutdown),
    };
    // Clipboard failures are supervised independently and never select the
    // input path out of service. Conversely, ending the input session cancels
    // its clipboard worker before token cleanup.
    clipboard_task.abort();
    let _ = clipboard_task.await;

    // Unlock and locally release injected state before any cleanup IPC.
    let _ = pump.set_lock_active(false);
    let _ = injected_state.release_local();

    // Do not submit synthetic captured releases through the ordinary exchange:
    // that would consume the daemon relay's authoritative pressed-state before
    // it can forward releases to the captured peer. Authorized detach owns the
    // release-then-clear operation as one server-side lifecycle transition.
    let _ = pump.drain_release_events();
    let _ = tokio::time::timeout(
        INPUT_BROKER_CLEANUP_RPC_TIMEOUT,
        client.detach_input_broker(InputBrokerDetachRequest {
            broker_token,
        }),
    )
    .await;

    loop_result.map(|_| session_end)
}

async fn input_broker_exchange_loop(
    client: &mut ControlPlaneServiceClient<Channel>,
    broker_token: &str,
    pump: &mut HookInputPump,
    injected_state: &mut InjectedInputState,
) -> Result<()> {
    let mut injected_frame_count = 0u32;
    let mut inject_failure_count = 0u32;
    let mut safety_unlock = SafetyUnlockReconciler::default();
    let mut capture_forwarding = BrokerCaptureForwardingGate::default();
    let mut inject_batches = BrokerInjectBatchState::default();

    loop {
        let inject_progress = inject_batches.process(injected_state);
        injected_frame_count = injected_frame_count
            .saturating_add(inject_progress.completed_frames);
        inject_failure_count = inject_failure_count
            .saturating_add(inject_progress.failed_attempts);
        safety_unlock.observe(pump.drain_control_actions());
        let observed_events = pump.poll_events();
        let observed_event_count = observed_events.len();
        let capture_batch = capture_forwarding.prepare_batch(
            observed_events,
            pump.lock_active(),
            safety_unlock.should_forward_captured_events(),
        );
        let captured = capture_batch.captured_events;
        let handoff_probe = capture_batch.handoff_probe;
        let cursor = pump
            .cursor_position()
            .or_else(|| platform_windows::input::cursor_position().ok().flatten());
        let bounds = virtual_screen_bounds();
        let wheel_sources = pump.take_wheel_source_counts();
        let (escape_unlock_count, lease_expired_unlock_count, detector_unavailable_unlock_count) =
            safety_unlock.report_counts();
        let dropped_event_count = pump
            .take_dropped_event_count()
            .saturating_add(capture_forwarding.take_dropped_event_count());

        let reply = client
            .exchange_input_broker(InputBrokerExchangeRequest {
                broker_token: broker_token.to_string(),
                captured_events: broker_events_from_input_events(&captured),
                cursor_valid: cursor.is_some(),
                cursor_x: cursor.map(|(x, _)| x).unwrap_or_default(),
                cursor_y: cursor.map(|(_, y)| y).unwrap_or_default(),
                bounds_valid: bounds.is_some(),
                bounds_left: bounds.map(|bounds| bounds.0).unwrap_or_default(),
                bounds_top: bounds.map(|bounds| bounds.1).unwrap_or_default(),
                bounds_right: bounds.map(|bounds| bounds.2).unwrap_or_default(),
                bounds_bottom: bounds.map(|bounds| bounds.3).unwrap_or_default(),
                escape_unlock_count,
                lease_expired_unlock_count,
                detector_unavailable_unlock_count,
                handoff_probe_dx: handoff_probe.map(|(dx, _)| dx).unwrap_or_default(),
                handoff_probe_dy: handoff_probe.map(|(_, dy)| dy).unwrap_or_default(),
                lock_active: pump.lock_active(),
                dropped_event_count,
                injected_frame_count,
                inject_failure_count,
                inject_backpressure: inject_batches.backpressure_active(),
                acked_inject_batch_id: inject_batches.acked_batch_id(),
                raw_device_wheel_event_count: wheel_sources.raw_device,
                raw_system_wheel_event_count: wheel_sources.raw_system,
                hook_wheel_event_count: wheel_sources.hook,
            })
            .await?
            .into_inner();
        if !reply.accepted {
            bail!("input broker exchange rejected: {}", reply.message);
        }
        pump.renew_lock_lease();
        safety_unlock.mark_report_delivered();
        // A gesture or watchdog expiry can happen while the exchange future is
        // in flight. Drain again before considering the daemon's lock reply so
        // a stale response can never re-lock local input.
        let relock_generation = pump.safety_unlock_generation();
        safety_unlock.observe(pump.drain_control_actions());
        injected_frame_count = 0;
        inject_failure_count = 0;

        let local_lock_should_be_active = safety_unlock
            .lock_should_be_active(reply.lock_should_be_active, reply.capture_active);
        if pump.lock_active() != local_lock_should_be_active {
            let result = if local_lock_should_be_active {
                pump.set_lock_active_if_safety_generation(true, relock_generation)
            } else {
                pump.set_lock_active(false)
            };
            if local_lock_should_be_active && result.is_err() {
                let _ = pump.set_lock_active(false);
            }
            safety_unlock.observe_lock_update(
                local_lock_should_be_active,
                &result,
                pump.drain_control_actions(),
            );
            if local_lock_should_be_active && matches!(result, Ok(false)) {
                eprintln!(
                    "boundless input broker local lock activation refused; clearing daemon capture"
                );
            } else if let Err(error) = result {
                eprintln!(
                    "boundless input broker failed to update local input lock: {error:#}"
                );
            }
        }
        capture_forwarding.observe_daemon_authorization(
            reply.capture_forwarding_authorized,
            pump.lock_active(),
            safety_unlock.should_forward_captured_events(),
        );

        let had_inject_frames = reply.inject_batch_id != 0;
        let mut decoded_inject_frames = Vec::with_capacity(reply.inject_frames.len());
        for frame in &reply.inject_frames {
            let (events, undecodable) = input_events_from_broker_events(&frame.events);
            if undecodable > 0 {
                bail!(
                    "input broker inject batch {} frame {} contained {undecodable} undecodable events",
                    reply.inject_batch_id,
                    frame.sequence
                );
            }
            decoded_inject_frames.push(events);
        }
        inject_batches.accept_batch(reply.inject_batch_id, decoded_inject_frames)?;

        let poll = if reply.capture_active || had_inject_frames || observed_event_count > 0 {
            INPUT_BROKER_ACTIVE_POLL
        } else {
            INPUT_BROKER_IDLE_POLL
        };
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod input_broker_tests {
    use super::*;

    #[test]
    fn unversioned_accepted_attach_is_rejected_and_scheduled_for_cleanup() {
        let old_daemon_reply = InputBrokerAttachReply {
            accepted: true,
            broker_token: "stale-token".to_string(),
            message: String::new(),
            protocol_revision: 0,
        };

        let error = validate_input_broker_attach_revision(&old_daemon_reply)
            .expect_err("new tray must reject an unversioned daemon");
        assert!(error.to_string().contains("daemon=0"));
        assert_eq!(
            mismatched_attach_cleanup_token(&old_daemon_reply),
            Some("stale-token")
        );

        let current = InputBrokerAttachReply {
            protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
            ..old_daemon_reply
        };
        validate_input_broker_attach_revision(&current).expect("current daemon revision");
        assert_eq!(mismatched_attach_cleanup_token(&current), None);
    }

    #[test]
    fn inject_batch_retries_exact_suffix_before_later_frames_and_then_acks() {
        let key_down = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let mouse_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let key_up = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_batch(
                7,
                vec![
                    vec![key_down.clone(), mouse_down.clone()],
                    vec![key_up.clone()],
                ],
            )
            .expect("stage batch");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        let mut attempts = Vec::new();
        let first = batch.process_with(&mut injected, |events, state| {
            attempts.push(events.to_vec());
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: vec![mouse_down.clone()],
                    error: Some(anyhow::anyhow!("scripted suffix failure")),
                },
            )
        });
        assert_eq!(
            first,
            BrokerInjectProgress {
                completed_frames: 0,
                failed_attempts: 1,
            }
        );
        assert!(batch.backpressure_active());
        assert_eq!(batch.acked_batch_id(), 0);

        let second = batch.process_with(&mut injected, |events, state| {
            attempts.push(events.to_vec());
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        });
        assert_eq!(
            second,
            BrokerInjectProgress {
                completed_frames: 2,
                failed_attempts: 0,
            }
        );
        assert_eq!(
            attempts,
            vec![
                vec![key_down, mouse_down.clone()],
                vec![mouse_down],
                vec![key_up],
            ]
        );
        assert!(!batch.backpressure_active());
        assert_eq!(batch.acked_batch_id(), 7);

        batch
            .accept_batch(7, vec![vec![core_input::InputEvent::MouseMove { dx: 1, dy: 0 }]])
            .expect("completed batch replay is deduplicated");
        assert!(!batch.backpressure_active());
    }

    #[test]
    fn safety_unlock_suppresses_stale_relock_until_daemon_reconciles() {
        let mut state = SafetyUnlockReconciler::default();
        state.observe(vec![HookControlAction::EscapeUnlock]);

        assert_eq!(state.report_counts(), (1, 0, 0));
        assert!(!state.should_forward_captured_events());
        state.mark_report_delivered();
        assert_eq!(state.report_counts(), (0, 0, 0));
        assert!(
            !state.lock_should_be_active(true, true),
            "the reply racing the escape must not re-lock local input"
        );
        assert!(!state.should_forward_captured_events());
        assert!(!state.lock_should_be_active(false, false));
        assert!(state.should_forward_captured_events());
        assert!(state.lock_should_be_active(true, true));
    }

    #[test]
    fn safety_unlocks_during_exchange_remain_pending_for_next_report() {
        let mut state = SafetyUnlockReconciler::default();
        state.observe(vec![HookControlAction::EscapeUnlock]);
        let submitted = state.report_counts();
        assert_eq!(submitted, (1, 0, 0));
        state.mark_report_delivered();

        state.observe(vec![HookControlAction::LeaseExpiredUnlock]);
        assert_eq!(state.report_counts(), (0, 1, 0));
        assert!(!state.lock_should_be_active(true, true));
    }

    #[test]
    fn refused_local_lock_becomes_a_safety_unlock_reconciliation() {
        let mut state = SafetyUnlockReconciler::default();
        let result = Ok(false);

        state.observe_lock_update(true, &result, Vec::new());

        assert_eq!(state.report_counts(), (0, 0, 1));
        assert!(!state.should_forward_captured_events());
        assert!(
            !state.lock_should_be_active(true, true),
            "the stale daemon reply must not keep capture active after local refusal"
        );
        state.mark_report_delivered();
        assert!(!state.lock_should_be_active(false, false));
        assert!(state.should_forward_captured_events());
    }

    #[test]
    fn safety_unlock_reports_preserve_each_platform_cause() {
        let mut state = SafetyUnlockReconciler::default();
        state.observe(vec![
            HookControlAction::EscapeUnlock,
            HookControlAction::LeaseExpiredUnlock,
            HookControlAction::DetectorUnavailableUnlock,
        ]);

        assert_eq!(state.report_counts(), (1, 1, 1));
    }

    #[test]
    fn captured_events_wait_for_lock_and_daemon_authorization() {
        let mut gate = BrokerCaptureForwardingGate::default();
        let local_batch = gate.prepare_batch(
            vec![
                core_input::InputEvent::MouseMove { dx: 7, dy: -2 },
                core_input::InputEvent::Key {
                    scan_code: 30,
                    state: core_input::KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                },
            ],
            false,
            true,
        );
        assert!(local_batch.captured_events.is_empty());
        assert_eq!(local_batch.handoff_probe, Some((7, -2)));

        let awaiting_ack = gate.prepare_batch(
            vec![core_input::InputEvent::Key {
                scan_code: 31,
                state: core_input::KeyState::Down,
                semantics: core_input::KeySemantics::Physical,
            }],
            true,
            true,
        );
        assert!(awaiting_ack.captured_events.is_empty());
        gate.observe_daemon_authorization(true, true, true);

        let authorized = gate.prepare_batch(
            vec![core_input::InputEvent::Key {
                scan_code: 31,
                state: core_input::KeyState::Up,
                semantics: core_input::KeySemantics::Physical,
            }],
            true,
            true,
        );
        assert_eq!(
            authorized.captured_events,
            vec![
                core_input::InputEvent::Key {
                    scan_code: 31,
                    state: core_input::KeyState::Down,
                    semantics: core_input::KeySemantics::Physical,
                },
                core_input::InputEvent::Key {
                    scan_code: 31,
                    state: core_input::KeyState::Up,
                    semantics: core_input::KeySemantics::Physical,
                },
            ]
        );
    }

    #[test]
    fn lock_refusal_discards_staged_pre_ack_events() {
        let mut gate = BrokerCaptureForwardingGate::default();
        let awaiting_ack = gate.prepare_batch(
            vec![core_input::InputEvent::Key {
                scan_code: 30,
                state: core_input::KeyState::Down,
                semantics: core_input::KeySemantics::Physical,
            }],
            true,
            true,
        );
        assert!(awaiting_ack.captured_events.is_empty());

        gate.observe_daemon_authorization(false, false, false);
        gate.observe_daemon_authorization(true, true, true);
        let after_recovery = gate.prepare_batch(Vec::new(), true, true);

        assert!(
            after_recovery.captured_events.is_empty(),
            "a refused generation must never leak its staged batch after later recovery"
        );
    }

    #[test]
    fn clipboard_payload_survives_transient_read_and_rpc_failures() {
        let mut state = ClipboardBrokerState::default();
        assert!(state.should_read_sequence(41));

        let error = state
            .stage_local_read_result(41, Err(anyhow::anyhow!("clipboard busy")))
            .expect_err("transient read failure must surface");
        assert!(error.to_string().contains("clipboard busy"));
        assert!(
            state.should_read_sequence(41),
            "a failed read must leave the sequence eligible for retry"
        );

        let rejection = state
            .stage_local_read_result(
                41,
                Ok(Some(core_clipboard::ClipboardPayload::Text(
                    "retry me".to_string(),
                ))),
            )
            .expect("successful retry");
        assert!(
            rejection.is_none(),
            "policy-valid payload must not be rejected"
        );
        assert_eq!(
            state.local_payload_for_request(),
            Some(core_clipboard::ClipboardPayload::Text(
                "retry me".to_string()
            ))
        );
        assert_eq!(
            state.local_payload_for_request(),
            Some(core_clipboard::ClipboardPayload::Text(
                "retry me".to_string()
            )),
            "a transport failure must leave the same payload pending"
        );
        assert_eq!(state.local_sequence_for_request(), Some(41));

        state.mark_local_payload_consumed();
        assert!(state.local_payload_for_request().is_none());
        assert!(state.local_sequence_for_request().is_none());
        assert!(
            !state.should_read_sequence(41),
            "only an accepted app reply consumes a staged payload sequence"
        );
    }

    #[test]
    fn oversized_clipboard_payload_is_consumed_without_staging_rpc_retry() {
        let mut state = ClipboardBrokerState::default();
        let policy = core_clipboard::ClipboardPolicy::default();
        let rejection = state
            .stage_local_read_result(
                52,
                Ok(Some(core_clipboard::ClipboardPayload::Image(vec![
                    0;
                    policy.max_image_bytes + 1
                ]))),
            )
            .expect("deterministic local validation")
            .expect("oversize payload must be rejected");

        assert!(matches!(
            rejection,
            core_clipboard::ClipboardPolicyError::ImageTooLarge { .. }
        ));
        assert!(state.local_payload_for_request().is_none());
        assert!(
            !state.should_read_sequence(52),
            "an unchanged oversize sequence must not retry toward tonic"
        );
        assert!(state.should_read_sequence(53));
    }

    #[test]
    fn newer_user_copy_supersedes_pending_payload_before_remote_apply() {
        let mut state = ClipboardBrokerState::default();
        assert!(state
            .stage_local_read_result(
                70,
                Ok(Some(core_clipboard::ClipboardPayload::Text(
                    "old pending".to_string(),
                ))),
            )
            .expect("stage old payload")
            .is_none());

        assert!(
            state.should_read_sequence(71),
            "a pending retry must not hide a newer clipboard sequence"
        );
        state
            .stage_local_read_result(71, Err(anyhow::anyhow!("clipboard temporarily busy")))
            .expect_err("newer sequence read should remain pending");
        assert!(state.local_payload_for_request().is_none());
        assert!(state.local_sequence_for_request().is_none());
        assert!(should_defer_remote_payload(
            &ClipboardPollOutcome {
                suppress_remote_apply: true,
                error: None,
            },
            false,
            ClipboardBrokerLocalPayloadDisposition::NotSubmitted,
        ));
        assert!(
            state.should_read_sequence(71),
            "the unread newer sequence must be retried"
        );
        assert!(state
            .stage_local_read_result(
                71,
                Ok(Some(core_clipboard::ClipboardPayload::Text(
                    "new user copy".to_string(),
                ))),
            )
            .expect("stage newer payload")
            .is_none());
        assert_eq!(
            state.local_payload_for_request(),
            Some(core_clipboard::ClipboardPayload::Text(
                "new user copy".to_string()
            )),
            "the request must carry the newest user copy, never the stale retry"
        );
        assert_eq!(state.local_sequence_for_request(), Some(71));
    }

    #[test]
    fn transient_app_rejection_retries_but_deterministic_rejection_consumes() {
        let mut state = ClipboardBrokerState::default();
        assert!(state
            .stage_local_read_result(
                81,
                Ok(Some(core_clipboard::ClipboardPayload::Text(
                    "pending".to_string(),
                ))),
            )
            .expect("stage payload")
            .is_none());

        let transient = reconcile_local_payload_disposition(
            &mut state,
            true,
            ClipboardBrokerLocalPayloadDisposition::TransientRejected,
            "temporary queue failure",
        );
        assert!(transient.is_some());
        assert_eq!(
            state.local_payload_for_request(),
            Some(core_clipboard::ClipboardPayload::Text("pending".to_string()))
        );

        let deterministic = reconcile_local_payload_disposition(
            &mut state,
            true,
            ClipboardBrokerLocalPayloadDisposition::DeterministicRejected,
            "invalid payload",
        );
        assert!(deterministic.is_none());
        assert!(state.local_payload_for_request().is_none());
        assert!(!state.should_read_sequence(81));
    }

    #[test]
    fn clipboard_apply_report_is_retained_until_exchange_is_accepted() {
        let mut state = ClipboardBrokerState::default();
        state.stage_apply_report(ClipboardBrokerApplyReport {
            source_peer_id: "peer-a".to_string(),
            hash: "hash-a".to_string(),
            applied: true,
            message: String::new(),
        });

        assert_eq!(
            state
                .apply_report_for_request()
                .expect("first attempt")
                .hash,
            "hash-a"
        );
        assert_eq!(
            state
                .apply_report_for_request()
                .expect("retry after transport error")
                .hash,
            "hash-a"
        );
        state.mark_apply_report_accepted();
        assert!(state.apply_report_for_request().is_none());
    }

    #[test]
    fn injected_state_synthesizes_releases_for_shutdown() {
        let mut state = InjectedInputState::new(WindowsNumLockState::new(false));
        state.observe(&[
            core_input::InputEvent::Key {
                scan_code: 30,
                state: core_input::KeyState::Down,
                semantics: core_input::KeySemantics::Physical,
            },
            core_input::InputEvent::MouseButton {
                button: core_input::MouseButton::Left,
                state: core_input::KeyState::Down,
            },
            core_input::InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: 120,
            },
        ]);

        assert_eq!(
            state.drain_release_events(),
            vec![
                core_input::InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: core_input::KeyState::Up,
                },
                core_input::InputEvent::Key {
                    scan_code: 30,
                    state: core_input::KeyState::Up,
                    semantics: core_input::KeySemantics::Physical,
                },
            ]
        );
        assert!(state.drain_release_events().is_empty());
    }

    #[test]
    fn injected_shutdown_release_keeps_first_down_semantics_across_repeat() {
        let mut state = InjectedInputState::new(WindowsNumLockState::new(false));
        let first_down = core_input::KeySemantics::Windows {
            virtual_key: 0x61,
            num_lock_on: true,
        };
        let repeat_after_toggle = core_input::KeySemantics::Windows {
            virtual_key: 0x23,
            num_lock_on: false,
        };
        state.observe(&[
            core_input::InputEvent::Key {
                scan_code: 0x4F,
                state: core_input::KeyState::Down,
                semantics: first_down,
            },
            core_input::InputEvent::Key {
                scan_code: 0x4F,
                state: core_input::KeyState::Down,
                semantics: repeat_after_toggle,
            },
        ]);

        assert_eq!(
            state.drain_release_events(),
            vec![core_input::InputEvent::Key {
                scan_code: 0x4F,
                state: core_input::KeyState::Up,
                semantics: first_down,
            }]
        );
    }

    fn assert_partial_injection_tracks_only_committed_prefix(
        events: Vec<core_input::InputEvent>,
        expected_releases: Vec<core_input::InputEvent>,
    ) {
        let mut state = InjectedInputState::new(WindowsNumLockState::new(false));
        let mut calls = 0usize;
        let outcome = state
            .windows_input
            .send_events_with_sender(&events, |records| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(records.len(), 2);
                        Ok(1)
                    }
                    2 => Err(anyhow::anyhow!("scripted suffix failure")),
                    _ => panic!("unexpected SendInput attempt {calls}"),
                }
            });

        observe_injected_input_outcome(&events, &mut state, outcome)
            .into_result()
            .expect_err("partial injection must retain its failure");
        assert_eq!(
            state.drain_release_events(),
            expected_releases,
            "shutdown cleanup must release the committed prefix and ignore the failed suffix"
        );
    }

    #[test]
    fn partial_injection_tracks_committed_key_and_mouse_prefixes_for_shutdown() {
        let key_down = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let key_up = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mouse_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let mouse_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };

        assert_partial_injection_tracks_only_committed_prefix(
            vec![key_down.clone(), mouse_down.clone()],
            vec![key_up],
        );
        assert_partial_injection_tracks_only_committed_prefix(
            vec![mouse_down, key_down],
            vec![mouse_up],
        );
    }

    #[test]
    fn supervisor_shutdown_signal_joins_cooperative_worker() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let thread = std::thread::spawn(move || {
            while !*shutdown_rx.borrow() {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let mut supervisor = InputBrokerSupervisor {
            shutdown: InputBrokerShutdownSignal { shutdown_tx },
            thread: Some(thread),
        };

        let started = Instant::now();
        supervisor.shutdown();
        assert!(supervisor.thread.is_none());
        assert!(started.elapsed() < INPUT_BROKER_SHUTDOWN_JOIN_TIMEOUT);
    }
}

fn apply_injected_input_events(
    events: &[core_input::InputEvent],
    injected_state: &mut InjectedInputState,
) -> InputSendOutcome {
    let outcome = injected_state.windows_input.send_events(events);
    observe_injected_input_outcome(events, injected_state, outcome)
}

fn observe_injected_input_outcome(
    events: &[core_input::InputEvent],
    injected_state: &mut InjectedInputState,
    outcome: InputSendOutcome,
) -> InputSendOutcome {
    let committed_event_count = outcome.committed_event_count.min(events.len());
    injected_state.observe(&events[..committed_event_count]);
    outcome
}

async fn clipboard_broker_supervisor_loop(
    mut client: ControlPlaneServiceClient<Channel>,
    broker_token: String,
) {
    let mut state = ClipboardBrokerState::default();
    loop {
        let delay = match clipboard_broker_exchange_once(&mut client, &broker_token, &mut state).await
        {
            Ok(()) => CLIPBOARD_BROKER_POLL,
            Err(error) => {
                eprintln!("boundless clipboard broker exchange failed: {error:#}");
                CLIPBOARD_BROKER_RETRY
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn clipboard_broker_exchange_once(
    client: &mut ControlPlaneServiceClient<Channel>,
    broker_token: &str,
    state: &mut ClipboardBrokerState,
) -> Result<()> {
    // A busy local clipboard must not block apply reports or inbound payloads.
    // Preserve the unread sequence, complete this exchange, then surface the
    // local error so the supervisor uses its bounded retry delay.
    let poll = stage_clipboard_payload_if_changed(state).await;
    let local_payload = state.local_payload_for_request();
    let local_sequence = state.local_sequence_for_request();
    let local_payload_submitted = local_payload.is_some();
    let reply = client
        .exchange_clipboard_broker(ClipboardBrokerExchangeRequest {
            broker_token: broker_token.to_string(),
            local_payload: local_payload.map(clipboard_payload_to_proto),
            apply_report: state.apply_report_for_request(),
            local_sequence_valid: local_sequence.is_some(),
            local_sequence: local_sequence.unwrap_or_default(),
        })
        .await?
        .into_inner();
    if !reply.accepted {
        bail!("clipboard broker exchange rejected: {}", reply.message);
    }
    state.mark_apply_report_accepted();
    let local_disposition = ClipboardBrokerLocalPayloadDisposition::try_from(
        reply.local_payload_disposition,
    )
    .unwrap_or(ClipboardBrokerLocalPayloadDisposition::Unspecified);
    let local_disposition_error = reconcile_local_payload_disposition(
        state,
        local_payload_submitted,
        local_disposition,
        &reply.message,
    );
    if !reply.message.is_empty() {
        eprintln!("boundless clipboard broker: {}", reply.message);
    }

    if let Some(remote_payload) = reply.remote_payload {
        if should_defer_remote_payload(&poll, local_payload_submitted, local_disposition) {
            eprintln!(
                "boundless clipboard remote payload deferred because a newer local value is pending"
            );
        } else {
            let result = write_clipboard_payload(remote_payload).await;
            state.stage_apply_report(ClipboardBrokerApplyReport {
                source_peer_id: reply.remote_source_peer_id,
                hash: reply.remote_hash,
                applied: result.is_ok(),
                message: result
                    .err()
                    .map(|error| format!("{error:#}"))
                    .unwrap_or_default(),
            });
        }
    }

    if let Some(error) = poll.error {
        return Err(error).context("local clipboard read deferred");
    }
    if let Some(error) = local_disposition_error {
        return Err(error);
    }

    Ok(())
}

fn reconcile_local_payload_disposition(
    state: &mut ClipboardBrokerState,
    local_payload_submitted: bool,
    disposition: ClipboardBrokerLocalPayloadDisposition,
    message: &str,
) -> Option<anyhow::Error> {
    match disposition {
        ClipboardBrokerLocalPayloadDisposition::Accepted
        | ClipboardBrokerLocalPayloadDisposition::DeterministicRejected => {
            state.mark_local_payload_consumed();
            None
        }
        ClipboardBrokerLocalPayloadDisposition::TransientRejected => Some(anyhow::anyhow!(
            "daemon transiently rejected local clipboard payload: {}",
            message
        )),
        ClipboardBrokerLocalPayloadDisposition::NotSubmitted if !local_payload_submitted => None,
        ClipboardBrokerLocalPayloadDisposition::NotSubmitted
        | ClipboardBrokerLocalPayloadDisposition::Unspecified => {
            local_payload_submitted.then(|| {
                anyhow::anyhow!(
                    "daemon did not resolve submitted local clipboard payload: {}",
                    message
                )
            })
        }
    }
}

fn should_defer_remote_payload(
    poll: &ClipboardPollOutcome,
    local_payload_submitted: bool,
    disposition: ClipboardBrokerLocalPayloadDisposition,
) -> bool {
    poll.suppress_remote_apply
        || (local_payload_submitted
            && disposition != ClipboardBrokerLocalPayloadDisposition::DeterministicRejected)
        || disposition == ClipboardBrokerLocalPayloadDisposition::TransientRejected
}

async fn stage_clipboard_payload_if_changed(
    state: &mut ClipboardBrokerState,
) -> ClipboardPollOutcome {
    let sequence = match tokio::task::spawn_blocking(|| {
        let mut backend = WindowsClipboardBackend;
        backend.sequence_number()
    })
    .await
    .context("clipboard sequence task panicked")
    .and_then(|sequence| sequence.context("clipboard sequence unavailable"))
    {
        Ok(sequence) => sequence,
        Err(error) => {
            return ClipboardPollOutcome {
                suppress_remote_apply: true,
                error: Some(error),
            };
        }
    };
    if !state.should_read_sequence(sequence) {
        return ClipboardPollOutcome::default();
    }

    let payload_result = match tokio::task::spawn_blocking(|| {
        let mut backend = WindowsClipboardBackend;
        backend.read_payload()
    })
    .await
    .context("clipboard read task panicked")
    {
        Ok(result) => result,
        Err(error) => Err(error),
    };
    let error = match state.stage_local_read_result(sequence, payload_result) {
        Ok(Some(error)) => {
            eprintln!(
                "boundless clipboard local payload skipped sequence={sequence} reason={error}"
            );
            None
        }
        Ok(None) => None,
        Err(error) => Some(error),
    };
    ClipboardPollOutcome {
        suppress_remote_apply: true,
        error,
    }
}

async fn write_clipboard_payload(payload: ClipboardBrokerPayload) -> Result<()> {
    let payload = clipboard_payload_from_proto(payload).context("clipboard broker payload empty")?;
    tokio::task::spawn_blocking(move || {
        let mut backend = WindowsClipboardBackend;
        backend.write_payload(&payload)
    })
    .await
    .context("clipboard write task panicked")?
}

fn clipboard_payload_from_proto(
    payload: ClipboardBrokerPayload,
) -> Option<core_clipboard::ClipboardPayload> {
    match payload.payload? {
        clipboard_broker_payload::Payload::Text(text) => {
            Some(core_clipboard::ClipboardPayload::Text(text))
        }
        clipboard_broker_payload::Payload::ImageBmp(image_bmp) => {
            Some(core_clipboard::ClipboardPayload::Image(image_bmp))
        }
    }
}

fn clipboard_payload_to_proto(payload: core_clipboard::ClipboardPayload) -> ClipboardBrokerPayload {
    ClipboardBrokerPayload {
        payload: Some(match payload {
            core_clipboard::ClipboardPayload::Text(text) => {
                clipboard_broker_payload::Payload::Text(text)
            }
            core_clipboard::ClipboardPayload::Image(image_bmp) => {
                clipboard_broker_payload::Payload::ImageBmp(image_bmp)
            }
        }),
    }
}
