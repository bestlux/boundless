use std::time::Duration;

#[cfg(windows)]
use std::collections::VecDeque;

#[cfg(windows)]
use std::{
    collections::HashMap,
    sync::mpsc,
    thread::{self, JoinHandle},
};

use anyhow::Result;
#[cfg(windows)]
use anyhow::{Context, bail};
use tokio::time;
use tracing::warn;

use chrono::Utc;

use core_input::{InputEvent, MAX_EVENTS_PER_FRAME, SwitchDirection};

use crate::state::{AppState, PendingInjectInputFrame, TransportEventRecord};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
            MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput,
        },
        Input::{
            GetRawInputData, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
            RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetCursorPos,
            GetMessageW, HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
            WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
        },
    },
};

const INPUT_TICK: Duration = Duration::from_millis(5);
const INPUT_CAPTURE_TICK: Duration = Duration::from_millis(8);
const EDGE_PRESSURE_THRESHOLD: i32 = 300;
const EDGE_REMOTE_PRESSURE_THRESHOLD_MAX: i32 = 2400;
const EDGE_REMOTE_PRESSURE_THRESHOLD_NUMERATOR: i32 = 4;
const EDGE_REMOTE_PRESSURE_THRESHOLD_DENOMINATOR: i32 = 5;
const EDGE_POSITION_TOLERANCE_PX: i32 = 2;
const EDGE_SWITCH_POST_HANDOFF_SUPPRESS_MS: u64 = 220;
const ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS: u64 = 600;
#[cfg(windows)]
const ESCAPE_DOUBLE_CTRL_WINDOW_MS: u64 = 400;
#[cfg(windows)]
const RAW_INPUT_USAGE_PAGE_GENERIC: u16 = 0x01;
#[cfg(windows)]
const RAW_INPUT_USAGE_MOUSE: u16 = 0x02;
#[cfg(windows)]
const STATIC_WINDOW_CLASS_NAME: [u16; 7] = [83, 84, 65, 84, 73, 67, 0];
#[cfg(windows)]
const EMPTY_WINDOW_NAME: [u16; 1] = [0];

mod backends;
mod edge_switch;
mod runtime;
#[cfg(windows)]
mod windows_hook_backend;
#[cfg(windows)]
mod windows_hook_runtime;
#[cfg(windows)]
mod windows_hooks;
#[cfg(windows)]
mod windows_inject;
#[cfg(windows)]
mod windows_raw_input;

use edge_switch::{
    edge_switch_direction_from_motion, filter_edge_start_replay_events, handoff_anchor_event,
    local_virtual_screen_bounds, maybe_handoff_capture_target_from_motion, unix_now_ms,
};
use runtime::{
    capture_and_queue_outgoing_frames, drain_pending_inject_frames, record_local_input_runtime_event,
    run,
};
#[cfg(all(test, not(windows)))]
use runtime::apply_frame;
#[cfg(windows)]
use windows_hook_runtime::{
    HookSenderGuard, captured_key_virtual_keys, is_hook_lock_active, mouse_button_from_virtual_key,
    mouse_button_virtual_keys, send_hook_event, set_hook_event_sender, set_hook_lock_active,
    update_escape_state_for_key, virtual_key_for_mouse_button,
};
#[cfg(windows)]
use windows_hooks::{install_keyboard_hook, install_mouse_hook, run_hook_message_loop};
#[cfg(windows)]
use windows_inject::{
    cursor_position, high_word, input_event_kind, input_records_for_event, is_virtual_key_down,
    send_input_records, send_input_records_with_sender, signed_high_word, vk_to_scan_code,
};
#[cfg(windows)]
use windows_raw_input::{raw_mouse_relative_delta, spawn_raw_input_thread};

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
    suppress_until_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct VirtualScreenBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "input runtime stopped");
        }
    });
}

trait InputBackend: Send {
    fn apply(&mut self, event: &InputEvent) -> Result<()>;
}

trait InputCaptureBackend: Send {
    fn drain_release_events(&mut self) -> Vec<InputEvent>;
    fn reset(&mut self);
    fn poll_events(&mut self) -> Result<Vec<InputEvent>>;
    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction>;
    fn set_lock_active(&mut self, active: bool) -> Result<bool>;
    fn lock_supported(&self) -> bool;
    fn backend_mode(&self) -> &'static str;
    fn cursor_position(&self) -> Option<(i32, i32)> {
        None
    }
}

fn input_backend() -> Box<dyn InputBackend> {
    #[cfg(windows)]
    {
        Box::new(WindowsInputBackend)
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopInputBackend)
    }
}

fn input_capture_backend() -> Box<dyn InputCaptureBackend> {
    #[cfg(windows)]
    {
        match WindowsHookCaptureBackend::new() {
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
        Box::new(NoopCaptureBackend)
    }
}

#[cfg(not(windows))]
struct NoopInputBackend;

#[cfg(not(windows))]
struct NoopCaptureBackend;

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
#[derive(Debug, Clone)]
enum HookCaptureEvent {
    MouseDelta { dx: i32, dy: i32 },
    MousePosition { x: i32, y: i32 },
    Input(InputEvent),
    Control(CaptureControlAction),
}

#[cfg(windows)]
struct WindowsHookCaptureBackend {
    event_rx: mpsc::Receiver<HookCaptureEvent>,
    hook_thread_id: u32,
    hook_thread: Option<JoinHandle<()>>,
    raw_input_thread_id: Option<u32>,
    raw_input_thread: Option<JoinHandle<()>>,
    raw_input_enabled: bool,
    lock_active: bool,
    control_actions: VecDeque<CaptureControlAction>,
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, bool>,
    last_button_down: HashMap<u16, bool>,
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

    struct CountingBackend {
        applied: usize,
    }

    impl InputBackend for CountingBackend {
        fn apply(&mut self, _event: &InputEvent) -> Result<()> {
            self.applied += 1;
            Ok(())
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

        let events = vec![InputEvent::MouseMove { dx: 1, dy: 1 }; MAX_EVENTS_PER_FRAME + 1];
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
            .set_layout("left,self,right".to_string())
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
    fn send_input_records_with_sender_sends_one_record_per_call() {
        let records = input_records_for_event(&InputEvent::MouseWheel {
            delta_x: 120,
            delta_y: -120,
        });
        let mut call_count = 0usize;

        send_input_records_with_sender(&records, |chunk| {
            call_count += 1;
            assert_eq!(chunk.len(), 1);
            Ok(1)
        })
        .expect("send should succeed");

        assert_eq!(call_count, 2);
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

    #[cfg(windows)]
    #[test]
    fn hook_backend_flushes_mouse_move_before_button_event() {
        let (tx, rx) = mpsc::channel();
        let mut backend = WindowsHookCaptureBackend {
            event_rx: rx,
            hook_thread_id: 0,
            hook_thread: None,
            raw_input_thread_id: None,
            raw_input_thread: None,
            raw_input_enabled: true,
            lock_active: false,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
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
            event_rx: rx,
            hook_thread_id: 0,
            hook_thread: None,
            raw_input_thread_id: None,
            raw_input_thread: None,
            raw_input_enabled: true,
            lock_active: false,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
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
            event_rx: rx,
            hook_thread_id: 0,
            hook_thread: None,
            raw_input_thread_id: None,
            raw_input_thread: None,
            raw_input_enabled: false,
            lock_active: false,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
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
