use std::time::{Duration, Instant};

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(all(windows, test))]
use std::sync::mpsc;

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use tokio::time;
use tracing::warn;

use chrono::Utc;

use core_input::{InputEvent, MAX_EVENTS_PER_FRAME, SwitchDirection};

use crate::{
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::{AppState, PendingInjectInputFrame, TransportEventRecord},
};

#[cfg(all(windows, test))]
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL,
};
#[cfg(all(windows, test))]
use windows_sys::Win32::UI::Input::{MOUSE_MOVE_ABSOLUTE, RAWMOUSE};

const INPUT_RUNTIME_SAFETY_TICK: Duration = Duration::from_millis(50);
#[cfg(windows)]
const DIRECT_INPUT_LOCK_LEASE: Duration = Duration::from_secs(2);
const INPUT_INJECT_MAX_FRAMES_PER_WAKE: usize = 64;
const INPUT_INJECT_WORK_QUANTUM: Duration = Duration::from_millis(2);
const INPUT_INJECT_RETRY_BASE_BACKOFF_MS: u64 = 12;
const INPUT_INJECT_RETRY_MAX_BACKOFF_MS: u64 = 160;
const INPUT_INJECT_MAX_RETRIES: u8 = 5;
const INPUT_INJECT_MAX_AGE_MS: i64 = 1_500;
const EDGE_PRESSURE_THRESHOLD: i32 = 300;
const EDGE_REMOTE_PRESSURE_THRESHOLD_MAX: i32 = 2400;
const EDGE_REMOTE_PRESSURE_THRESHOLD_NUMERATOR: i32 = 4;
const EDGE_REMOTE_PRESSURE_THRESHOLD_DENOMINATOR: i32 = 5;
const EDGE_POSITION_TOLERANCE_PX: i32 = 2;
const EDGE_SWITCH_POST_HANDOFF_SUPPRESS_MS: u64 = 220;
const ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS: u64 = 600;

mod backends;
mod edge_switch;
mod runtime;
#[cfg(windows)]
mod windows_hook_backend;

#[cfg(test)]
use edge_switch::{edge_switch_direction_from_motion, handoff_anchor_event};
use edge_switch::{
    filter_edge_start_replay_events, local_virtual_screen_bounds,
    maybe_handoff_capture_target_from_motion,
};
#[cfg(all(windows, test))]
use platform_windows::input::CaptureRuntime;
#[cfg(all(windows, test))]
use platform_windows::input::HookCaptureEvent;
#[cfg(all(windows, test))]
use platform_windows::input::raw_mouse_relative_delta;
#[cfg(test)]
#[cfg(windows)]
use platform_windows::input::send_input_records_with_sender;
#[cfg(windows)]
use platform_windows::input::{
    HookControlAction, HookInputPump, captured_key_virtual_keys,
    current_process_can_use_interactive_input, cursor_position, input_event_kind,
    input_records_for_event, is_virtual_key_down, mouse_button_from_virtual_key,
    mouse_button_virtual_keys, send_input_records, vk_to_scan_code,
};
#[cfg(all(test, not(windows)))]
use runtime::apply_frame;
#[cfg(test)]
use runtime::{capture_and_queue_outgoing_frames, drain_pending_inject_frames};
use runtime::{record_local_input_runtime_event, run};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureControlAction {
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    EscapeUnlock,
}

#[derive(Debug, Default)]
struct EdgeSwitchState {
    last_direction: Option<SwitchDirection>,
    x_pressure: i32,
    y_pressure: i32,
    suppress_until_instant: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct VirtualScreenBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputRuntimeMode {
    #[default]
    Auto,
    ServiceSessionUnsupported,
}

impl InputRuntimeMode {
    pub fn is_service_session_unsupported(self) -> bool {
        matches!(self, Self::ServiceSessionUnsupported)
    }
}

pub async fn apply_startup_mode(state: &AppState, mode: InputRuntimeMode) {
    if mode.is_service_session_unsupported() {
        state.input_broker_relay().mark_service_session_input();
        state.set_input_lock_runtime(false, false).await;
        state
            .set_input_capture_backend_mode(crate::state::SERVICE_SESSION_UNSUPPORTED_BACKEND_MODE)
            .await;
    }
}

pub fn start(state: AppState, mode: InputRuntimeMode) {
    let task_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "input.runtime",
            RuntimeTaskOwner::Input,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        async move {
            if let Err(error) = run(task_state, mode).await {
                warn!(error = ?error, "input runtime stopped");
            }
        },
    );
}

trait InputBackend: Send {
    fn apply(&mut self, event: &InputEvent) -> Result<()>;

    fn apply_frame(&mut self, events: &[InputEvent]) -> Result<()> {
        for event in events {
            self.apply(event)?;
        }
        Ok(())
    }
}

trait InputCaptureBackend: Send {
    fn drain_release_events(&mut self) -> Vec<InputEvent>;
    fn reset(&mut self);
    fn poll_events(&mut self) -> Result<Vec<InputEvent>>;
    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction>;
    fn set_lock_active(&mut self, active: bool) -> Result<bool>;
    fn safety_unlock_generation(&self) -> u64 {
        0
    }
    fn set_lock_active_if_safety_generation(
        &mut self,
        active: bool,
        _expected_generation: u64,
    ) -> Result<bool> {
        self.set_lock_active(active)
    }
    fn renew_lock_lease(&self) -> bool {
        false
    }
    fn lock_supported(&self) -> bool;
    fn backend_mode(&self) -> &'static str;
    fn cursor_position(&self) -> Option<(i32, i32)> {
        None
    }

    /// Bounds of the session that produced the captured events; `None` means
    /// fall back to this process's own virtual-screen metrics.
    fn virtual_screen_bounds(&self) -> Option<VirtualScreenBounds> {
        None
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        0
    }
}

fn input_backend(mode: InputRuntimeMode) -> Box<dyn InputBackend> {
    if mode.is_service_session_unsupported() {
        return service_session_unsupported_input_backend();
    }

    #[cfg(windows)]
    {
        if !windows_interactive_input_supported("input injection") {
            return service_session_unsupported_input_backend();
        }
        Box::new(WindowsInputBackend)
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopInputBackend)
    }
}

fn input_capture_backend(state: &AppState, mode: InputRuntimeMode) -> Box<dyn InputCaptureBackend> {
    if mode.is_service_session_unsupported() {
        return Box::new(BrokerRelayCaptureBackend {
            relay: state.input_broker_relay(),
        });
    }

    #[cfg(windows)]
    {
        if !windows_interactive_input_supported("input capture") {
            return service_session_unsupported_capture_backend();
        }
        match WindowsHookCaptureBackend::new(state) {
            Ok(backend) => Box::new(backend),
            Err(error) => {
                warn!(
                    error = ?error,
                    "failed to start low-level capture hooks; falling back to polling capture backend"
                );
                Box::new(WindowsPollingCaptureBackend::default())
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = state;
        Box::new(NoopCaptureBackend)
    }
}

#[cfg(any(test, windows))]
fn service_session_unsupported_input_backend() -> Box<dyn InputBackend> {
    Box::new(UnsupportedInteractiveInputBackend)
}

#[cfg(all(not(test), not(windows)))]
fn service_session_unsupported_input_backend() -> Box<dyn InputBackend> {
    Box::new(NoopInputBackend)
}

#[cfg(windows)]
fn service_session_unsupported_capture_backend() -> Box<dyn InputCaptureBackend> {
    Box::new(UnsupportedInteractiveCaptureBackend)
}

#[cfg(windows)]
fn windows_interactive_input_supported(surface: &str) -> bool {
    match current_process_can_use_interactive_input() {
        Ok(true) => true,
        Ok(false) => {
            warn!(
                surface,
                "Windows service session 0 cannot observe or inject interactive desktop input"
            );
            false
        }
        Err(error) => {
            warn!(
                surface,
                error = ?error,
                "failed to verify Windows process session for interactive input"
            );
            false
        }
    }
}

#[cfg(not(windows))]
struct NoopInputBackend;

#[cfg(not(windows))]
struct NoopCaptureBackend;

#[cfg(any(test, windows))]
struct UnsupportedInteractiveInputBackend;

#[cfg(any(test, windows))]
struct UnsupportedInteractiveCaptureBackend;

/// Capture backend for service mode: relays events pushed by the attached
/// user-session input broker and reports `service_session_unsupported`
/// whenever no fresh broker is attached.
struct BrokerRelayCaptureBackend {
    relay: std::sync::Arc<crate::state::InputBrokerRelay>,
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsInputBackend;

#[cfg(windows)]
#[derive(Default)]
struct WindowsPollingCaptureBackend {
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, bool>,
    last_button_down: HashMap<u16, bool>,
}

#[cfg(windows)]
struct WindowsHookCaptureBackend {
    pump: HookInputPump,
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::state::AppState;
    use core_input::{InputFrame, KeyState};

    #[cfg(not(windows))]
    #[test]
    fn noop_backend_accepts_events() {
        let mut backend = NoopInputBackend;
        let frame = PendingInjectInputFrame {
            peer_id: "peer-a".to_string(),
            sequence: 1,
            capture_timestamp_unix_ms: 1,
            received_timestamp_unix_ms: 2,
            queued_timestamp_unix_ms: 3,
            retry_count: 0,
            next_retry_at: None,
            events: vec![
                InputEvent::MouseMove { dx: 1, dy: -1 },
                InputEvent::Key {
                    scan_code: 30,
                    state: core_input::KeyState::Down,
                },
            ],
        };

        apply_frame(&mut backend, &frame).expect("noop backend should accept events");
    }

    #[test]
    fn unsupported_interactive_backend_rejects_injection_with_stable_reason() {
        let mut backend = UnsupportedInteractiveInputBackend;
        let frame = PendingInjectInputFrame {
            peer_id: "peer-a".to_string(),
            sequence: 1,
            capture_timestamp_unix_ms: 1,
            received_timestamp_unix_ms: 2,
            queued_timestamp_unix_ms: 3,
            retry_count: 0,
            next_retry_at: None,
            events: vec![InputEvent::MouseMove { dx: 1, dy: 0 }],
        };

        let error = backend
            .apply_frame(&frame.events)
            .expect_err("unsupported injection");
        assert!(
            error
                .to_string()
                .contains("interactive input unsupported from Windows service session 0"),
            "error should be actionable and stable"
        );
    }

    #[test]
    fn unsupported_interactive_capture_reports_no_lock_or_events() {
        let mut backend = UnsupportedInteractiveCaptureBackend;

        assert_eq!(backend.backend_mode(), "service_session_unsupported");
        assert!(!backend.lock_supported());
        assert!(
            !backend
                .set_lock_active(true)
                .expect("unsupported lock update should be non-fatal"),
            "unsupported capture must not report an active lock"
        );
        assert!(
            backend
                .poll_events()
                .expect("unsupported capture poll")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn safety_tick_wake_events_are_not_recorded_as_transport_events() {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-runtime-wake-noise-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        record_local_input_runtime_event(
            &state,
            "input_runtime_wake",
            "channel=all source=safety_tick",
            "none",
        )
        .await;
        record_local_input_runtime_event(
            &state,
            "input_runtime_wake",
            "channel=input_inject source=retry_deadline",
            "none",
        )
        .await;

        let events = state.transport_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "input_runtime_wake");
        assert_eq!(
            events[0].detail,
            "channel=input_inject source=retry_deadline"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    struct CountingBackend {
        applied: usize,
    }

    impl InputBackend for CountingBackend {
        fn apply(&mut self, _event: &InputEvent) -> Result<()> {
            self.applied += 1;
            Ok(())
        }
    }

    struct FailOnceBackend {
        fail_scan_code_once: u16,
        failed: bool,
        applied_scan_codes: Vec<u16>,
    }

    impl InputBackend for FailOnceBackend {
        fn apply(&mut self, event: &InputEvent) -> Result<()> {
            match event {
                InputEvent::Key { scan_code, .. }
                    if *scan_code == self.fail_scan_code_once && !self.failed =>
                {
                    self.failed = true;
                    Err(anyhow::anyhow!(
                        "scripted injection failure for scan code {scan_code}"
                    ))
                }
                InputEvent::Key { scan_code, .. } => {
                    self.applied_scan_codes.push(*scan_code);
                    Ok(())
                }
                _ => Ok(()),
            }
        }
    }

    struct ScriptedCaptureBackend {
        batches: VecDeque<Vec<InputEvent>>,
        release_events: Vec<InputEvent>,
        control_actions: VecDeque<CaptureControlAction>,
        lock_supported: bool,
        lock_active: bool,
        lock_updates: Vec<bool>,
        reset_count: usize,
        poll_count: usize,
    }

    impl ScriptedCaptureBackend {
        fn new(batches: Vec<Vec<InputEvent>>, release_events: Vec<InputEvent>) -> Self {
            Self {
                batches: VecDeque::from(batches),
                release_events,
                control_actions: VecDeque::new(),
                lock_supported: true,
                lock_active: false,
                lock_updates: Vec::new(),
                reset_count: 0,
                poll_count: 0,
            }
        }

        fn with_control_actions(mut self, actions: Vec<CaptureControlAction>) -> Self {
            self.control_actions = VecDeque::from(actions);
            self
        }
    }

    impl InputCaptureBackend for ScriptedCaptureBackend {
        fn drain_release_events(&mut self) -> Vec<InputEvent> {
            std::mem::take(&mut self.release_events)
        }

        fn reset(&mut self) {
            self.reset_count += 1;
        }

        fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
            self.poll_count += 1;
            Ok(self.batches.pop_front().unwrap_or_default())
        }

        fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
            self.control_actions.drain(..).collect()
        }

        fn set_lock_active(&mut self, active: bool) -> Result<bool> {
            self.lock_updates.push(active);
            if self.lock_supported {
                self.lock_active = active;
                Ok(active)
            } else {
                self.lock_active = false;
                Ok(false)
            }
        }

        fn lock_supported(&self) -> bool {
            self.lock_supported
        }

        fn backend_mode(&self) -> &'static str {
            "scripted"
        }
    }

    struct BlockingBrokerCaptureBackend {
        relay: std::sync::Arc<crate::state::InputBrokerRelay>,
        poll_entered: std::sync::mpsc::Sender<()>,
        continue_poll: std::sync::mpsc::Receiver<()>,
    }

    impl InputCaptureBackend for BlockingBrokerCaptureBackend {
        fn drain_release_events(&mut self) -> Vec<InputEvent> {
            self.relay.drain_release_events()
        }

        fn reset(&mut self) {
            self.relay.reset_capture_stream();
        }

        fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
            let events = self.relay.drain_captured_events();
            self.poll_entered.send(()).expect("signal captured batch");
            self.continue_poll.recv().expect("release captured batch");
            Ok(events)
        }

        fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
            Vec::new()
        }

        fn set_lock_active(&mut self, active: bool) -> Result<bool> {
            Ok(self.relay.set_desired_lock_active(active))
        }

        fn lock_supported(&self) -> bool {
            self.relay.lock_supported()
        }

        fn backend_mode(&self) -> &'static str {
            crate::state::INPUT_BROKER_BACKEND_MODE
        }
    }

    async fn state_with_peer_for_input_test() -> (AppState, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-runtime-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state = AppState::load_or_create_with_paths(config_path, security_root).expect("state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join");

        (state, peer_id, root)
    }

    #[tokio::test]
    async fn perf_probe_inject_wake_backlog_throughput() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner"),
            "owner claim should succeed"
        );

        let frame_count = 240u64;
        for sequence in 1..=frame_count {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms: Utc::now().timestamp_millis(),
                        events: vec![InputEvent::Key {
                            scan_code: sequence as u16,
                            state: KeyState::Down,
                        }],
                    },
                )
                .await
                .expect("route frame");
        }

        let mut backend = CountingBackend { applied: 0 };
        let started = Instant::now();
        drain_pending_inject_frames(&state, &mut backend).await;
        let elapsed = started.elapsed();
        let remaining = state.pending_inject_input_frame_count().await;

        eprintln!(
            "PERF_PROBE inject_wake frame_count={} processed={} remaining={} elapsed_us={}",
            frame_count,
            backend.applied,
            remaining,
            elapsed.as_micros()
        );

        assert!(backend.applied > 0, "probe must process at least one frame");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn drain_skips_frame_if_owner_changes_before_inject() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    }],
                },
            )
            .await
            .expect("route");

        assert!(state.release_input_owner(&peer_id).await, "release owner");

        let mut backend = CountingBackend { applied: 0 };
        drain_pending_inject_frames(&state, &mut backend).await;
        assert_eq!(backend.applied, 0, "stale owner frame must not be injected");
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "skipped stale frame should be dropped"
        );

        let events = state.transport_events().await;
        assert!(
            events
                .iter()
                .any(|event| event.kind == "input_inject_skipped" && event.peer_id == peer_id),
            "runtime should emit skipped event telemetry"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn drain_preserves_fifo_when_earlier_frame_retries() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        let frame_count = INPUT_INJECT_MAX_FRAMES_PER_WAKE + 1;
        let timestamp_unix_ms = Utc::now().timestamp_millis();
        for sequence in 1..=frame_count as u64 {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms,
                        events: vec![InputEvent::Key {
                            scan_code: sequence as u16,
                            state: KeyState::Down,
                        }],
                    },
                )
                .await
                .expect("route");
        }

        let mut backend = FailOnceBackend {
            fail_scan_code_once: 1,
            failed: false,
            applied_scan_codes: Vec::new(),
        };

        let first_outcome = drain_pending_inject_frames(&state, &mut backend).await;
        assert!(
            !first_outcome.continue_immediately,
            "retry backoff should prevent an immediate follow-up drain"
        );
        assert_eq!(
            backend.applied_scan_codes,
            Vec::<u16>::new(),
            "later frames must wait behind a failed earlier frame"
        );

        let early_retry_outcome = drain_pending_inject_frames(&state, &mut backend).await;
        assert!(
            !early_retry_outcome.continue_immediately,
            "retry backoff should still block follow-up drains before the deadline"
        );
        assert_eq!(
            backend.applied_scan_codes,
            Vec::<u16>::new(),
            "later frames must also wait during pre-deadline retry drains"
        );

        tokio::time::sleep(Duration::from_millis(
            INPUT_INJECT_RETRY_BASE_BACKOFF_MS + 5,
        ))
        .await;
        while state.pending_inject_input_frame_count().await > 0 {
            drain_pending_inject_frames(&state, &mut backend).await;
        }
        assert_eq!(
            backend.applied_scan_codes,
            (1..=frame_count as u16).collect::<Vec<_>>(),
            "retry must preserve FIFO order for non-coalescible frames"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn drain_caps_work_per_wake_and_leaves_remainder_queued() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner"),
            "owner claim should succeed"
        );

        let frame_count = INPUT_INJECT_MAX_FRAMES_PER_WAKE + 8;
        for sequence in 1..=frame_count as u64 {
            state
                .route_incoming_input_frame(
                    &peer_id,
                    InputFrame {
                        source_peer_id: peer_id.clone(),
                        sequence,
                        timestamp_unix_ms: Utc::now().timestamp_millis(),
                        events: vec![InputEvent::Key {
                            scan_code: sequence as u16,
                            state: KeyState::Down,
                        }],
                    },
                )
                .await
                .expect("route");
        }

        let mut backend = CountingBackend { applied: 0 };
        let first_outcome = drain_pending_inject_frames(&state, &mut backend).await;
        assert_eq!(
            backend.applied, INPUT_INJECT_MAX_FRAMES_PER_WAKE,
            "first wake should be bounded by inject frame budget"
        );
        assert_eq!(
            state.pending_inject_input_frame_count().await,
            8,
            "remaining frames should stay queued for the next wake"
        );
        assert!(
            first_outcome.continue_immediately,
            "ready backlog should request another immediate drain without waiting for a timer"
        );

        let second_outcome = drain_pending_inject_frames(&state, &mut backend).await;
        assert_eq!(
            backend.applied, frame_count,
            "second wake should drain the remaining frames"
        );
        assert_eq!(state.pending_inject_input_frame_count().await, 0);
        assert!(
            !second_outcome.continue_immediately,
            "once the backlog is drained there should be no immediate follow-up drain request"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn capture_queues_events_for_active_target_and_chunks_batches() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let events = (0..=MAX_EVENTS_PER_FRAME)
            .map(|index| InputEvent::Key {
                scan_code: index as u16 + 1,
                state: KeyState::Down,
            })
            .collect::<Vec<_>>();
        let mut backend = ScriptedCaptureBackend::new(vec![events], Vec::new());
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        let queued = state.drain_outgoing(&peer_id).await;
        assert_eq!(queued.len(), 2);
        assert!(matches!(
            queued.first(),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 1, events, .. }) if events.len() == MAX_EVENTS_PER_FRAME
        ));
        assert!(matches!(
            queued.get(1),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 2, events, .. }) if events.len() == 1
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn capture_resets_backend_when_target_becomes_inactive() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let mut backend = ScriptedCaptureBackend::new(vec![Vec::new(), Vec::new()], Vec::new());
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        let reset_after_set = backend.reset_count;
        assert!(
            reset_after_set >= 1,
            "initial target activation should reset capture backend"
        );

        state.clear_input_capture_target().await;
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        assert!(
            backend.reset_count > reset_after_set,
            "clearing target should reset capture backend"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn capture_drains_events_while_inactive() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        let mut backend = ScriptedCaptureBackend::new(
            vec![vec![InputEvent::MouseMove { dx: 5, dy: -3 }]],
            Vec::new(),
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert_eq!(
            backend.poll_count, 1,
            "inactive capture loop should drain backend events"
        );
        assert!(
            state.drain_outgoing(&peer_id).await.is_empty(),
            "inactive drain must not enqueue frames"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn capture_target_switch_flushes_release_events_to_previous_target() {
        let (state, peer_one, root) = state_with_peer_for_input_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let peer_two = state
            .join_peer(
                code,
                "127.0.0.1:15101".to_string(),
                Some("peer-two".to_string()),
            )
            .await
            .expect("join second peer");
        state
            .set_peer_connected(&peer_one, true)
            .await
            .expect("connect one");
        state
            .set_peer_connected(&peer_two, true)
            .await
            .expect("connect two");

        state
            .set_input_capture_target(Some(&peer_one))
            .await
            .expect("set target one");

        let mut backend = ScriptedCaptureBackend::new(
            vec![Vec::new(), Vec::new()],
            vec![
                InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: KeyState::Up,
                },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up,
                },
            ],
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        state
            .set_input_capture_target(Some(&peer_two))
            .await
            .expect("switch target");
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        let previous_outgoing = state.drain_outgoing(&peer_one).await;
        assert_eq!(previous_outgoing.len(), 1);
        assert!(matches!(
            previous_outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 1, events, .. }) if events.len() == 2
        ));
        assert!(
            state.drain_outgoing(&peer_two).await.is_empty(),
            "release events should flush to previous target only"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn capture_target_clear_flushes_release_events_to_previous_target() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let mut backend = ScriptedCaptureBackend::new(
            vec![Vec::new(), Vec::new()],
            vec![InputEvent::Key {
                scan_code: 42,
                state: KeyState::Up,
            }],
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        state.clear_input_capture_target().await;
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        let outgoing = state.drain_outgoing(&peer_id).await;
        assert_eq!(outgoing.len(), 1);
        assert!(matches!(
            outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 1, events, .. }) if matches!(
                events.as_slice(),
                [InputEvent::Key { scan_code: 42, state: KeyState::Up }]
            )
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn edge_switch_handoff_updates_capture_target_from_layout() {
        let (state, left_peer, root) = state_with_peer_for_input_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let right_peer = state
            .join_peer(
                code,
                "127.0.0.1:15101".to_string(),
                Some("right".to_string()),
            )
            .await
            .expect("join right peer");

        state
            .set_peer_connected(&left_peer, true)
            .await
            .expect("connect left");
        state
            .set_peer_connected(&right_peer, true)
            .await
            .expect("connect right");
        state
            .set_layout("peer,right,self".to_string())
            .await
            .expect("set layout");
        state
            .set_input_capture_target(Some(&left_peer))
            .await
            .expect("set initial target");

        let mut backend = ScriptedCaptureBackend::new(
            vec![
                Vec::new(),
                vec![InputEvent::MouseMove {
                    dx: EDGE_PRESSURE_THRESHOLD,
                    dy: 0,
                }],
                Vec::new(),
            ],
            vec![InputEvent::Key {
                scan_code: 30,
                state: KeyState::Up,
            }],
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(right_peer.as_str()),
            "right-edge switch should hand off capture target to right layout neighbor"
        );
        let left_outgoing = state.drain_outgoing(&left_peer).await;
        assert_eq!(left_outgoing.len(), 2);
        assert!(matches!(
            left_outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 1, events, .. }) if matches!(
                events.as_slice(),
                [InputEvent::MouseMove { dx, dy }] if *dx == EDGE_PRESSURE_THRESHOLD && *dy == 0
            )
        ));
        assert!(matches!(
            left_outgoing.get(1),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 2, events, .. }) if matches!(
                events.as_slice(),
                [InputEvent::Key { scan_code: 30, state: KeyState::Up }]
            )
        ));
        let right_outgoing = state.drain_outgoing(&right_peer).await;
        assert!(matches!(
            right_outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { sequence: 1, events, .. })
                if matches!(events.as_slice(), [InputEvent::MouseMoveAbsolute { .. }])
        ));
        assert_eq!(
            right_outgoing.len(),
            1,
            "new target should receive only anchor event in this scenario"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn edge_switch_handoff_respects_easy_mouse_toggle() {
        let (state, left_peer, root) = state_with_peer_for_input_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let right_peer = state
            .join_peer(
                code,
                "127.0.0.1:15101".to_string(),
                Some("right".to_string()),
            )
            .await
            .expect("join right peer");

        state
            .set_peer_connected(&left_peer, true)
            .await
            .expect("connect left");
        state
            .set_peer_connected(&right_peer, true)
            .await
            .expect("connect right");
        state
            .set_layout("peer,self,right".to_string())
            .await
            .expect("set layout");
        state
            .set_input_capture_target(Some(&left_peer))
            .await
            .expect("set initial target");
        state
            .set_feature("easy_mouse".to_string(), false)
            .await
            .expect("disable easy mouse");

        let mut backend = ScriptedCaptureBackend::new(
            vec![
                Vec::new(),
                vec![InputEvent::MouseMove {
                    dx: EDGE_PRESSURE_THRESHOLD,
                    dy: 0,
                }],
            ],
            Vec::new(),
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(left_peer.as_str()),
            "edge handoff must not run when easy_mouse is disabled"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn edge_switch_handoff_starts_capture_when_inactive() {
        let (state, left_peer, root) = state_with_peer_for_input_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let right_peer = state
            .join_peer(
                code,
                "127.0.0.1:15101".to_string(),
                Some("right".to_string()),
            )
            .await
            .expect("join right peer");

        state
            .set_peer_connected(&left_peer, true)
            .await
            .expect("connect left");
        state
            .set_peer_connected(&right_peer, true)
            .await
            .expect("connect right");
        state
            .set_layout("peer,self,right".to_string())
            .await
            .expect("set layout");
        state.clear_input_capture_target().await;

        let mut backend = ScriptedCaptureBackend::new(
            vec![vec![InputEvent::MouseMove {
                dx: EDGE_PRESSURE_THRESHOLD,
                dy: 0,
            }]],
            Vec::new(),
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(right_peer.as_str()),
            "edge handoff should auto-start capture from local when pushing into configured edge"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn edge_switch_handoff_from_local_anchors_without_replaying_trigger_motion() {
        let (state, _left_peer, root) = state_with_peer_for_input_test().await;
        let (code, _) = state.create_pairing_code(120).await;
        let right_peer = state
            .join_peer(
                code,
                "127.0.0.1:15101".to_string(),
                Some("right".to_string()),
            )
            .await
            .expect("join right peer");

        state
            .set_peer_connected(&right_peer, true)
            .await
            .expect("connect right");
        state
            .set_layout("self,right".to_string())
            .await
            .expect("set layout");
        state.clear_input_capture_target().await;

        let mut backend = ScriptedCaptureBackend::new(
            vec![vec![InputEvent::MouseMove {
                dx: EDGE_PRESSURE_THRESHOLD,
                dy: 0,
            }]],
            Vec::new(),
        );
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        let outgoing = state.drain_outgoing(&right_peer).await;
        assert_eq!(
            outgoing.len(),
            1,
            "local edge-start handoff should only emit an anchor on first frame"
        );
        assert!(matches!(
            outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { events, .. })
                if matches!(events.as_slice(), [InputEvent::MouseMoveAbsolute { .. }])
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn remote_edge_handoff_uses_screen_scaled_pressure_threshold() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };

        let direction = edge_switch_direction_from_motion(
            &[InputEvent::MouseMove {
                dx: EDGE_PRESSURE_THRESHOLD,
                dy: 0,
            }],
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            Some("peer-a"),
            None,
            Some(bounds),
        );
        assert_eq!(
            direction, None,
            "active remote capture should require stronger pressure than local edge start"
        );

        let direction = edge_switch_direction_from_motion(
            &[InputEvent::MouseMove { dx: 2000, dy: 0 }],
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            Some("peer-a"),
            None,
            Some(bounds),
        );
        assert_eq!(
            direction,
            Some(SwitchDirection::Right),
            "strong sustained pressure should still allow handoff when actively controlling a peer"
        );
    }

    #[test]
    fn local_edge_handoff_requires_cursor_at_boundary() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let events = vec![InputEvent::MouseMove {
            dx: EDGE_PRESSURE_THRESHOLD,
            dy: 0,
        }];
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };

        let direction = edge_switch_direction_from_motion(
            &events,
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            None,
            Some((960, 540)),
            Some(bounds),
        );
        assert_eq!(
            direction, None,
            "local capture start must ignore pressure when cursor is not at an edge"
        );
    }

    #[test]
    fn local_edge_handoff_accepts_cursor_at_boundary() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let events = vec![InputEvent::MouseMove {
            dx: EDGE_PRESSURE_THRESHOLD,
            dy: 0,
        }];
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };

        let direction = edge_switch_direction_from_motion(
            &events,
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            None,
            Some((1919, 540)),
            Some(bounds),
        );
        assert_eq!(
            direction,
            Some(SwitchDirection::Right),
            "local capture start should trigger when pressure points into a configured edge"
        );
    }

    #[test]
    fn local_edge_handoff_accepts_small_push_at_boundary() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let events = vec![InputEvent::MouseMove { dx: 5, dy: 0 }];
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };

        let direction = edge_switch_direction_from_motion(
            &events,
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            None,
            Some((1919, 540)),
            Some(bounds),
        );
        assert_eq!(
            direction,
            Some(SwitchDirection::Right),
            "local capture start should work with a small push once cursor is at the edge"
        );
    }

    #[test]
    fn local_edge_handoff_blocks_configured_corners() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let events = vec![InputEvent::MouseMove { dx: 5, dy: 0 }];
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };

        let direction = edge_switch_direction_from_motion(
            &events,
            &mut edge_switch_state,
            false,
            &crate::config::InputHandoffConfig::default(),
            None,
            Some((1919, 8)),
            Some(bounds),
        );
        assert_eq!(
            direction, None,
            "corner blocking should prevent accidental edge handoff near the top-right corner"
        );
    }

    #[test]
    fn local_edge_handoff_allows_corners_when_corner_blocking_disabled() {
        let mut edge_switch_state = EdgeSwitchState::default();
        let events = vec![InputEvent::MouseMove { dx: 5, dy: 0 }];
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };
        let policy = crate::config::InputHandoffConfig {
            block_screen_corners: false,
            ..crate::config::InputHandoffConfig::default()
        };

        let direction = edge_switch_direction_from_motion(
            &events,
            &mut edge_switch_state,
            false,
            &policy,
            None,
            Some((1919, 8)),
            Some(bounds),
        );
        assert_eq!(direction, Some(SwitchDirection::Right));
    }

    #[test]
    fn handoff_anchor_places_cursor_on_destination_edge() {
        let bounds = VirtualScreenBounds {
            left: 0,
            top: 0,
            right: 1919,
            bottom: 1079,
        };
        let anchor = handoff_anchor_event(SwitchDirection::Right, Some((1919, 540)), Some(bounds));
        assert!(matches!(
            anchor,
            InputEvent::MouseMoveAbsolute { x_norm, y_norm } if x_norm == 0 && y_norm > 32000
        ));
    }

    #[tokio::test]
    async fn capture_escape_action_clears_target_and_unlocks() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let mut backend = ScriptedCaptureBackend::new(vec![Vec::new()], Vec::new())
            .with_control_actions(vec![CaptureControlAction::EscapeUnlock]);
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert!(
            state.input_capture_target().await.is_none(),
            "escape control action should clear capture target"
        );
        let (locked, _) = state.input_lock_runtime().await;
        assert!(!locked, "escape control action should release lock");
        assert!(
            !backend.lock_updates.contains(&true),
            "a pending safety action must be drained before any relock attempt"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn escape_unlock_suppresses_immediate_edge_recapture() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        state
            .set_layout("self,peer".to_string())
            .await
            .expect("set layout");
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let mut backend = ScriptedCaptureBackend::new(
            vec![vec![InputEvent::MouseMove {
                dx: EDGE_PRESSURE_THRESHOLD,
                dy: 0,
            }]],
            Vec::new(),
        )
        .with_control_actions(vec![CaptureControlAction::EscapeUnlock]);
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        assert!(
            state.input_capture_target().await.is_none(),
            "escape should not be immediately undone by same-tick edge movement"
        );
        let (locked, _) = state.input_lock_runtime().await;
        assert!(!locked, "escape should leave lock disengaged");

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn maps_key_event_to_scan_code_record() {
        let records = input_records_for_event(&InputEvent::Key {
            scan_code: 30,
            state: core_input::KeyState::Down,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].r#type, INPUT_KEYBOARD);

        let record = unsafe { records[0].Anonymous.ki };
        assert_eq!(record.wScan, 30);
        assert_eq!(record.dwFlags & KEYEVENTF_SCANCODE, KEYEVENTF_SCANCODE);
        assert_eq!(record.dwFlags & KEYEVENTF_KEYUP, 0);
    }

    #[cfg(windows)]
    #[test]
    fn maps_extended_scan_code_with_extended_flag() {
        let records = input_records_for_event(&InputEvent::Key {
            scan_code: 0xE04D,
            state: core_input::KeyState::Down,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].r#type, INPUT_KEYBOARD);

        let record = unsafe { records[0].Anonymous.ki };
        assert_eq!(record.wScan, 0x4D);
        assert_eq!(
            record.dwFlags & KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_EXTENDEDKEY
        );
    }

    #[cfg(windows)]
    #[test]
    fn maps_e1_prefixed_scan_code_with_extended_flag() {
        let records = input_records_for_event(&InputEvent::Key {
            scan_code: 0xE11D,
            state: core_input::KeyState::Down,
        });
        assert_eq!(records.len(), 1);

        let record = unsafe { records[0].Anonymous.ki };
        assert_eq!(record.wScan, 0x1D);
        assert_eq!(
            record.dwFlags & KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_EXTENDEDKEY
        );
    }

    #[cfg(windows)]
    #[test]
    fn maps_wheel_event_to_two_records_when_both_axes_present() {
        let records = input_records_for_event(&InputEvent::MouseWheel {
            delta_x: 120,
            delta_y: -120,
        });
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].r#type, INPUT_MOUSE);
        assert_eq!(records[1].r#type, INPUT_MOUSE);

        let vertical = unsafe { records[0].Anonymous.mi };
        let horizontal = unsafe { records[1].Anonymous.mi };
        assert_eq!(vertical.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(horizontal.dwFlags, MOUSEEVENTF_HWHEEL);
    }

    #[cfg(windows)]
    #[test]
    fn maps_absolute_move_event_to_absolute_mouse_record() {
        let records = input_records_for_event(&InputEvent::MouseMoveAbsolute {
            x_norm: 1234,
            y_norm: 5678,
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].r#type, INPUT_MOUSE);

        let record = unsafe { records[0].Anonymous.mi };
        assert_eq!(record.dx, 1234);
        assert_eq!(record.dy, 5678);
        assert_eq!(
            record.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
    }

    #[cfg(windows)]
    #[test]
    fn send_input_records_with_sender_batches_records_per_call() {
        let records = input_records_for_event(&InputEvent::MouseWheel {
            delta_x: 120,
            delta_y: -120,
        });
        let mut call_count = 0usize;

        send_input_records_with_sender(&records, |chunk| {
            call_count += 1;
            assert_eq!(chunk.len(), records.len());
            Ok(chunk.len() as u32)
        })
        .expect("send should succeed");

        assert_eq!(call_count, 1);
    }

    #[cfg(windows)]
    #[test]
    fn send_input_records_with_sender_stops_after_first_failed_record() {
        let records = input_records_for_event(&InputEvent::MouseWheel {
            delta_x: 120,
            delta_y: -120,
        });
        let mut call_count = 0usize;

        let err = send_input_records_with_sender(&records, |_chunk| {
            call_count += 1;
            if call_count == 1 { Ok(1) } else { Ok(0) }
        })
        .expect_err("second record failure should surface");

        assert_eq!(call_count, 2, "must not replay successfully sent prefix");
        assert!(err.to_string().contains("index 1"));
    }

    fn allowed_broker_client() -> Option<crate::state::InputBrokerClientIdentity> {
        Some(crate::state::InputBrokerClientIdentity {
            user_sid: Some("S-1-5-21-1000-2000-3000-1001".to_string()),
            session_id: Some(2),
        })
    }

    #[tokio::test]
    async fn broker_relay_backend_reports_ready_only_while_attached() {
        let (state, _peer_id, root) = state_with_peer_for_input_test().await;
        apply_startup_mode(&state, InputRuntimeMode::ServiceSessionUnsupported).await;
        state.set_input_broker_allowed_user_sid("S-1-5-21-1000-2000-3000-1001");
        let mut backend = BrokerRelayCaptureBackend {
            relay: state.input_broker_relay(),
        };

        assert_eq!(backend.backend_mode(), "service_session_unsupported");
        assert!(!backend.lock_supported());

        let attach = state
            .attach_input_broker(allowed_broker_client(), "test-broker".to_string(), true)
            .await;
        assert!(attach.accepted);
        assert_eq!(
            backend.backend_mode(),
            "user_session_broker",
            "broker-ready state must appear only after a broker attaches"
        );
        assert!(backend.lock_supported());

        state.input_broker_relay().expire_attachment_for_test();
        assert_eq!(
            backend.backend_mode(),
            "service_session_unsupported",
            "a stale broker must revert to the truthful unsupported state"
        );
        assert!(!backend.lock_supported());
        assert!(
            backend.poll_events().expect("poll").is_empty(),
            "stale broker events must not leak into the capture path"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn broker_relayed_events_route_to_connected_capture_target() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        apply_startup_mode(&state, InputRuntimeMode::ServiceSessionUnsupported).await;
        state.set_input_broker_allowed_user_sid("S-1-5-21-1000-2000-3000-1001");

        let attach = state
            .attach_input_broker(allowed_broker_client(), "test-broker".to_string(), true)
            .await;
        assert!(attach.accepted);
        // The runtime loop republishes the backend mode when it flips; mirror
        // that transition before the capture pass.
        state
            .set_input_capture_backend_mode(crate::state::INPUT_BROKER_BACKEND_MODE)
            .await;
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");

        let mut backend = BrokerRelayCaptureBackend {
            relay: state.input_broker_relay(),
        };
        let mut last_target = None;
        let mut edge_switch_state = EdgeSwitchState::default();
        // First pass latches the new capture target (and resets the relay
        // stream, as any backend reset does on a target change).
        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        let outcome = state
            .exchange_input_broker(
                allowed_broker_client(),
                &attach.broker_token,
                crate::state::InputBrokerExchangeObservations {
                    captured_events: vec![InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    }],
                    ..Default::default()
                },
            )
            .await;
        assert!(outcome.accepted);

        capture_and_queue_outgoing_frames(
            &state,
            &mut backend,
            &mut last_target,
            &mut edge_switch_state,
        )
        .await;

        let outgoing = state.drain_outgoing(&peer_id).await;
        assert_eq!(
            outgoing.len(),
            1,
            "broker-relayed events must flow through the trusted per-peer queue"
        );
        assert!(matches!(
            outgoing.first(),
            Some(crate::state::OutboundPayload::InputFrame { events, .. }) if matches!(
                events.as_slice(),
                [InputEvent::Key { scan_code: 30, state: KeyState::Down }]
            )
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn broker_detach_orders_final_release_after_in_flight_capture_batch() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect");
        apply_startup_mode(&state, InputRuntimeMode::ServiceSessionUnsupported).await;
        state.set_input_broker_allowed_user_sid("S-1-5-21-1000-2000-3000-1001");
        let attach = state
            .attach_input_broker(allowed_broker_client(), "test-broker".to_string(), true)
            .await;
        assert!(attach.accepted);
        state
            .set_input_capture_backend_mode(crate::state::INPUT_BROKER_BACKEND_MODE)
            .await;
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set target");
        let _ = state.drain_outgoing(&peer_id).await;

        let observed = state
            .exchange_input_broker(
                allowed_broker_client(),
                &attach.broker_token,
                crate::state::InputBrokerExchangeObservations {
                    captured_events: vec![InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    }],
                    ..Default::default()
                },
            )
            .await;
        assert!(observed.accepted);

        let (poll_entered_tx, poll_entered_rx) = std::sync::mpsc::channel();
        let (continue_poll_tx, continue_poll_rx) = std::sync::mpsc::channel();
        let capture_state = state.clone();
        let capture_peer = peer_id.clone();
        let capture = tokio::spawn(async move {
            let mut backend = BlockingBrokerCaptureBackend {
                relay: capture_state.input_broker_relay(),
                poll_entered: poll_entered_tx,
                continue_poll: continue_poll_rx,
            };
            let mut last_target = Some(capture_peer);
            let mut edge_switch_state = EdgeSwitchState::default();
            capture_and_queue_outgoing_frames(
                &capture_state,
                &mut backend,
                &mut last_target,
                &mut edge_switch_state,
            )
            .await;
        });

        tokio::task::spawn_blocking(move || {
            poll_entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("capture pass must drain the broker batch")
        })
        .await
        .expect("join poll barrier");

        let detach_state = state.clone();
        let broker_token = attach.broker_token.clone();
        let mut detach = tokio::spawn(async move {
            detach_state
                .detach_input_broker(allowed_broker_client(), &broker_token)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut detach)
                .await
                .is_err(),
            "detach must wait until the already-drained capture batch is queued"
        );

        continue_poll_tx.send(()).expect("release capture pass");
        capture.await.expect("capture pass");
        assert!(detach.await.expect("detach task"));

        let routed_events = state
            .drain_outgoing(&peer_id)
            .await
            .into_iter()
            .filter_map(|payload| match payload {
                crate::state::OutboundPayload::InputFrame { events, .. } => Some(events),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(
            routed_events,
            vec![
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up,
                },
            ],
            "the final release must be ordered after any pre-detach captured down event"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn direct_hook_backend_watchdog_fails_open_after_stalled_capture_cycle() {
        let (_tx, rx) = mpsc::channel();
        let mut backend = WindowsHookCaptureBackend {
            pump: HookInputPump::from_capture_runtime(CaptureRuntime::from_test_parts(rx, true)),
        };
        backend
            .pump
            .enable_lock_lease(Duration::from_millis(40))
            .expect("enable direct hook lock lease");
        let generation = backend.safety_unlock_generation();
        assert!(
            backend
                .set_lock_active_if_safety_generation(true, generation)
                .expect("engage guarded lock")
        );

        std::thread::sleep(Duration::from_millis(140));

        assert!(!backend.pump.lock_active(), "stalled cycle must fail open");
        assert_eq!(
            backend.drain_control_actions(),
            vec![CaptureControlAction::EscapeUnlock]
        );
        assert!(
            !backend
                .set_lock_active_if_safety_generation(true, generation)
                .expect("stale guarded relock"),
            "the pre-stall generation must not relock after watchdog expiry"
        );
    }

    #[cfg(windows)]
    #[test]
    fn hook_backend_flushes_mouse_move_before_button_event() {
        let (tx, rx) = mpsc::channel();
        let mut backend = WindowsHookCaptureBackend {
            pump: HookInputPump::from_capture_runtime(CaptureRuntime::from_test_parts(rx, true)),
        };

        tx.send(HookCaptureEvent::MouseDelta { dx: 9, dy: -4 })
            .expect("send mouse delta");
        tx.send(HookCaptureEvent::Input(InputEvent::MouseButton {
            button: core_input::MouseButton::Left,
            state: KeyState::Down,
        }))
        .expect("send mouse button");

        let events = backend.poll_events().expect("poll");
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::MouseMove { dx, dy },
                InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: KeyState::Down
                }
            ] if *dx == 9 && *dy == -4
        ));
    }

    #[cfg(windows)]
    #[test]
    fn raw_mouse_relative_delta_ignores_absolute_packets() {
        let relative = RAWMOUSE {
            usFlags: 0,
            lLastX: 12,
            lLastY: -7,
            ..Default::default()
        };
        assert_eq!(raw_mouse_relative_delta(&relative), Some((12, -7)));

        let absolute = RAWMOUSE {
            usFlags: MOUSE_MOVE_ABSOLUTE,
            lLastX: 1200,
            lLastY: 800,
            ..Default::default()
        };
        assert_eq!(raw_mouse_relative_delta(&absolute), None);
    }

    #[cfg(windows)]
    #[test]
    fn hook_backend_uses_mouse_position_when_unlocked_with_raw_mode() {
        let (tx, rx) = mpsc::channel();
        let mut backend = WindowsHookCaptureBackend {
            pump: HookInputPump::from_capture_runtime(CaptureRuntime::from_test_parts(rx, true)),
        };

        tx.send(HookCaptureEvent::MousePosition { x: 100, y: 100 })
            .expect("send first position");
        tx.send(HookCaptureEvent::MousePosition { x: 130, y: 90 })
            .expect("send second position");

        let events = backend.poll_events().expect("poll");
        assert!(matches!(
            events.as_slice(),
            [InputEvent::MouseMove { dx, dy }] if *dx == 30 && *dy == -10
        ));
    }

    #[cfg(windows)]
    #[test]
    fn hook_backend_preserves_repeated_key_down_events() {
        let (tx, rx) = mpsc::channel();
        let mut backend = WindowsHookCaptureBackend {
            pump: HookInputPump::from_capture_runtime(CaptureRuntime::from_test_parts(rx, false)),
        };

        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 30,
            state: KeyState::Down,
        }))
        .expect("send key down 1");
        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 30,
            state: KeyState::Down,
        }))
        .expect("send key down 2");
        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 30,
            state: KeyState::Up,
        }))
        .expect("send key up");
        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 30,
            state: KeyState::Up,
        }))
        .expect("send duplicate key up");

        let events = backend.poll_events().expect("poll");
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down
                },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down
                },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Up
                }
            ]
        ));
    }
}
