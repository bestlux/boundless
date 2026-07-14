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
    elevated_controller: Option<ElevatedInputController>,
    delivery_lane: InputDeliveryLane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputDeliveryLane {
    Direct,
    Elevated,
    Blocked,
}

#[derive(Debug, Default)]
struct BrokerInjectBatchState {
    delivery_epoch: Option<String>,
    active_batch_id: Option<u64>,
    active_authorization_generation: Option<u64>,
    frames: std::collections::VecDeque<Vec<core_input::InputEvent>>,
    last_completed_batch_id: u64,
    held_authorization_generation: Option<u64>,
    held_input_resume: Option<BrokerHeldInputResume>,
    pending_local_releases: Vec<core_input::InputEvent>,
    pending_cleanup_windows_input: Option<WindowsInputState>,
    failed_inject_batch_id: Option<u64>,
    pending_elevated_cleanup: Option<ElevatedInputController>,
    input_session_reset_required: bool,
}

#[derive(Debug)]
struct BrokerHeldInputResume {
    authorization_generation: u64,
    authorized_for_attempt: bool,
    intended_downs: Vec<core_input::InputEvent>,
    pending_restore: Vec<core_input::InputEvent>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BrokerInjectProgress {
    completed_frames: u32,
    failed_attempts: u32,
    requires_session_teardown: bool,
}

#[derive(Debug)]
struct ElevatedDeliveryUncertain;

impl std::fmt::Display for ElevatedDeliveryUncertain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("elevated input delivery outcome is uncertain")
    }
}

impl std::error::Error for ElevatedDeliveryUncertain {}

impl BrokerInjectBatchState {
    fn begin_delivery_epoch(&mut self, delivery_epoch: &str) -> Result<()> {
        if delivery_epoch.is_empty() {
            bail!("input broker attach omitted the daemon delivery epoch");
        }
        if self.delivery_epoch.as_deref() == Some(delivery_epoch) {
            return Ok(());
        }

        // A new daemon process can restart batch IDs from one. Receipts and
        // retained suffixes are meaningful only within the epoch that issued
        // them, so never acknowledge or apply them against another daemon.
        self.delivery_epoch = Some(delivery_epoch.to_string());
        self.active_batch_id = None;
        self.active_authorization_generation = None;
        self.frames.clear();
        self.last_completed_batch_id = 0;
        self.held_authorization_generation = None;
        self.held_input_resume = None;
        self.failed_inject_batch_id = None;
        self.input_session_reset_required = false;
        Ok(())
    }

    fn backpressure_active(&self) -> bool {
        self.active_batch_id.is_some()
    }

    fn acked_batch_id(&self) -> u64 {
        self.last_completed_batch_id
    }

    fn failed_batch_id(&self) -> u64 {
        self.failed_inject_batch_id.unwrap_or_default()
    }

    fn require_session_reset_for_elevated_recovery(&mut self) {
        if self.failed_inject_batch_id.is_none() {
            self.failed_inject_batch_id = self.active_batch_id;
        }
        self.input_session_reset_required = true;
    }

    fn accept_reply(
        &mut self,
        batch_id: u64,
        cancelled: bool,
        frames: Vec<Vec<core_input::InputEvent>>,
        authorization_generation: u64,
    ) -> Result<bool> {
        if !cancelled {
            self.accept_authorized_batch(batch_id, frames, authorization_generation)?;
            return Ok(false);
        }
        if batch_id == 0 {
            bail!("input broker cancelled an inject batch without a batch id");
        }
        if !frames.is_empty() {
            bail!("input broker cancelled inject batch {batch_id} while also returning frames");
        }
        if authorization_generation != 0 {
            bail!("input broker cancelled inject batch {batch_id} with an authorization generation");
        }
        if self.last_completed_batch_id == batch_id {
            // A lost response can replay the cancellation until its ack is
            // observed by the daemon.
            return Ok(false);
        }
        if let Some(active_batch_id) = self.active_batch_id
            && active_batch_id != batch_id
        {
            bail!(
                "input broker cancelled batch {batch_id} while {active_batch_id} remained pending"
            );
        }
        self.frames.clear();
        self.active_batch_id = None;
        self.active_authorization_generation = None;
        self.last_completed_batch_id = batch_id;
        self.held_input_resume = None;
        self.failed_inject_batch_id = None;
        Ok(true)
    }

    #[cfg(test)]
    fn accept_batch(
        &mut self,
        batch_id: u64,
        frames: Vec<Vec<core_input::InputEvent>>,
    ) -> Result<()> {
        self.accept_authorized_batch(batch_id, frames, 1)
    }

    fn accept_authorized_batch(
        &mut self,
        batch_id: u64,
        frames: Vec<Vec<core_input::InputEvent>>,
        authorization_generation: u64,
    ) -> Result<()> {
        if batch_id == 0 {
            if frames.is_empty() && authorization_generation == 0 {
                return Ok(());
            }
            bail!("input broker inject reply carried frames or authorization without a batch id");
        }
        if authorization_generation == 0 {
            bail!("input broker inject batch {batch_id} omitted its authorization generation");
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
            if self.active_authorization_generation != Some(authorization_generation) {
                bail!("input broker changed authorization generation for retained batch {batch_id}");
            }
            return Ok(());
        }
        if frames.is_empty() {
            bail!("input broker announced new inject batch {batch_id} without frames");
        }
        self.active_batch_id = Some(batch_id);
        self.active_authorization_generation = Some(authorization_generation);
        self.frames.extend(frames);
        Ok(())
    }

    fn prepare_held_input_resume(&mut self, injected_state: &InjectedInputState) {
        let current_downs = injected_state.held_down_events();
        let previous = self.held_input_resume.take();
        if current_downs.is_empty() && previous.is_none() {
            return;
        }
        let authorization_generation = previous
            .as_ref()
            .map(|resume| resume.authorization_generation)
            .or(self.held_authorization_generation);
        let Some(authorization_generation) = authorization_generation else {
            return;
        };
        let intended_downs = previous.map_or(current_downs, |resume| resume.intended_downs);
        self.held_input_resume = Some(BrokerHeldInputResume {
            authorization_generation,
            authorized_for_attempt: false,
            pending_restore: intended_downs.clone(),
            intended_downs,
        });
    }

    fn held_authorization_request(&self, injected_state: &InjectedInputState) -> u64 {
        self.held_input_resume
            .as_ref()
            .map(|resume| resume.authorization_generation)
            .or_else(|| {
                (!injected_state.held_down_events().is_empty())
                    .then_some(self.held_authorization_generation)
                    .flatten()
            })
            .unwrap_or(0)
    }

    fn observe_held_authorization_reply(
        &mut self,
        requested_generation: u64,
        authorized: bool,
    ) -> Result<bool> {
        if requested_generation == 0 {
            if authorized {
                bail!("input broker authorized held input without a requested generation");
            }
            return Ok(false);
        }
        if authorized {
            if let Some(resume) = self.held_input_resume.as_mut()
                && resume.authorization_generation == requested_generation
            {
                resume.authorized_for_attempt = true;
            }
            return Ok(false);
        }

        if self
            .held_input_resume
            .as_ref()
            .is_some_and(|resume| resume.authorization_generation == requested_generation)
        {
            self.held_input_resume = None;
        }
        if self.held_authorization_generation == Some(requested_generation) {
            self.held_authorization_generation = None;
        }
        Ok(true)
    }

    fn clear_held_input_resume(&mut self) {
        self.held_input_resume = None;
    }

    fn stage_local_cleanup(&mut self, injected_state: &mut InjectedInputState) {
        for release in injected_state.release_events_snapshot() {
            if !self.pending_local_releases.contains(&release) {
                self.pending_local_releases.push(release);
            }
        }
        let elevated_controller = injected_state.elevated_cleanup_controller();
        if !self.pending_local_releases.is_empty()
            || injected_state.windows_input.has_pending_native_cleanup()
        {
            self.pending_cleanup_windows_input = Some(injected_state.windows_input.clone());
        }
        if let Some(controller) = elevated_controller {
            self.pending_elevated_cleanup = Some(controller);
        }
    }

    fn local_cleanup_pending(&self) -> bool {
        self.pending_elevated_cleanup.is_some()
            || !self.pending_local_releases.is_empty()
            || self
                .pending_cleanup_windows_input
                .as_ref()
                .is_some_and(WindowsInputState::has_pending_native_cleanup)
    }

    fn pending_local_cleanup_state(&self) -> InjectedInputState {
        let mut state = InjectedInputState::with_windows_input(
            self.pending_cleanup_windows_input
                .clone()
                .unwrap_or_else(|| WindowsInputState::new(WindowsNumLockState::new(false))),
        );
        if let Some(controller) = self.pending_elevated_cleanup.clone() {
            state.elevated_controller = Some(controller);
            state.delivery_lane = InputDeliveryLane::Blocked;
        }
        state
    }

    fn process_local_cleanup(&mut self, injected_state: &mut InjectedInputState) -> bool {
        self.process_local_cleanup_with(injected_state, apply_injected_input_events)
    }

    fn process_local_cleanup_with<F>(
        &mut self,
        injected_state: &mut InjectedInputState,
        mut apply: F,
    ) -> bool
    where
        F: FnMut(&[core_input::InputEvent], &mut InjectedInputState) -> InputSendOutcome,
    {
        if self.pending_elevated_cleanup.is_some() {
            if !injected_state.stop_elevated_lane_for_cleanup() {
                return false;
            }
            self.held_authorization_generation = None;
        }
        if self.pending_local_releases.is_empty()
            && !injected_state.windows_input.has_pending_native_cleanup()
        {
            self.complete_elevated_cleanup();
            return true;
        }
        let releases = self.pending_local_releases.clone();
        let outcome = apply(&releases, injected_state);
        let complete = outcome.error.is_none()
            && outcome.remaining_events.is_empty()
            && !injected_state.windows_input.has_pending_native_cleanup();
        self.pending_local_releases = outcome.remaining_events;
        if complete {
            self.held_authorization_generation = None;
            self.pending_cleanup_windows_input = None;
            self.complete_elevated_cleanup();
        } else if self.pending_cleanup_windows_input.is_none() {
            self.pending_cleanup_windows_input = Some(injected_state.windows_input.clone());
        }
        complete
    }

    fn complete_elevated_cleanup(&mut self) {
        if let Some(controller) = self.pending_elevated_cleanup.take()
            && controller.direct_recovery_cleanup_required()
        {
            controller.complete_direct_recovery_cleanup();
        }
    }

    fn process_local_cleanup_bounded_with<F>(
        &mut self,
        injected_state: &mut InjectedInputState,
        max_attempts: usize,
        mut apply: F,
    ) -> bool
    where
        F: FnMut(&[core_input::InputEvent], &mut InjectedInputState) -> InputSendOutcome,
    {
        for _ in 0..max_attempts {
            if self.process_local_cleanup_with(injected_state, &mut apply) {
                return true;
            }
        }
        !self.local_cleanup_pending()
    }

    fn process(&mut self, injected_state: &mut InjectedInputState) -> BrokerInjectProgress {
        self.process_with(injected_state, |events, state| state.send_events(events))
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
        if self.pending_elevated_cleanup.is_some()
            && !self.process_local_cleanup(injected_state)
        {
            progress.failed_attempts = 1;
            return self.finish_progress(progress);
        }
        if self.failed_inject_batch_id.is_some() {
            progress.failed_attempts = 1;
            return self.finish_progress(progress);
        }
        if self.local_cleanup_pending() {
            progress.failed_attempts = 1;
            return self.finish_progress(progress);
        }
        if self.held_input_resume.is_some() {
            if !self
                .held_input_resume
                .as_ref()
                .is_some_and(|resume| resume.authorized_for_attempt)
            {
                progress.failed_attempts = progress.failed_attempts.saturating_add(1);
                return self.finish_progress(progress);
            }
            let (restore_events, authorization_generation) = {
                let resume = self
                    .held_input_resume
                    .as_mut()
                    .expect("held resume remains present");
                resume.authorized_for_attempt = false;
                (
                    resume.pending_restore.clone(),
                    resume.authorization_generation,
                )
            };
            if restore_events.is_empty() {
                self.held_input_resume = None;
            } else {
                let outcome = apply(&restore_events, injected_state);
                if outcome.error.is_some() {
                    let uncertain = outcome.error.as_ref().is_some_and(|error| {
                        error.downcast_ref::<ElevatedDeliveryUncertain>().is_some()
                    });
                    if uncertain {
                        self.held_input_resume = None;
                        self.failed_inject_batch_id = self.active_batch_id;
                        self.input_session_reset_required = true;
                        self.stage_local_cleanup(injected_state);
                        let _ = self.process_local_cleanup_with(injected_state, &mut apply);
                        progress.failed_attempts = progress.failed_attempts.saturating_add(1);
                        return self.finish_progress(progress);
                    }
                    if let Some(resume) = self.held_input_resume.as_mut() {
                        resume.pending_restore = outcome.remaining_events;
                    }
                    if !injected_state.held_down_events().is_empty() {
                        self.held_authorization_generation = Some(authorization_generation);
                    }
                    progress.failed_attempts = progress.failed_attempts.saturating_add(1);
                    return self.finish_progress(progress);
                }
                if !injected_state.held_down_events().is_empty() {
                    self.held_authorization_generation = Some(authorization_generation);
                }
                self.held_input_resume = None;
            }
        }
        while let Some(events) = self.frames.front().cloned() {
            let authorization_generation = self.active_authorization_generation;
            let outcome = apply(&events, injected_state);
            if !injected_state.held_down_events().is_empty() {
                self.held_authorization_generation = authorization_generation;
            } else {
                self.held_authorization_generation = None;
            }
            if outcome.error.is_none() {
                self.frames.pop_front();
                progress.completed_frames = progress.completed_frames.saturating_add(1);
                continue;
            }
            if outcome
                .error
                .as_ref()
                .is_some_and(|error| error.downcast_ref::<ElevatedDeliveryUncertain>().is_some())
            {
                self.failed_inject_batch_id = self.active_batch_id;
                self.input_session_reset_required = true;
                self.stage_local_cleanup(injected_state);
                let _ = self.process_local_cleanup_with(injected_state, &mut apply);
                progress.failed_attempts = progress.failed_attempts.saturating_add(1);
                break;
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
            self.active_authorization_generation = None;
            self.last_completed_batch_id = batch_id;
        }
        self.finish_progress(progress)
    }

    fn finish_progress(&self, mut progress: BrokerInjectProgress) -> BrokerInjectProgress {
        progress.requires_session_teardown =
            self.local_cleanup_pending() || self.input_session_reset_required;
        progress
    }

    fn complete_input_session_reset(&mut self) {
        self.active_batch_id = None;
        self.active_authorization_generation = None;
        self.frames.clear();
        self.held_authorization_generation = None;
        self.held_input_resume = None;
        self.failed_inject_batch_id = None;
        self.input_session_reset_required = false;
    }
}

impl InjectedInputState {
    #[cfg(test)]
    fn new(num_lock_state: WindowsNumLockState) -> Self {
        Self::with_windows_input(WindowsInputState::new(num_lock_state))
    }

    fn with_windows_input(windows_input: WindowsInputState) -> Self {
        Self {
            windows_input,
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            elevated_controller: None,
            delivery_lane: InputDeliveryLane::Direct,
        }
    }

    fn with_elevated_controller(
        num_lock_state: WindowsNumLockState,
        elevated_controller: ElevatedInputController,
    ) -> Self {
        let delivery_lane = if elevated_controller.direct_fallback_safe() {
            InputDeliveryLane::Direct
        } else {
            InputDeliveryLane::Blocked
        };
        Self {
            windows_input: WindowsInputState::new(num_lock_state),
            pressed_keys: Vec::new(),
            pressed_buttons: Vec::new(),
            elevated_controller: Some(elevated_controller),
            delivery_lane,
        }
    }

    fn send_events(&mut self, events: &[core_input::InputEvent]) -> InputSendOutcome {
        let Some(controller) = self.elevated_controller.clone() else {
            let outcome = self.windows_input.send_events(events);
            return observe_injected_input_outcome(events, self, outcome);
        };

        if matches!(self.delivery_lane, InputDeliveryLane::Elevated | InputDeliveryLane::Blocked)
            && controller.direct_fallback_safe()
            && let Err(error) = self.switch_to_direct_lane()
        {
            return InputSendOutcome {
                committed_event_count: 0,
                remaining_events: events.to_vec(),
                error: Some(error),
            };
        }
        if self.delivery_lane == InputDeliveryLane::Direct
            && self.held_down_events().is_empty()
            && !self.windows_input.has_pending_native_cleanup()
        {
            match controller.activate_if_ready() {
                ElevatedInputActivationResult::Activated => {
                    self.delivery_lane = InputDeliveryLane::Elevated;
                }
                ElevatedInputActivationResult::NotReady => {}
                ElevatedInputActivationResult::Uncertain => {
                    self.delivery_lane = InputDeliveryLane::Blocked;
                    return uncertain_elevated_input_outcome(events);
                }
            }
        }

        match self.delivery_lane {
            InputDeliveryLane::Direct => {
                let own_ready_helper_can_wait_for_drain = self
                    .elevated_controller
                    .as_ref()
                    .map(ElevatedInputController::status)
                    .as_ref()
                    .is_some_and(ready_helper_allows_direct_drain);
                if !own_ready_helper_can_wait_for_drain
                    && let Err(error) = ensure_direct_input_lane_available()
                {
                    return InputSendOutcome {
                        committed_event_count: 0,
                        remaining_events: events.to_vec(),
                        error: Some(error),
                    };
                }
                let outcome = self.windows_input.send_events(events);
                observe_injected_input_outcome(events, self, outcome)
            }
            InputDeliveryLane::Elevated => match controller.apply(
                events,
                self.windows_input.num_lock_is_on(),
            ) {
                ElevatedInputApplyResult::Applied {
                    committed_event_count,
                    remaining_events,
                    reason,
                    destination_num_lock_on,
                } => {
                    let _ = self
                        .windows_input
                        .synchronize_num_lock_if_native_idle(destination_num_lock_on);
                    let committed_event_count = committed_event_count.min(events.len());
                    self.observe(&events[..committed_event_count]);
                    InputSendOutcome {
                        committed_event_count,
                        remaining_events,
                        error: (reason
                            != platform_windows::elevated_input::InputInjectorReason::None)
                            .then(|| {
                                anyhow::anyhow!(
                                    "elevated input injector reported {}",
                                    platform_windows::elevated_input::reason_name(reason)
                                )
                            }),
                    }
                }
                ElevatedInputApplyResult::NotActive => {
                    if controller.direct_fallback_safe() {
                        if let Err(error) = self.switch_to_direct_lane() {
                            return InputSendOutcome {
                                committed_event_count: 0,
                                remaining_events: events.to_vec(),
                                error: Some(error),
                            };
                        }
                        let outcome = self.windows_input.send_events(events);
                        observe_injected_input_outcome(events, self, outcome)
                    } else {
                        uncertain_elevated_input_outcome(events)
                    }
                }
                ElevatedInputApplyResult::Uncertain => {
                    self.observe_possible_downs(events);
                    self.delivery_lane = InputDeliveryLane::Blocked;
                    let _ = controller.request_disable();
                    uncertain_elevated_input_outcome(events)
                }
            },
            InputDeliveryLane::Blocked => uncertain_elevated_input_outcome(events),
        }
    }

    fn elevated_cleanup_controller(&mut self) -> Option<ElevatedInputController> {
        let controller = self.elevated_controller.clone()?;
        // A helper can exit while it is connected but still waiting for the
        // ordinary lane to drain. No elevated payload has started in that
        // state, yet the recovery latch still needs a cleanup-owned marker
        // probe and explicit completion before the next broker session.
        if controller.direct_recovery_cleanup_required() {
            return Some(controller);
        }
        if self.delivery_lane == InputDeliveryLane::Direct {
            return None;
        }
        if controller.direct_fallback_safe() {
            return if self.switch_to_direct_lane().is_ok() {
                None
            } else {
                Some(controller)
            };
        }
        Some(controller)
    }

    fn stop_elevated_lane_for_cleanup(&mut self) -> bool {
        let Some(controller) = self.elevated_controller.clone() else {
            return true;
        };
        let stopped = controller.disable_and_wait();
        let direct_cleanup_ready = controller.direct_fallback_safe()
            || controller.direct_recovery_cleanup_required();
        if stopped && direct_cleanup_ready && self.switch_to_direct_lane().is_ok() {
            true
        } else {
            self.delivery_lane = InputDeliveryLane::Blocked;
            false
        }
    }

    fn elevated_status(&self) -> ElevatedInputControllerStatus {
        self.elevated_controller
            .as_ref()
            .map(ElevatedInputController::status)
            .unwrap_or_default()
    }

    fn clear_observed_input(&mut self) {
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
    }

    fn switch_to_direct_lane(&mut self) -> Result<()> {
        ensure_direct_input_lane_available()?;
        let num_lock_on = platform_windows::input::num_lock_state_from_dedicated_message_lane()
            .context("refresh destination Num Lock while leaving elevated input")?;
        if !self
            .windows_input
            .synchronize_num_lock_if_native_idle(num_lock_on)
        {
            bail!("direct input still had pending native Num Lock cleanup");
        }
        self.clear_observed_input();
        self.delivery_lane = InputDeliveryLane::Direct;
        Ok(())
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

    fn observe_possible_downs(&mut self, events: &[core_input::InputEvent]) {
        for event in events {
            match event {
                core_input::InputEvent::Key {
                    state: core_input::KeyState::Down,
                    ..
                }
                | core_input::InputEvent::MouseButton {
                    state: core_input::KeyState::Down,
                    ..
                } => self.observe(std::slice::from_ref(event)),
                _ => {}
            }
        }
    }

    fn held_down_events(&self) -> Vec<core_input::InputEvent> {
        // Restore keys before buttons so a modifier remains effective for a
        // resumed drag/click. Preserve first-down order within each class.
        let mut held = self
            .pressed_keys
            .iter()
            .map(|(scan_code, semantics)| core_input::InputEvent::Key {
                scan_code: *scan_code,
                state: core_input::KeyState::Down,
                semantics: *semantics,
            })
            .collect::<Vec<_>>();
        held.extend(self.pressed_buttons.iter().map(|button| {
            core_input::InputEvent::MouseButton {
                button: *button,
                state: core_input::KeyState::Down,
            }
        }));
        held
    }

    #[cfg(test)]
    fn drain_release_events(&mut self) -> Vec<core_input::InputEvent> {
        let releases = self.release_events_snapshot();
        self.pressed_buttons.clear();
        self.pressed_keys.clear();
        releases
    }

    fn release_events_snapshot(&self) -> Vec<core_input::InputEvent> {
        let mut pressed_buttons = self.pressed_buttons.clone();
        pressed_buttons.sort_by_key(|button| match button {
            core_input::MouseButton::Left => 0,
            core_input::MouseButton::Right => 1,
            core_input::MouseButton::Middle => 2,
            core_input::MouseButton::X1 => 3,
            core_input::MouseButton::X2 => 4,
        });
        let mut pressed_keys = self.pressed_keys.clone();
        pressed_keys.sort_unstable_by_key(|(scan_code, _)| *scan_code);
        let mut releases = pressed_buttons
            .into_iter()
            .map(|button| core_input::InputEvent::MouseButton {
                button,
                state: core_input::KeyState::Up,
            })
            .collect::<Vec<_>>();
        releases.extend(
            pressed_keys
                .into_iter()
                .map(|(scan_code, semantics)| core_input::InputEvent::Key {
                    scan_code,
                    state: core_input::KeyState::Up,
                    semantics,
                }),
        );
        releases
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

#[derive(Clone, Copy)]
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
const LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS: usize = 4;

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
    elevated_input_controller: ElevatedInputController,
) -> Result<InputBrokerSupervisor> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let thread = std::thread::Builder::new()
        .name("boundless-input-broker".to_string())
        .spawn(move || {
            input_broker_supervisor_loop(endpoint, elevated_input_controller, shutdown_rx)
        })
        .context("spawn input broker supervisor")?;
    Ok(InputBrokerSupervisor {
        shutdown: InputBrokerShutdownSignal { shutdown_tx },
        thread: Some(thread),
    })
}

fn input_broker_supervisor_loop(
    endpoint: String,
    elevated_input_controller: ElevatedInputController,
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
        // A completed delivery receipt must outlive an individual exchange
        // future/session. That closes the response-loss window between the
        // final SendInput call and the next exchange acknowledgement.
        let mut inject_batches = BrokerInjectBatchState::default();
        // A local safety unlock must likewise outlive the broker session that
        // observed it. If the reporting RPC fails before the daemon receives
        // the count, a replacement session must remain unlocked and retry the
        // same reconciliation instead of accepting the daemon's stale lock
        // request.
        let mut safety_unlock = SafetyUnlockReconciler::default();
        loop {
            if inject_batches.local_cleanup_pending()
                && !retry_pending_local_cleanup(&mut inject_batches)
            {
                tokio::select! {
                    _ = tokio::time::sleep(INPUT_BROKER_SUPERVISOR_RETRY) => continue,
                    _ = wait_for_broker_shutdown(&mut shutdown_rx) => {
                        finish_pending_local_cleanup_for_shutdown(&mut inject_batches);
                        break;
                    }
                }
            }

            match run_input_broker_session(
                &endpoint,
                shutdown_rx.clone(),
                &mut inject_batches,
                &mut safety_unlock,
                &elevated_input_controller,
            )
            .await
            {
                Ok(BrokerSessionEnd::Shutdown) => {
                    finish_pending_local_cleanup_for_shutdown(&mut inject_batches);
                    break;
                }
                Ok(BrokerSessionEnd::NotNeeded) | Ok(BrokerSessionEnd::Detached) => {}
                Err(error) => eprintln!("boundless input broker session ended: {error:#}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(INPUT_BROKER_SUPERVISOR_RETRY) => {}
                _ = wait_for_broker_shutdown(&mut shutdown_rx) => {
                    finish_pending_local_cleanup_for_shutdown(&mut inject_batches);
                    break;
                },
            }
        }
    });
}

fn retry_pending_local_cleanup(inject_batches: &mut BrokerInjectBatchState) -> bool {
    let mut cleanup_state = inject_batches.pending_local_cleanup_state();
    let complete = inject_batches.process_local_cleanup(&mut cleanup_state);
    if !complete {
        eprintln!("boundless input broker local cleanup remains pending");
    }
    complete
}

fn finish_pending_local_cleanup_for_shutdown(inject_batches: &mut BrokerInjectBatchState) {
    let mut cleanup_state = inject_batches.pending_local_cleanup_state();
    if !inject_batches.process_local_cleanup_bounded_with(
        &mut cleanup_state,
        LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS,
        apply_injected_input_events,
    ) {
        eprintln!(
            "boundless_input_broker_cleanup=shutdown_retry_exhausted attempts={LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS}"
        );
    }
}

fn reconcile_elevated_controller_without_input_state<F>(
    controller: &ElevatedInputController,
    mut stop: F,
) -> bool
where
    F: FnMut() -> bool,
{
    // A hard exit can be observed between broker sessions, when no
    // InjectedInputState remains to own a release snapshot. The supervisor has
    // already drained any persisted local cleanup before entering this path,
    // so a marker-absent recovery latch can be completed without inventing
    // held input. Two bounded stop passes cover the transition where the first
    // failed stop discovers the hard exit and the second confirms lane absence.
    for _ in 0..2 {
        if controller.direct_fallback_safe() {
            return true;
        }
        let stopped = stop();
        if stopped && controller.direct_recovery_cleanup_required() {
            controller.complete_direct_recovery_cleanup();
        }
        if controller.direct_fallback_safe() {
            return true;
        }
        if !controller.direct_recovery_cleanup_required() {
            return false;
        }
    }
    false
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
    inject_batches: &mut BrokerInjectBatchState,
    safety_unlock: &mut SafetyUnlockReconciler,
    elevated_input_controller: &ElevatedInputController,
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
    if inject_batches.local_cleanup_pending() {
        bail!("pending local input cleanup reached broker-session preflight");
    }
    if !reconcile_elevated_controller_without_input_state(elevated_input_controller, || {
        elevated_input_controller.disable_and_wait()
    }) {
        bail!("elevated input cleanup was not confirmed before a replacement broker session");
    }
    if backend_mode != INPUT_BROKER_SERVICE_UNSUPPORTED_MODE {
        // A user-session daemon owns capture/injection directly; a broker
        // would double-capture. It cannot consume the elevated controller,
        // so close any explicit helper before staying detached.
        if elevated_input_controller.status().state
            != platform_windows::elevated_input::InputInjectorState::Off
        {
            let _ = elevated_input_controller.disable_and_wait();
            if !reconcile_elevated_controller_without_input_state(
                elevated_input_controller,
                || elevated_input_controller.disable_and_wait(),
            ) {
                bail!("elevated input cleanup remained pending without a service-session broker");
            }
        }
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
                    delivery_epoch: String::new(),
                    acked_inject_batch_id: 0,
                    reset_input_session: false,
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
    inject_batches.begin_delivery_epoch(&attach.delivery_epoch)?;
    let broker_token = attach.broker_token;
    let delivery_epoch = attach.delivery_epoch;

    let mut input_client = client.clone();
    let clipboard_task = tokio::spawn(clipboard_broker_supervisor_loop(
        client.clone(),
        broker_token.clone(),
    ));
    let mut injected_state = InjectedInputState::with_elevated_controller(
        pump.num_lock_state(),
        elevated_input_controller.clone(),
    );
    let (loop_result, session_end) = tokio::select! {
        result = input_broker_exchange_loop(
            &mut input_client,
            &broker_token,
            &mut pump,
            &mut injected_state,
            inject_batches,
            safety_unlock,
        ) => (result, BrokerSessionEnd::Detached),
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => (Ok(()), BrokerSessionEnd::Shutdown),
    };
    // Clipboard failures are supervised independently and never select the
    // input path out of service. Conversely, ending the input session cancels
    // its clipboard worker before token cleanup.
    clipboard_task.abort();
    let _ = clipboard_task.await;

    // Preserve the intended held state before fail-open cleanup. A later
    // same-epoch session restores it only after the daemon reauthorizes the
    // retained batch, then continues with the exact payload suffix.
    if matches!(session_end, BrokerSessionEnd::Detached)
        && !inject_batches.input_session_reset_required
        && injected_state.delivery_lane != InputDeliveryLane::Blocked
    {
        inject_batches.prepare_held_input_resume(&injected_state);
    } else {
        inject_batches.clear_held_input_resume();
    }

    // The escape gesture or lease watchdog can fire after the last exchange
    // request was assembled, including while that RPC is failing. Retain
    // those final actions in supervisor-owned state before this pump drops so
    // the next attachment reports them and refuses stale relock authority.
    safety_unlock.observe(pump.drain_control_actions());

    // Unlock and locally release injected state before any cleanup IPC.
    let _ = pump.set_lock_active(false);
    inject_batches.stage_local_cleanup(&mut injected_state);
    if !inject_batches.process_local_cleanup(&mut injected_state) {
        eprintln!("boundless input broker failed to complete local injected-input cleanup");
    }

    // Do not submit synthetic captured releases through the ordinary exchange:
    // that would consume the daemon relay's authoritative pressed-state before
    // it can forward releases to the captured peer. Authorized detach owns the
    // release-then-clear operation as one server-side lifecycle transition.
    let _ = pump.drain_release_events();
    // A transient exchange failure with an incomplete batch keeps the daemon
    // attachment/batch intact so this supervisor can reattach to the same
    // delivery ID and retry only its retained suffix. Cooperative shutdown,
    // and every cleanup after a completed batch, atomically submit the latest
    // exact receipt before the daemon considers any unacknowledged requeue.
    if should_detach_input_broker_session(session_end, inject_batches) {
        let reset_input_session = inject_batches.input_session_reset_required;
        let detached = tokio::time::timeout(
            INPUT_BROKER_CLEANUP_RPC_TIMEOUT,
            client.detach_input_broker(InputBrokerDetachRequest {
                broker_token,
                delivery_epoch,
                acked_inject_batch_id: inject_batches.acked_batch_id(),
                reset_input_session,
            }),
        )
        .await;
        if reset_input_session
            && matches!(detached, Ok(Ok(response)) if response.get_ref().ok)
        {
            inject_batches.complete_input_session_reset();
        }
    }

    loop_result.map(|_| session_end)
}

fn should_detach_input_broker_session(
    session_end: BrokerSessionEnd,
    inject_batches: &BrokerInjectBatchState,
) -> bool {
    match session_end {
        BrokerSessionEnd::Shutdown => true,
        BrokerSessionEnd::Detached => {
            inject_batches.input_session_reset_required
                || (!inject_batches.backpressure_active()
                && inject_batches.held_input_resume.is_none()
                && !inject_batches.local_cleanup_pending())
        }
        BrokerSessionEnd::NotNeeded => false,
    }
}

async fn input_broker_exchange_loop(
    client: &mut ControlPlaneServiceClient<Channel>,
    broker_token: &str,
    pump: &mut HookInputPump,
    injected_state: &mut InjectedInputState,
    inject_batches: &mut BrokerInjectBatchState,
    safety_unlock: &mut SafetyUnlockReconciler,
) -> Result<()> {
    let mut injected_frame_count = 0u32;
    let mut inject_failure_count = 0u32;
    let mut capture_forwarding = BrokerCaptureForwardingGate::default();
    loop {
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
        let held_input_authorization_generation =
            inject_batches.held_authorization_request(injected_state);
        let elevated_status = injected_state.elevated_status();
        if elevated_status.direct_recovery_cleanup_required {
            inject_batches.require_session_reset_for_elevated_recovery();
            bail!(
                "elevated input helper exited before held-input cleanup; resetting broker authorization"
            );
        }
        let (elevated_state, elevated_reason, elevated_signature_trust) =
            elevated_status.telemetry_fields();

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
                held_input_authorization_generation,
                raw_device_wheel_event_count: wheel_sources.raw_device,
                raw_system_wheel_event_count: wheel_sources.raw_system,
                hook_wheel_event_count: wheel_sources.hook,
                elevated_injector_state: elevated_state.to_string(),
                elevated_injector_reason: elevated_reason.to_string(),
                elevated_injector_signature_trust: elevated_signature_trust.to_string(),
                failed_inject_batch_id: inject_batches.failed_batch_id(),
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
        let inject_batch_cancelled_now = inject_batches.accept_reply(
            reply.inject_batch_id,
            reply.inject_batch_cancelled,
            decoded_inject_frames,
            reply.inject_authorization_generation,
        )?;
        let held_input_authorization_revoked = inject_batches
            .observe_held_authorization_reply(
                held_input_authorization_generation,
                reply.held_input_authorized,
            )?;
        if inject_batch_cancelled_now || held_input_authorization_revoked {
            inject_batches.stage_local_cleanup(injected_state);
            if !inject_batches.process_local_cleanup(injected_state) {
                bail!("local held-input cleanup remains pending after daemon authorization change");
            }
        }
        // Native injection happens only after this exchange has successfully
        // re-authorized (or cancelled) the retained batch. A response loss
        // therefore cannot trigger a suffix retry under stale owner/feature
        // state.
        let inject_progress = inject_batches.process(injected_state);
        injected_frame_count = injected_frame_count
            .saturating_add(inject_progress.completed_frames);
        inject_failure_count = inject_failure_count
            .saturating_add(inject_progress.failed_attempts);
        if inject_progress.requires_session_teardown {
            bail!("local held-input cleanup requires a fresh broker session");
        }

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
            delivery_epoch: String::new(),
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
            delivery_epoch: "daemon-epoch".to_string(),
            ..old_daemon_reply
        };
        validate_input_broker_attach_revision(&current).expect("current daemon revision");
        assert_eq!(mismatched_attach_cleanup_token(&current), None);
    }

    #[test]
    fn connected_idle_helper_allows_direct_held_input_to_drain_before_activation() {
        let mut status = ElevatedInputControllerStatus {
            state: platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle,
            ..ElevatedInputControllerStatus::default()
        };
        assert!(ready_helper_allows_direct_drain(&status));

        status.direct_fallback_safe = false;
        assert!(!ready_helper_allows_direct_drain(&status));
        status.direct_fallback_safe = true;
        status.state = platform_windows::elevated_input::InputInjectorState::Active;
        assert!(!ready_helper_allows_direct_drain(&status));
    }

    #[test]
    fn elevated_cleanup_retains_confirmed_and_possible_direct_releases() {
        let controller = ElevatedInputController::start().expect("start test controller");
        update_elevated_input_status(&controller.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
            status.reason =
                platform_windows::elevated_input::InputInjectorReason::DeliveryUncertain;
            status.direct_fallback_safe = false;
        });
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let button_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let button_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };
        let mut injected = InjectedInputState::with_elevated_controller(
            WindowsNumLockState::new(false),
            controller,
        );
        injected.delivery_lane = InputDeliveryLane::Elevated;
        injected.observe(std::slice::from_ref(&ctrl_down));
        injected.observe_possible_downs(std::slice::from_ref(&button_down));

        let mut batch = BrokerInjectBatchState::default();
        batch.stage_local_cleanup(&mut injected);
        assert!(batch.pending_elevated_cleanup.is_some());
        assert!(batch.pending_local_releases.contains(&ctrl_up));
        assert!(batch.pending_local_releases.contains(&button_up));

        let mut applied_releases = Vec::new();
        assert!(batch.process_local_cleanup_with(
            &mut injected,
            |events, state| {
                applied_releases.extend_from_slice(events);
                observe_injected_input_outcome(
                    events,
                    state,
                    InputSendOutcome {
                        committed_event_count: events.len(),
                        remaining_events: Vec::new(),
                        error: None,
                    },
                )
            }
        ));
        assert!(applied_releases.contains(&ctrl_up));
        assert!(applied_releases.contains(&button_up));
        assert_eq!(injected.delivery_lane, InputDeliveryLane::Direct);
        assert!(injected.elevated_status().direct_fallback_safe);
    }

    #[test]
    fn hard_helper_exit_resets_authorization_and_releases_held_ctrl_before_direct_resume() {
        let controller = ElevatedInputController::start().expect("start test controller");
        update_elevated_input_status(&controller.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
            status.reason = platform_windows::elevated_input::InputInjectorReason::ParentExited;
            status.direct_fallback_safe = false;
            status.direct_recovery_cleanup_required = true;
        });
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let unrelated_move = core_input::InputEvent::MouseMove { dx: 1, dy: 0 };
        let mut injected = InjectedInputState::with_elevated_controller(
            WindowsNumLockState::new(false),
            controller,
        );
        injected.delivery_lane = InputDeliveryLane::Elevated;
        injected.observe(std::slice::from_ref(&ctrl_down));

        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_batch(41, vec![vec![unrelated_move.clone()]])
            .expect("stage next unrelated event");
        batch.require_session_reset_for_elevated_recovery();
        assert!(batch.input_session_reset_required);
        assert_eq!(batch.failed_batch_id(), 41);

        batch.stage_local_cleanup(&mut injected);
        let mut applied_releases = Vec::new();
        assert!(batch.process_local_cleanup_with(
            &mut injected,
            |events, state| {
                applied_releases.extend_from_slice(events);
                observe_injected_input_outcome(
                    events,
                    state,
                    InputSendOutcome {
                        committed_event_count: events.len(),
                        remaining_events: Vec::new(),
                        error: None,
                    },
                )
            }
        ));
        assert_eq!(applied_releases, vec![ctrl_up]);
        assert!(!applied_releases.contains(&unrelated_move));
        assert_eq!(injected.delivery_lane, InputDeliveryLane::Direct);
        let recovered = injected.elevated_status();
        assert!(recovered.direct_fallback_safe);
        assert!(!recovered.direct_recovery_cleanup_required);

        batch.complete_input_session_reset();
        assert!(batch.frames.is_empty());
        assert!(!batch.input_session_reset_required);
    }

    #[test]
    fn hard_helper_exit_while_direct_and_idle_clears_recovery_latch_during_cleanup() {
        let controller = ElevatedInputController::start().expect("start test controller");
        update_elevated_input_status(&controller.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
            status.reason = platform_windows::elevated_input::InputInjectorReason::ParentExited;
            status.direct_fallback_safe = false;
            status.direct_recovery_cleanup_required = true;
        });
        let mut injected = InjectedInputState::with_elevated_controller(
            WindowsNumLockState::new(false),
            controller,
        );
        injected.delivery_lane = InputDeliveryLane::Direct;
        assert!(injected.held_down_events().is_empty());

        let mut batch = BrokerInjectBatchState::default();
        batch.require_session_reset_for_elevated_recovery();
        batch.stage_local_cleanup(&mut injected);
        assert!(batch.pending_elevated_cleanup.is_some());
        assert!(batch.process_local_cleanup_with(
            &mut injected,
            |events, _state| panic!("idle recovery emitted unexpected releases: {events:?}")
        ));

        let recovered = injected.elevated_status();
        assert!(recovered.direct_fallback_safe);
        assert!(!recovered.direct_recovery_cleanup_required);
        assert!(!batch.local_cleanup_pending());
    }

    #[test]
    fn hard_helper_exit_between_sessions_completes_controller_only_recovery() {
        let controller = ElevatedInputController::start().expect("start test controller");
        update_elevated_input_status(&controller.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Active;
            status.direct_fallback_safe = false;
            status.direct_recovery_cleanup_required = false;
        });
        let mut stop_attempts = 0usize;
        assert!(reconcile_elevated_controller_without_input_state(
            &controller,
            || {
                stop_attempts += 1;
                if stop_attempts == 1 {
                    mark_direct_recovery_required_after_helper_exit(
                        &controller.inner.status,
                        true,
                    );
                    false
                } else {
                    true
                }
            }
        ));
        assert_eq!(stop_attempts, 2);
        let recovered = controller.status();
        assert!(recovered.direct_fallback_safe);
        assert!(!recovered.direct_recovery_cleanup_required);
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
                requires_session_teardown: false,
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
                requires_session_teardown: false,
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
    fn uncertain_elevated_delivery_forces_atomic_session_reset_without_replay() {
        let event = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_batch(9, vec![vec![event.clone()]])
            .expect("stage elevated batch");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));

        let first = batch.process_with(&mut injected, |events, _state| InputSendOutcome {
            committed_event_count: 0,
            remaining_events: events.to_vec(),
            error: Some(anyhow::Error::new(ElevatedDeliveryUncertain)),
        });
        assert_eq!(first.failed_attempts, 1);
        assert!(first.requires_session_teardown);
        assert_eq!(batch.failed_batch_id(), 9);
        assert!(batch.backpressure_active());
        assert!(should_detach_input_broker_session(
            BrokerSessionEnd::Detached,
            &batch
        ));

        let mut replayed = false;
        let waiting = batch.process_with(&mut injected, |_events, _state| {
            replayed = true;
            InputSendOutcome {
                committed_event_count: 1,
                remaining_events: Vec::new(),
                error: None,
            }
        });
        assert_eq!(waiting.failed_attempts, 1);
        assert!(!replayed, "uncertain elevated input must never be replayed");

        batch.complete_input_session_reset();
        assert_eq!(batch.failed_batch_id(), 0);
        assert!(!batch.backpressure_active());
        assert_eq!(batch.acked_batch_id(), 0);
    }

    #[test]
    fn completed_receipt_survives_same_epoch_reattach_without_reinjection() {
        let event = core_input::InputEvent::MouseMove { dx: 1, dy: 0 };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin first broker session");
        batch
            .accept_batch(7, vec![vec![event.clone()]])
            .expect("stage batch");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        let mut inject_calls = 0;
        let completed = batch.process_with(&mut injected, |events, state| {
            inject_calls += 1;
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
        assert_eq!(completed.completed_frames, 1);
        assert_eq!(inject_calls, 1);
        assert_eq!(batch.acked_batch_id(), 7);

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon epoch");
        batch
            .accept_batch(7, vec![vec![event]])
            .expect("replayed response after lost acknowledgement");
        let replay = batch.process_with(&mut injected, |_events, _state| {
            panic!("same-epoch completed delivery must not reach SendInput twice")
        });
        assert_eq!(replay, BrokerInjectProgress::default());
        assert_eq!(batch.acked_batch_id(), 7);
    }

    #[test]
    fn completed_down_only_batch_preserves_held_intent_for_reattach() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let c_down = core_input::InputEvent::Key {
            scan_code: 46,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let c_up = core_input::InputEvent::Key {
            scan_code: 46,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_batch(8, vec![vec![ctrl_down.clone()]])
            .expect("stage completed Down-only batch");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        let completed = batch.process_with(&mut injected, |events, state| {
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
        assert_eq!(completed.completed_frames, 1);
        assert!(!batch.backpressure_active());

        batch.prepare_held_input_resume(&injected);
        assert!(
            batch.held_input_resume.is_some(),
            "a completed receipt must not erase a still-held modifier before same-epoch reattach"
        );
        batch.stage_local_cleanup(&mut injected);
        let mut cleanup_attempts = Vec::new();
        assert!(batch.process_local_cleanup_with(
            &mut injected,
            |events, state| {
                cleanup_attempts.push(events.to_vec());
                observe_injected_input_outcome(
                    events,
                    state,
                    InputSendOutcome {
                        committed_event_count: events.len(),
                        remaining_events: Vec::new(),
                        error: None,
                    },
                )
            }
        ));
        assert_eq!(cleanup_attempts, vec![vec![ctrl_up.clone()]]);

        // Lost request: the daemon has not consumed the receipt yet and the
        // replacement exchange returns no later payload.
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same epoch");
        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&replacement);
        assert_eq!(requested, 1);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("fresh daemon authorization");
        batch
            .accept_authorized_batch(0, Vec::new(), 0)
            .expect("receipt-only reply");
        let mut attempts = Vec::new();
        batch.process_with(&mut replacement, |events, state| {
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

        let later_payload = vec![c_down, c_up, ctrl_up];
        batch
            .accept_authorized_batch(9, vec![later_payload.clone()], 1)
            .expect("later chord payload");
        batch.process_with(&mut replacement, |events, state| {
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
        assert_eq!(attempts, vec![vec![ctrl_down], later_payload]);
        assert!(replacement.held_down_events().is_empty());
    }

    #[test]
    fn response_lost_after_completed_button_receipt_restores_before_returned_payload() {
        let mouse_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let mouse_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };
        let later_payload = vec![
            core_input::InputEvent::MouseMove { dx: 4, dy: 1 },
            mouse_up.clone(),
        ];
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_authorized_batch(20, vec![vec![mouse_down.clone()]], 5)
            .expect("stage completed button batch");
        let mut first = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first, |events, state| {
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
        batch.prepare_held_input_resume(&first);
        batch.stage_local_cleanup(&mut first);
        assert!(batch.process_local_cleanup_with(&mut first, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        }));

        // The receipt request reached the daemon and its response (which also
        // carried batch 21) was lost. The next response repeats only batch 21;
        // the old button intent must still restore first.
        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&replacement);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("reauthorize retained button");
        batch
            .accept_authorized_batch(21, vec![later_payload.clone()], 5)
            .expect("repeat later payload after response loss");
        let mut attempts = Vec::new();
        batch.process_with(&mut replacement, |events, state| {
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
        assert_eq!(attempts, vec![vec![mouse_down], later_payload]);
    }

    #[test]
    fn uncertain_elevated_restore_is_never_replayed() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let later = core_input::InputEvent::MouseMove { dx: 1, dy: 0 };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_authorized_batch(20, vec![vec![ctrl_down.clone()]], 5)
            .expect("stage held modifier");
        let mut first = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first, |events, state| {
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
        batch.prepare_held_input_resume(&first);
        batch.stage_local_cleanup(&mut first);
        assert!(batch.process_local_cleanup_with(&mut first, |events, state| {
            assert_eq!(events, std::slice::from_ref(&ctrl_up));
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        }));

        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&replacement);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("reauthorize retained modifier");
        batch
            .accept_authorized_batch(21, vec![vec![later]], 5)
            .expect("stage later payload");

        let mut attempts = Vec::new();
        let first = batch.process_with(&mut replacement, |events, _state| {
            attempts.push(events.to_vec());
            InputSendOutcome {
                committed_event_count: 0,
                remaining_events: events.to_vec(),
                error: Some(anyhow::Error::new(ElevatedDeliveryUncertain)),
            }
        });
        assert_eq!(first.failed_attempts, 1);
        assert_eq!(attempts, vec![vec![ctrl_down.clone()]]);
        assert!(batch.held_input_resume.is_none());
        assert_eq!(batch.failed_batch_id(), 21);

        let waiting = batch.process_with(&mut replacement, |events, _state| {
            attempts.push(events.to_vec());
            InputSendOutcome {
                committed_event_count: events.len(),
                remaining_events: Vec::new(),
                error: None,
            }
        });
        assert_eq!(waiting.failed_attempts, 1);
        assert_eq!(attempts, vec![vec![ctrl_down]]);
    }

    #[test]
    fn uncertain_completed_hold_with_failed_cleanup_requires_session_teardown() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let alt_down = core_input::InputEvent::Key {
            scan_code: 56,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let alt_up = core_input::InputEvent::Key {
            scan_code: 56,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_authorized_batch(20, vec![vec![ctrl_down.clone()]], 5)
            .expect("stage held modifier");
        let mut first = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first, |events, state| {
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
        batch.prepare_held_input_resume(&first);
        batch.stage_local_cleanup(&mut first);
        assert!(batch.process_local_cleanup_with(&mut first, |events, state| {
            assert_eq!(events, std::slice::from_ref(&ctrl_up));
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        }));
        assert_eq!(batch.failed_batch_id(), 0);

        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        replacement.observe(std::slice::from_ref(&alt_down));
        let requested = batch.held_authorization_request(&replacement);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("reauthorize completed hold");

        let mut attempts = Vec::new();
        let progress = batch.process_with(&mut replacement, |events, _state| {
            attempts.push(events.to_vec());
            InputSendOutcome {
                committed_event_count: 0,
                remaining_events: events.to_vec(),
                error: Some(if events == std::slice::from_ref(&ctrl_down) {
                    anyhow::Error::new(ElevatedDeliveryUncertain)
                } else {
                    anyhow::anyhow!("scripted cleanup failure")
                }),
            }
        });
        assert_eq!(attempts, vec![vec![ctrl_down], vec![alt_up]]);
        assert_eq!(batch.failed_batch_id(), 0);
        assert!(batch.local_cleanup_pending());
        assert!(progress.requires_session_teardown);
    }

    #[test]
    fn completed_hold_real_cleanup_path_skips_transient_detach_and_restores() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin session");
        batch
            .accept_authorized_batch(30, vec![vec![ctrl_down.clone()]], 7)
            .expect("stage completed hold");
        let mut first_session = InjectedInputState::new(WindowsNumLockState::new(false));
        assert_eq!(
            batch
                .process_with(&mut first_session, |events, state| {
                    observe_injected_input_outcome(
                        events,
                        state,
                        InputSendOutcome {
                            committed_event_count: events.len(),
                            remaining_events: Vec::new(),
                            error: None,
                        },
                    )
                })
                .completed_frames,
            1
        );

        batch.prepare_held_input_resume(&first_session);
        batch.stage_local_cleanup(&mut first_session);
        assert!(batch.process_local_cleanup_with(
            &mut first_session,
            |events, state| {
                assert_eq!(events, std::slice::from_ref(&ctrl_up));
                observe_injected_input_outcome(
                    events,
                    state,
                    InputSendOutcome {
                        committed_event_count: events.len(),
                        remaining_events: Vec::new(),
                        error: None,
                    },
                )
            }
        ));
        assert!(batch.held_input_resume.is_some());
        assert!(
            !should_detach_input_broker_session(BrokerSessionEnd::Detached, &batch),
            "successful transient cleanup must not release daemon owner authority before restore"
        );
        assert!(should_detach_input_broker_session(
            BrokerSessionEnd::Shutdown,
            &batch
        ));

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon");
        let requested = batch.held_authorization_request(&first_session);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("daemon reauthorizes retained generation");
        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        let mut restored = Vec::new();
        batch.process_with(&mut replacement, |events, state| {
            restored.extend_from_slice(events);
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
        assert_eq!(restored, vec![ctrl_down]);
    }

    #[test]
    fn authorization_revocation_discards_completed_hold_before_new_payload() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let later = core_input::InputEvent::MouseMove { dx: 1, dy: 0 };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_authorized_batch(30, vec![vec![ctrl_down]], 7)
            .expect("stage held modifier");
        let mut first = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first, |events, state| {
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
        batch.prepare_held_input_resume(&first);
        batch.stage_local_cleanup(&mut first);
        assert!(batch.process_local_cleanup_with(&mut first, |events, state| {
            assert_eq!(events, std::slice::from_ref(&ctrl_up));
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        }));

        let mut replacement = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&replacement);
        assert!(
            batch
                .observe_held_authorization_reply(requested, false)
                .expect("owner/policy revocation reply")
        );
        batch
            .accept_authorized_batch(31, vec![vec![later.clone()]], 8)
            .expect("new owner payload");
        let mut attempts = Vec::new();
        batch.process_with(&mut replacement, |events, state| {
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
        assert_eq!(attempts, vec![vec![later]]);
    }

    #[test]
    fn revoked_hold_is_not_rebound_when_cleanup_fails_beside_new_generation() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_authorized_batch(50, vec![vec![ctrl_down]], 10)
            .expect("stage old owner hold");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut injected, |events, state| {
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
        let requested = batch.held_authorization_request(&injected);
        assert_eq!(requested, 10);
        assert!(
            batch
                .observe_held_authorization_reply(requested, false)
                .expect("old owner revoked")
        );
        batch
            .accept_authorized_batch(
                51,
                vec![vec![core_input::InputEvent::MouseMove { dx: 1, dy: 0 }]],
                11,
            )
            .expect("new generation payload");
        batch.stage_local_cleanup(&mut injected);
        assert!(!batch.process_local_cleanup_with(&mut injected, |events, state| {
            assert_eq!(events, std::slice::from_ref(&ctrl_up));
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 0,
                    remaining_events: events.to_vec(),
                    error: Some(anyhow::anyhow!("scripted revoke cleanup failure")),
                },
            )
        }));

        batch.prepare_held_input_resume(&injected);
        assert!(
            batch.held_input_resume.is_none(),
            "revoked old-owner state must never inherit the new payload generation"
        );
        assert!(batch.local_cleanup_pending());
    }

    #[test]
    fn partial_ctrl_chord_reattach_restores_modifier_before_suffix() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let c_down = core_input::InputEvent::Key {
            scan_code: 46,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let c_up = core_input::InputEvent::Key {
            scan_code: 46,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let suffix = vec![c_down, c_up, ctrl_up.clone()];
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin first broker session");
        batch
            .accept_batch(
                7,
                vec![std::iter::once(ctrl_down.clone())
                    .chain(suffix.clone())
                    .collect()],
            )
            .expect("stage chord batch");
        let mut first_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let first = batch.process_with(&mut first_session, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: suffix.clone(),
                    error: Some(anyhow::anyhow!("scripted suffix failure")),
                },
            )
        });
        assert_eq!(first.failed_attempts, 1);
        batch.prepare_held_input_resume(&first_session);
        assert_eq!(first_session.drain_release_events(), vec![ctrl_up]);

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon");
        batch
            .accept_batch(7, Vec::new())
            .expect("daemon reauthorizes retained batch under backpressure");
        let mut second_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&second_session);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("authorize held modifier restore");
        let mut attempts = Vec::new();
        batch.process_with(&mut second_session, |events, state| {
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

        assert_eq!(attempts, vec![vec![ctrl_down], suffix]);
    }

    #[test]
    fn partial_drag_reattach_restores_button_before_motion_suffix() {
        let mouse_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let mouse_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };
        let suffix = vec![
            core_input::InputEvent::MouseMove { dx: 8, dy: 2 },
            mouse_up.clone(),
        ];
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin first broker session");
        batch
            .accept_batch(
                9,
                vec![std::iter::once(mouse_down.clone())
                    .chain(suffix.clone())
                    .collect()],
            )
            .expect("stage drag batch");
        let mut first_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let first = batch.process_with(&mut first_session, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: suffix.clone(),
                    error: Some(anyhow::anyhow!("scripted drag suffix failure")),
                },
            )
        });
        assert_eq!(first.failed_attempts, 1);
        batch.prepare_held_input_resume(&first_session);
        assert_eq!(first_session.drain_release_events(), vec![mouse_up]);

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon");
        batch
            .accept_batch(9, Vec::new())
            .expect("daemon reauthorizes retained drag batch");
        let mut second_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&second_session);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("authorize held button restore");
        let mut attempts = Vec::new();
        let completed = batch.process_with(&mut second_session, |events, state| {
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

        assert_eq!(attempts, vec![vec![mouse_down], suffix]);
        assert_eq!(completed.completed_frames, 1);
        assert!(second_session.drain_release_events().is_empty());
    }

    #[test]
    fn cancellation_before_reattach_restore_discards_held_intent() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let suffix = vec![core_input::InputEvent::Key {
            scan_code: 46,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        }];
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin broker session");
        batch
            .accept_batch(
                11,
                vec![std::iter::once(ctrl_down).chain(suffix.clone()).collect()],
            )
            .expect("stage chord batch");
        let mut first_session = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first_session, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: suffix.clone(),
                    error: Some(anyhow::anyhow!("scripted suffix failure")),
                },
            )
        });
        batch.prepare_held_input_resume(&first_session);
        assert_eq!(first_session.drain_release_events(), vec![ctrl_up]);

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon");
        assert!(
            batch
                .accept_reply(11, true, Vec::new(), 0)
                .expect("daemon cancellation")
        );
        let mut second_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let progress = batch.process_with(&mut second_session, |_events, _state| {
            panic!("cancelled batch must not restore held input or retry its suffix")
        });
        assert_eq!(progress, BrokerInjectProgress::default());
        assert_eq!(batch.acked_batch_id(), 11);
    }

    #[test]
    fn new_epoch_discards_old_held_intent_before_new_batch() {
        let old_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let old_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };
        let old_suffix = vec![core_input::InputEvent::MouseMove { dx: 5, dy: 0 }];
        let new_event = core_input::InputEvent::MouseMove { dx: 1, dy: 0 };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("old-epoch")
            .expect("begin old daemon epoch");
        batch
            .accept_batch(
                13,
                vec![std::iter::once(old_down)
                    .chain(old_suffix.clone())
                    .collect()],
            )
            .expect("stage old drag batch");
        let mut old_session = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut old_session, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: old_suffix.clone(),
                    error: Some(anyhow::anyhow!("scripted old suffix failure")),
                },
            )
        });
        batch.prepare_held_input_resume(&old_session);
        assert_eq!(old_session.drain_release_events(), vec![old_up]);

        batch
            .begin_delivery_epoch("new-epoch")
            .expect("start replacement daemon epoch");
        batch
            .accept_batch(1, vec![vec![new_event.clone()]])
            .expect("stage new daemon batch");
        let mut new_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let mut attempts = Vec::new();
        batch.process_with(&mut new_session, |events, state| {
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
        assert_eq!(attempts, vec![vec![new_event]]);
        assert_eq!(batch.acked_batch_id(), 1);
    }

    #[test]
    fn partial_held_restore_retains_exact_suffix_and_defers_payload() {
        let ctrl_down = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let shift_down = core_input::InputEvent::Key {
            scan_code: 42,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let shift_up = core_input::InputEvent::Key {
            scan_code: 42,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let payload_suffix = vec![
            core_input::InputEvent::MouseMove { dx: 2, dy: 0 },
            shift_up.clone(),
            ctrl_up.clone(),
        ];
        let mut batch = BrokerInjectBatchState::default();
        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("begin first broker session");
        batch
            .accept_batch(
                15,
                vec![vec![ctrl_down.clone(), shift_down.clone()]
                    .into_iter()
                    .chain(payload_suffix.clone())
                    .collect::<Vec<_>>()],
            )
            .expect("stage held-state batch");
        let mut first_session = InjectedInputState::new(WindowsNumLockState::new(false));
        batch.process_with(&mut first_session, |events, state| {
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 2,
                    remaining_events: payload_suffix.clone(),
                    error: Some(anyhow::anyhow!("scripted payload suffix failure")),
                },
            )
        });
        batch.prepare_held_input_resume(&first_session);
        assert_eq!(
            first_session.drain_release_events(),
            vec![ctrl_up, shift_up]
        );

        batch
            .begin_delivery_epoch("daemon-epoch")
            .expect("reattach to same daemon");
        batch
            .accept_batch(15, Vec::new())
            .expect("first retained-batch reauthorization");
        let mut second_session = InjectedInputState::new(WindowsNumLockState::new(false));
        let requested = batch.held_authorization_request(&second_session);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("authorize first held-state restore attempt");
        let mut attempts = Vec::new();
        let first_restore = batch.process_with(&mut second_session, |events, state| {
            attempts.push(events.to_vec());
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: vec![shift_down.clone()],
                    error: Some(anyhow::anyhow!("scripted restore suffix failure")),
                },
            )
        });
        assert_eq!(
            first_restore,
            BrokerInjectProgress {
                completed_frames: 0,
                failed_attempts: 1,
                requires_session_teardown: false,
            }
        );
        assert_eq!(attempts, vec![vec![ctrl_down.clone(), shift_down.clone()]]);

        batch
            .accept_batch(15, Vec::new())
            .expect("second retained-batch reauthorization");
        let requested = batch.held_authorization_request(&second_session);
        batch
            .observe_held_authorization_reply(requested, true)
            .expect("authorize second held-state restore attempt");
        let second_restore = batch.process_with(&mut second_session, |events, state| {
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
        assert_eq!(second_restore.completed_frames, 1);
        assert_eq!(second_restore.failed_attempts, 0);
        assert_eq!(
            attempts,
            vec![
                vec![ctrl_down, shift_down.clone()],
                vec![shift_down],
                payload_suffix,
            ]
        );
        assert_eq!(batch.acked_batch_id(), 15);
        assert!(second_session.drain_release_events().is_empty());
    }

    #[test]
    fn cancelled_batch_drops_partial_suffix_before_retry_and_allows_later_batch() {
        let key_down = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let mouse_down = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Down,
        };
        let later = core_input::InputEvent::Key {
            scan_code: 31,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_reply(
                7,
                false,
                vec![vec![key_down.clone(), mouse_down.clone()]],
                1,
            )
            .expect("stage authorized batch");
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        let first = batch.process_with(&mut injected, |events, state| {
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
        assert_eq!(first.failed_attempts, 1);
        assert!(batch.backpressure_active());

        let cancelled_now = batch
            .accept_reply(7, true, Vec::new(), 0)
            .expect("owner revocation cancels retained suffix");
        assert!(cancelled_now);
        assert_eq!(
            injected.drain_release_events(),
            vec![core_input::InputEvent::Key {
                scan_code: 30,
                state: core_input::KeyState::Up,
                semantics: core_input::KeySemantics::Physical,
            }],
            "cancelling a partially committed batch must release held local input"
        );
        let after_cancel = batch.process_with(&mut injected, |_events, _state| {
            panic!("cancelled input suffix must not be retried")
        });
        assert_eq!(after_cancel, BrokerInjectProgress::default());
        assert!(!batch.backpressure_active());
        assert_eq!(batch.acked_batch_id(), 7);

        assert!(
            !batch
            .accept_reply(7, true, Vec::new(), 0)
            .expect("lost cancellation response replays idempotently")
        );
        batch
            .accept_reply(8, false, vec![vec![later.clone()]], 1)
            .expect("later authorized batch proceeds");
        let mut attempted = Vec::new();
        let later_progress = batch.process_with(&mut injected, |events, state| {
            attempted.extend_from_slice(events);
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
        assert_eq!(later_progress.completed_frames, 1);
        assert_eq!(attempted, vec![later]);
        assert_eq!(batch.acked_batch_id(), 8);
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
    fn failed_exchange_preserves_safety_unlock_for_reattached_session() {
        // This value models the supervisor-owned reconciler shared by two
        // successive broker sessions. The first session submits an escape but
        // loses the RPC response before it can mark the report delivered.
        let mut supervisor_state = SafetyUnlockReconciler::default();
        {
            let first_session = &mut supervisor_state;
            first_session.observe(vec![HookControlAction::EscapeUnlock]);
            assert_eq!(first_session.report_counts(), (1, 0, 0));
            assert!(!first_session.lock_should_be_active(true, true));
        }

        // Reattachment must retry the exact pending cause and remain unlocked
        // through both the stale lock reply and the later daemon release.
        {
            let reattached_session = &mut supervisor_state;
            assert_eq!(reattached_session.report_counts(), (1, 0, 0));
            assert!(!reattached_session.should_forward_captured_events());
            assert!(!reattached_session.lock_should_be_active(true, true));
            reattached_session.mark_report_delivered();
            assert!(!reattached_session.lock_should_be_active(false, false));
            assert!(reattached_session.should_forward_captured_events());
        }
    }

    #[test]
    fn session_teardown_actions_join_unacknowledged_safety_report() {
        let mut supervisor_state = SafetyUnlockReconciler::default();
        supervisor_state.observe(vec![HookControlAction::EscapeUnlock]);
        let submitted_before_rpc_failure = supervisor_state.report_counts();
        assert_eq!(submitted_before_rpc_failure, (1, 0, 0));

        // The lease can expire while the failed exchange future is unwinding.
        // Teardown drains that final pump action into the same durable state.
        supervisor_state.observe(vec![HookControlAction::LeaseExpiredUnlock]);
        assert_eq!(supervisor_state.report_counts(), (1, 1, 0));
        assert!(!supervisor_state.lock_should_be_active(true, true));
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
    fn partial_and_zero_cleanup_failures_gate_payload_until_exact_suffix_completes() {
        let mouse_up = core_input::InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: core_input::KeyState::Up,
        };
        let ctrl_up = core_input::InputEvent::Key {
            scan_code: 29,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let shift_up = core_input::InputEvent::Key {
            scan_code: 42,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        injected.observe(&[
            core_input::InputEvent::MouseButton {
                button: core_input::MouseButton::Left,
                state: core_input::KeyState::Down,
            },
            core_input::InputEvent::Key {
                scan_code: 29,
                state: core_input::KeyState::Down,
                semantics: core_input::KeySemantics::Physical,
            },
            core_input::InputEvent::Key {
                scan_code: 42,
                state: core_input::KeyState::Down,
                semantics: core_input::KeySemantics::Physical,
            },
        ]);
        let mut batch = BrokerInjectBatchState::default();
        batch.stage_local_cleanup(&mut injected);

        let full = vec![mouse_up, ctrl_up.clone(), shift_up.clone()];
        assert!(!batch.process_local_cleanup_with(&mut injected, |events, state| {
            assert_eq!(events, full);
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 1,
                    remaining_events: vec![ctrl_up.clone(), shift_up.clone()],
                    error: Some(anyhow::anyhow!("scripted partial cleanup failure")),
                },
            )
        }));
        assert_eq!(
            batch.pending_local_releases,
            vec![ctrl_up.clone(), shift_up.clone()]
        );

        batch
            .accept_authorized_batch(
                40,
                vec![vec![core_input::InputEvent::MouseMove { dx: 9, dy: 0 }]],
                3,
            )
            .expect("stage payload behind cleanup");
        let blocked = batch.process_with(&mut injected, |_events, _state| {
            panic!("payload must stay deferred while cleanup remains pending")
        });
        assert_eq!(blocked.failed_attempts, 1);
        assert_eq!(blocked.completed_frames, 0);

        assert!(!batch.process_local_cleanup_with(&mut injected, |events, state| {
            assert_eq!(events, [ctrl_up.clone(), shift_up.clone()]);
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: 0,
                    remaining_events: events.to_vec(),
                    error: Some(anyhow::anyhow!("scripted zero-send cleanup failure")),
                },
            )
        }));
        assert_eq!(
            batch.pending_local_releases,
            vec![ctrl_up.clone(), shift_up.clone()]
        );

        assert!(batch.process_local_cleanup_with(&mut injected, |events, state| {
            assert_eq!(events, [ctrl_up.clone(), shift_up.clone()]);
            observe_injected_input_outcome(
                events,
                state,
                InputSendOutcome {
                    committed_event_count: events.len(),
                    remaining_events: Vec::new(),
                    error: None,
                },
            )
        }));
        let mut payload_attempts = Vec::new();
        let completed = batch.process_with(&mut injected, |events, state| {
            payload_attempts.push(events.to_vec());
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
        assert_eq!(completed.completed_frames, 1);
        assert_eq!(payload_attempts.len(), 1);
    }

    #[test]
    fn num_lock_native_cleanup_survives_session_loss_before_payload_retry() {
        let num_lock_down = core_input::InputEvent::Key {
            scan_code: 0x45,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Windows {
                virtual_key: 0x90,
                num_lock_on: true,
            },
        };
        let windows_input = WindowsInputState::new(WindowsNumLockState::new(false));
        let mut first_session = InjectedInputState::with_windows_input(windows_input.clone());
        let mut send_calls = 0usize;
        let partial_toggle = windows_input.send_events_with_sender(
            std::slice::from_ref(&num_lock_down),
            |records| {
                send_calls += 1;
                match send_calls {
                    1 => {
                        assert_eq!(records.len(), 2);
                        Ok(1)
                    }
                    2 => Err(anyhow::anyhow!("scripted Num Lock toggle suffix failure")),
                    3 => Err(anyhow::anyhow!("scripted Num Lock key-up cleanup failure")),
                    _ => panic!("unexpected initial SendInput call {send_calls}"),
                }
            },
        );
        assert!(partial_toggle.error.is_some());
        assert!(windows_input.has_pending_native_cleanup());
        observe_injected_input_outcome(
            std::slice::from_ref(&num_lock_down),
            &mut first_session,
            partial_toggle,
        );

        let payload = core_input::InputEvent::MouseMove { dx: 4, dy: 0 };
        let mut batch = BrokerInjectBatchState::default();
        batch
            .accept_authorized_batch(50, vec![vec![payload.clone()]], 9)
            .expect("stage payload behind native cleanup");
        batch.stage_local_cleanup(&mut first_session);
        drop(first_session);
        assert!(batch.local_cleanup_pending());
        assert_eq!(
            batch
                .process_with(
                    &mut InjectedInputState::new(WindowsNumLockState::new(false)),
                    |_events, _state| panic!("payload must wait for native cleanup")
                )
                .failed_attempts,
            1
        );

        let mut retry_state = batch.pending_local_cleanup_state();
        let mut cleanup_calls = 0usize;
        assert!(batch.process_local_cleanup_with(
            &mut retry_state,
            |events, state| {
                assert!(events.is_empty());
                state.windows_input.send_events_with_sender(events, |records| {
                    cleanup_calls += 1;
                    assert_eq!(records.len(), 1);
                    Ok(1)
                })
            }
        ));
        assert_eq!(cleanup_calls, 1);
        assert!(!windows_input.has_pending_native_cleanup());

        let mut payload_attempts = Vec::new();
        let completed = batch.process_with(&mut retry_state, |events, state| {
            payload_attempts.extend_from_slice(events);
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
        assert_eq!(completed.completed_frames, 1);
        assert_eq!(payload_attempts, vec![payload]);
    }

    #[test]
    fn shutdown_cleanup_retries_are_bounded_and_keep_exact_suffix() {
        let key_up = core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Up,
            semantics: core_input::KeySemantics::Physical,
        };
        let mut injected = InjectedInputState::new(WindowsNumLockState::new(false));
        injected.observe(&[core_input::InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Down,
            semantics: core_input::KeySemantics::Physical,
        }]);
        let mut batch = BrokerInjectBatchState::default();
        batch.stage_local_cleanup(&mut injected);
        let mut attempts = 0usize;
        assert!(batch.process_local_cleanup_bounded_with(
            &mut injected,
            LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS,
            |events, state| {
                attempts += 1;
                assert_eq!(events, std::slice::from_ref(&key_up));
                let succeeds = attempts == LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS;
                observe_injected_input_outcome(
                    events,
                    state,
                    InputSendOutcome {
                        committed_event_count: usize::from(succeeds),
                        remaining_events: if succeeds {
                            Vec::new()
                        } else {
                            events.to_vec()
                        },
                        error: (!succeeds)
                            .then(|| anyhow::anyhow!("scripted shutdown cleanup failure")),
                    },
                )
            }
        ));
        assert_eq!(attempts, LOCAL_INPUT_CLEANUP_SHUTDOWN_ATTEMPTS);
        assert!(!batch.local_cleanup_pending());
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

fn uncertain_elevated_input_outcome(
    events: &[core_input::InputEvent],
) -> InputSendOutcome {
    InputSendOutcome {
        committed_event_count: 0,
        remaining_events: events.to_vec(),
        error: Some(anyhow::Error::new(ElevatedDeliveryUncertain)),
    }
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

fn ensure_direct_input_lane_available() -> Result<()> {
    if !platform_windows::elevated_input::direct_input_lane_available()
        .context("probe elevated input lane before direct injection")?
    {
        bail!("elevated input helper still owns the interactive input lane");
    }
    Ok(())
}

fn ready_helper_allows_direct_drain(status: &ElevatedInputControllerStatus) -> bool {
    status.direct_fallback_safe
        && status.state
            == platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle
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
