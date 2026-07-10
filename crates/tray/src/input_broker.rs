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
    ClipboardBrokerLocalPayloadDisposition, InputBrokerAttachRequest, InputBrokerDetachRequest,
    InputBrokerExchangeRequest,
    clipboard_broker_payload, control_plane_service_client::ControlPlaneServiceClient,
};
use ipc_api::broker_events::{broker_events_from_input_events, input_events_from_broker_events};
use platform_windows::clipboard_backend::WindowsClipboardBackend;
use platform_windows::input::{
    HookControlAction, HookInputPump, current_process_can_use_interactive_input,
    input_records_for_events, send_input_records, virtual_screen_bounds,
};
use tonic::transport::Channel;

const INPUT_BROKER_SERVICE_UNSUPPORTED_MODE: &str = "service_session_unsupported";
const INPUT_BROKER_SUPERVISOR_RETRY: Duration = Duration::from_secs(3);
const INPUT_BROKER_ACTIVE_POLL: Duration = Duration::from_millis(8);
const INPUT_BROKER_IDLE_POLL: Duration = Duration::from_millis(40);
const INPUT_BROKER_LOCK_LEASE: Duration = Duration::from_secs(2);
const CLIPBOARD_BROKER_POLL: Duration = Duration::from_millis(200);
const CLIPBOARD_BROKER_RETRY: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
struct SafetyUnlockReconciler {
    pending_report_count: u32,
    waiting_for_daemon_release: bool,
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

#[derive(Debug, Default)]
struct InjectedInputState {
    pressed_keys: Vec<(u16, core_input::KeySemantics)>,
    pressed_buttons: Vec<core_input::MouseButton>,
}

impl InjectedInputState {
    fn observe(&mut self, events: &[core_input::InputEvent]) {
        for event in events {
            match event {
                core_input::InputEvent::Key {
                    scan_code,
                    state,
                    semantics,
                } => match state {
                    core_input::KeyState::Down => {
                        if let Some((_, pressed_semantics)) = self
                            .pressed_keys
                            .iter_mut()
                            .find(|(pressed_scan_code, _)| pressed_scan_code == scan_code)
                        {
                            *pressed_semantics = *semantics;
                        } else {
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
        let records = input_records_for_events(&releases);
        send_input_records(&records)
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
        for _ in actions {
            self.pending_report_count = self.pending_report_count.saturating_add(1);
            self.waiting_for_daemon_release = true;
        }
    }

    fn report_count(&self) -> u32 {
        self.pending_report_count
    }

    fn mark_report_delivered(&mut self) {
        self.pending_report_count = 0;
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
        daemon_lock
    }
}

enum BrokerSessionEnd {
    NotNeeded,
    Detached,
    Shutdown,
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
            }) => result?,
        _ = wait_for_broker_shutdown(&mut shutdown_rx) => return Ok(BrokerSessionEnd::Shutdown),
    }
    .into_inner();
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
    let mut injected_state = InjectedInputState::default();
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

    loop {
        safety_unlock.observe(pump.drain_control_actions());
        let captured = pump.poll_events();
        let captured = if safety_unlock.should_forward_captured_events() {
            captured
        } else {
            Vec::new()
        };
        let cursor = pump
            .cursor_position()
            .or_else(|| platform_windows::input::cursor_position().ok().flatten());
        let bounds = virtual_screen_bounds();
        let wheel_sources = pump.take_wheel_source_counts();

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
                escape_unlock_count: safety_unlock.report_count(),
                lock_active: pump.lock_active(),
                dropped_event_count: pump.take_dropped_event_count(),
                injected_frame_count,
                inject_failure_count,
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
            if let Err(error) = result {
                eprintln!(
                    "boundless input broker failed to update local input lock: {error:#}"
                );
            }
        }

        let had_inject_frames = !reply.inject_frames.is_empty();
        for frame in &reply.inject_frames {
            let (events, undecodable) = input_events_from_broker_events(&frame.events);
            if undecodable > 0 || inject_input_events(&events, injected_state).is_err() {
                inject_failure_count = inject_failure_count.saturating_add(1);
            } else {
                injected_frame_count = injected_frame_count.saturating_add(1);
            }
        }

        let poll = if reply.capture_active || had_inject_frames || !captured.is_empty() {
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
    fn safety_unlock_suppresses_stale_relock_until_daemon_reconciles() {
        let mut state = SafetyUnlockReconciler::default();
        state.observe(vec![HookControlAction::EscapeUnlock]);

        assert_eq!(state.report_count(), 1);
        assert!(!state.should_forward_captured_events());
        state.mark_report_delivered();
        assert_eq!(state.report_count(), 0);
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
        let submitted = state.report_count();
        assert_eq!(submitted, 1);
        state.mark_report_delivered();

        state.observe(vec![HookControlAction::LeaseExpiredUnlock]);
        assert_eq!(state.report_count(), 1);
        assert!(!state.lock_should_be_active(true, true));
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
        let mut state = InjectedInputState::default();
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

fn inject_input_events(
    events: &[core_input::InputEvent],
    injected_state: &mut InjectedInputState,
) -> Result<()> {
    let records = input_records_for_events(events);
    send_input_records(&records)?;
    injected_state.observe(events);
    Ok(())
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
