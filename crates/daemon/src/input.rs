use std::time::Duration;

#[cfg(windows)]
use std::collections::VecDeque;

#[cfg(windows)]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock, mpsc},
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

mod edge_switch;
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

async fn run(state: AppState) -> Result<()> {
    let mut inject_backend = input_backend();
    let mut capture_backend = input_capture_backend();
    state
        .set_input_lock_runtime(false, capture_backend.lock_supported())
        .await;
    let mut capture_backend_mode = capture_backend.backend_mode();
    record_local_input_runtime_event(
        &state,
        "input_capture_backend_mode",
        capture_backend_mode,
        "none",
    )
    .await;
    let mut inject_ticker = time::interval(INPUT_TICK);
    let mut capture_ticker = time::interval(INPUT_CAPTURE_TICK);
    let mut last_capture_target: Option<String> = None;
    let mut edge_switch_state = EdgeSwitchState::default();

    loop {
        tokio::select! {
            _ = inject_ticker.tick() => {
                drain_pending_inject_frames(&state, inject_backend.as_mut()).await;
            }
            _ = capture_ticker.tick() => {
                capture_and_queue_outgoing_frames(
                    &state,
                    capture_backend.as_mut(),
                    &mut last_capture_target,
                    &mut edge_switch_state,
                )
                .await;
                let next_mode = capture_backend.backend_mode();
                if next_mode != capture_backend_mode {
                    capture_backend_mode = next_mode;
                    record_local_input_runtime_event(
                        &state,
                        "input_capture_backend_mode",
                        capture_backend_mode,
                        "none",
                    )
                    .await;
                }
            }
        }
    }
}

async fn drain_pending_inject_frames(state: &AppState, backend: &mut dyn InputBackend) {
    while let Some(frame) = state.dequeue_pending_inject_input_frame().await {
        if !state.input_injection_allowed_for_peer(&frame.peer_id).await {
            state
                .record_input_inject_skipped(
                    &frame.peer_id,
                    frame.sequence,
                    frame.events.len(),
                    frame.timing(),
                    "owner_or_feature_changed",
                )
                .await;
            continue;
        }

        match apply_frame(backend, &frame) {
            Ok(()) => {
                state
                    .record_input_inject_applied(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                    )
                    .await;
            }
            Err(error) => {
                let message = format!("{error:#}");
                state
                    .record_input_inject_failed(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                        &message,
                    )
                    .await;
                state.requeue_pending_inject_input_frame_front(frame).await;
                break;
            }
        }
    }
}

fn apply_frame(backend: &mut dyn InputBackend, frame: &PendingInjectInputFrame) -> Result<()> {
    for event in &frame.events {
        backend.apply(event)?;
    }
    Ok(())
}

async fn capture_and_queue_outgoing_frames(
    state: &AppState,
    backend: &mut dyn InputCaptureBackend,
    last_capture_target: &mut Option<String>,
    edge_switch_state: &mut EdgeSwitchState,
) {
    let mut capture_target = state.active_input_capture_target().await;
    sync_local_input_lock(state, backend, capture_target.is_some()).await;

    if &capture_target != last_capture_target {
        if let Some(previous_target) = last_capture_target.as_deref() {
            let release_events = backend.drain_release_events();
            if !release_events.is_empty() {
                for chunk in release_events.chunks(MAX_EVENTS_PER_FRAME) {
                    if let Err(error) = state
                        .queue_input_events(previous_target, chunk.to_vec())
                        .await
                    {
                        warn!(
                            peer_id = %previous_target,
                            error = ?error,
                            "failed to queue synthetic release events for previous capture target"
                        );
                        break;
                    }
                }
            }
        }

        backend.reset();
        *last_capture_target = capture_target.clone();
    }

    let events = match backend.poll_events() {
        Ok(events) => events,
        Err(error) => {
            warn!(error = ?error, "input capture poll failed");
            Vec::new()
        }
    };
    let cursor_position = backend.cursor_position();
    let screen_bounds = local_virtual_screen_bounds();

    let mut escape_triggered = false;
    for action in backend.drain_control_actions() {
        if !matches!(action, CaptureControlAction::EscapeUnlock) {
            continue;
        }

        if capture_target.is_some() {
            state.clear_input_capture_target().await;
            record_local_input_runtime_event(
                state,
                "input_escape_triggered",
                "double_ctrl",
                "none",
            )
            .await;
            edge_switch_state.last_direction = None;
            edge_switch_state.x_pressure = 0;
            edge_switch_state.y_pressure = 0;
            edge_switch_state.suppress_until_unix_ms =
                Some(unix_now_ms().saturating_add(ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS));
            capture_target = None;
            sync_local_input_lock(state, backend, false).await;
            escape_triggered = true;
        }
    }

    let pre_handoff_target = capture_target;
    if let Some(peer_id) = pre_handoff_target.as_deref()
        && !events.is_empty()
    {
        for chunk in events.chunks(MAX_EVENTS_PER_FRAME) {
            if let Err(error) = state.queue_input_events(peer_id, chunk.to_vec()).await {
                warn!(
                    peer_id = %peer_id,
                    error = ?error,
                    "failed to queue captured local input frame"
                );
                break;
            }
        }
    }

    if !escape_triggered {
        maybe_handoff_capture_target_from_motion(
            state,
            &events,
            pre_handoff_target.as_deref(),
            edge_switch_state,
            cursor_position,
            screen_bounds,
        )
        .await;
    }

    let post_handoff_target = state.active_input_capture_target().await;
    sync_local_input_lock(state, backend, post_handoff_target.is_some()).await;

    let (Some(peer_id), None) = (
        post_handoff_target.as_deref(),
        pre_handoff_target.as_deref(),
    ) else {
        return;
    };

    let replay_events = if edge_switch_state.suppress_until_unix_ms.is_some() {
        filter_edge_start_replay_events(&events)
    } else {
        events
    };

    if !replay_events.is_empty() {
        for chunk in replay_events.chunks(MAX_EVENTS_PER_FRAME) {
            if let Err(error) = state.queue_input_events(peer_id, chunk.to_vec()).await {
                warn!(
                    peer_id = %peer_id,
                    error = ?error,
                    "failed to queue captured local input frame after local edge-start handoff"
                );
                break;
            }
        }
    }
}

async fn sync_local_input_lock(
    state: &AppState,
    backend: &mut dyn InputCaptureBackend,
    should_lock: bool,
) {
    let supported = backend.lock_supported();
    let active = match backend.set_lock_active(should_lock) {
        Ok(active) => active,
        Err(error) => {
            warn!(error = ?error, should_lock, "failed to update local input lock state");
            false
        }
    };

    let (last_active, last_supported) = state.input_lock_runtime().await;
    if last_active != active {
        let kind = if active {
            "input_lock_engaged"
        } else {
            "input_lock_released"
        };
        let detail = if should_lock && !active {
            "requested=true applied=false".to_string()
        } else {
            format!("requested={should_lock} applied={active}")
        };
        record_local_input_runtime_event(state, kind, &detail, "none").await;
    }
    if last_active != active || last_supported != supported {
        state.set_input_lock_runtime(active, supported).await;
    }
}

async fn record_local_input_runtime_event(
    state: &AppState,
    kind: &str,
    detail: &str,
    peer_id: &str,
) {
    state
        .record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: kind.to_string(),
            peer_id: peer_id.to_string(),
            detail: detail.to_string(),
            size_bytes: 0,
        })
        .await;
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
impl InputBackend for NoopInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(windows))]
struct NoopCaptureBackend;

#[cfg(not(windows))]
impl InputCaptureBackend for NoopCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        Vec::new()
    }

    fn reset(&mut self) {}

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        Vec::new()
    }

    fn set_lock_active(&mut self, _active: bool) -> Result<bool> {
        Ok(false)
    }

    fn lock_supported(&self) -> bool {
        false
    }

    fn backend_mode(&self) -> &'static str {
        "noop"
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsInputBackend;

#[cfg(windows)]
impl InputBackend for WindowsInputBackend {
    fn apply(&mut self, event: &InputEvent) -> Result<()> {
        let records = input_records_for_event(event);
        send_input_records(&records)
            .with_context(|| format!("SendInput failed for {}", input_event_kind(event)))
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsPollingCaptureBackend {
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, bool>,
    last_button_down: HashMap<u16, bool>,
}

#[cfg(windows)]
impl InputCaptureBackend for WindowsPollingCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();

        let mut pressed_buttons = self
            .last_button_down
            .iter()
            .filter_map(|(vk, down)| if *down { Some(*vk) } else { None })
            .collect::<Vec<_>>();
        pressed_buttons.sort_unstable();
        for vk in pressed_buttons {
            if let Some(button) = mouse_button_from_virtual_key(vk) {
                events.push(InputEvent::MouseButton {
                    button,
                    state: core_input::KeyState::Up,
                });
            }
        }

        let mut pressed_keys = self
            .last_key_down
            .iter()
            .filter_map(|(vk, down)| if *down { Some(*vk) } else { None })
            .collect::<Vec<_>>();
        pressed_keys.sort_unstable();
        for vk in pressed_keys {
            if let Some(scan_code) = vk_to_scan_code(vk) {
                events.push(InputEvent::Key {
                    scan_code,
                    state: core_input::KeyState::Up,
                });
            }
        }

        events
    }

    fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        let mut events = Vec::new();

        if let Some((x, y)) = cursor_position()? {
            if let Some((last_x, last_y)) = self.last_cursor {
                let dx = x - last_x;
                let dy = y - last_y;
                if dx != 0 || dy != 0 {
                    events.push(InputEvent::MouseMove { dx, dy });
                }
            }
            self.last_cursor = Some((x, y));
        }

        for (vk, button) in mouse_button_virtual_keys() {
            let down = is_virtual_key_down(vk);
            if let Some(last) = self.last_button_down.insert(vk, down)
                && last != down
            {
                events.push(InputEvent::MouseButton {
                    button,
                    state: if down {
                        core_input::KeyState::Down
                    } else {
                        core_input::KeyState::Up
                    },
                });
            }
        }

        for &vk in captured_key_virtual_keys() {
            let down = is_virtual_key_down(vk);
            if let Some(last) = self.last_key_down.insert(vk, down)
                && last != down
                && let Some(scan_code) = vk_to_scan_code(vk)
            {
                events.push(InputEvent::Key {
                    scan_code,
                    state: if down {
                        core_input::KeyState::Down
                    } else {
                        core_input::KeyState::Up
                    },
                });
            }
        }

        Ok(events)
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        Vec::new()
    }

    fn set_lock_active(&mut self, _active: bool) -> Result<bool> {
        Ok(false)
    }

    fn lock_supported(&self) -> bool {
        false
    }

    fn backend_mode(&self) -> &'static str {
        "polling"
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }
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

#[cfg(windows)]
impl WindowsHookCaptureBackend {
    fn new() -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel::<HookCaptureEvent>();
        let (startup_tx, startup_rx) = mpsc::channel::<Result<u32>>();

        let hook_thread = thread::spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            if let Err(error) = set_hook_event_sender(Some(event_tx)) {
                let _ = startup_tx.send(Err(error));
                return;
            }

            let _guard = HookSenderGuard;
            let keyboard_hook = unsafe { install_keyboard_hook() };
            let mouse_hook = unsafe { install_mouse_hook() };
            match (keyboard_hook, mouse_hook) {
                (Ok(keyboard_hook), Ok(mouse_hook)) => {
                    let _ = startup_tx.send(Ok(thread_id));
                    unsafe { run_hook_message_loop() };
                    unsafe {
                        let _ = UnhookWindowsHookEx(keyboard_hook);
                        let _ = UnhookWindowsHookEx(mouse_hook);
                    }
                }
                (keyboard, mouse) => {
                    if let Ok(hook) = keyboard.as_ref() {
                        unsafe {
                            let _ = UnhookWindowsHookEx(*hook);
                        }
                    }
                    if let Ok(hook) = mouse.as_ref() {
                        unsafe {
                            let _ = UnhookWindowsHookEx(*hook);
                        }
                    }
                    let error = keyboard
                        .err()
                        .or_else(|| mouse.err())
                        .unwrap_or_else(|| anyhow::anyhow!("failed to install capture hooks"));
                    let _ = startup_tx.send(Err(error));
                }
            }
        });

        let hook_thread_id = startup_rx.recv().context("hook startup channel closed")??;
        let (raw_input_thread_id, raw_input_thread, raw_input_enabled) =
            match spawn_raw_input_thread() {
                Ok((thread_id, thread)) => (Some(thread_id), Some(thread), true),
                Err(error) => {
                    warn!(
                        error = ?error,
                        "raw input mouse capture unavailable; falling back to mouse hook position deltas"
                    );
                    (None, None, false)
                }
            };

        Ok(Self {
            event_rx,
            hook_thread_id,
            hook_thread: Some(hook_thread),
            raw_input_thread_id,
            raw_input_thread,
            raw_input_enabled,
            lock_active: false,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
        })
    }

    fn drain_pending_events(&mut self) -> Vec<HookCaptureEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn update_pressed_state_and_filter(&mut self, event: InputEvent, output: &mut Vec<InputEvent>) {
        match event {
            InputEvent::MouseButton { button, state } => {
                let vk = virtual_key_for_mouse_button(button);
                let is_down = matches!(state, core_input::KeyState::Down);
                let prior = self.last_button_down.insert(vk, is_down);
                if prior != Some(is_down) {
                    output.push(InputEvent::MouseButton { button, state });
                }
            }
            InputEvent::Key { scan_code, state } => {
                let is_down = matches!(state, core_input::KeyState::Down);
                let prior = self.last_key_down.insert(scan_code, is_down);
                if is_down || prior != Some(is_down) {
                    output.push(InputEvent::Key { scan_code, state });
                }
            }
            InputEvent::MouseMove { .. }
            | InputEvent::MouseMoveAbsolute { .. }
            | InputEvent::MouseWheel { .. } => output.push(event),
        }
    }

    fn accumulate_pending_move(pending_move: &mut Option<(i32, i32)>, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        match pending_move {
            Some((pending_dx, pending_dy)) => {
                *pending_dx = pending_dx.saturating_add(dx);
                *pending_dy = pending_dy.saturating_add(dy);
            }
            None => *pending_move = Some((dx, dy)),
        }
    }

    fn flush_pending_move(output: &mut Vec<InputEvent>, pending_move: &mut Option<(i32, i32)>) {
        let Some((dx, dy)) = pending_move.take() else {
            return;
        };
        if dx == 0 && dy == 0 {
            return;
        }
        output.push(InputEvent::MouseMove { dx, dy });
    }

    fn update_raw_input_runtime_state(&mut self) {
        if !self.raw_input_enabled {
            return;
        }

        let finished = self
            .raw_input_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished());
        if !finished {
            return;
        }

        if let Some(thread) = self.raw_input_thread.take() {
            let _ = thread.join();
        }
        self.raw_input_thread_id = None;
        self.raw_input_enabled = false;
        self.last_cursor = None;
        warn!("raw input capture thread exited; using mouse hook position delta fallback");
    }
}

#[cfg(windows)]
impl Drop for WindowsHookCaptureBackend {
    fn drop(&mut self) {
        if self.lock_active {
            let _ = set_hook_lock_active(false);
            self.lock_active = false;
        }
        if let Some(thread_id) = self.raw_input_thread_id {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(thread) = self.raw_input_thread.take() {
            let _ = thread.join();
        }
        self.raw_input_thread_id = None;
        self.raw_input_enabled = false;
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.hook_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
impl InputCaptureBackend for WindowsHookCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();

        let mut pressed_buttons = self
            .last_button_down
            .iter()
            .filter_map(|(vk, down)| if *down { Some(*vk) } else { None })
            .collect::<Vec<_>>();
        pressed_buttons.sort_unstable();
        for vk in pressed_buttons {
            if let Some(button) = mouse_button_from_virtual_key(vk) {
                events.push(InputEvent::MouseButton {
                    button,
                    state: core_input::KeyState::Up,
                });
            }
        }

        let mut pressed_keys = self
            .last_key_down
            .iter()
            .filter_map(|(scan_code, down)| if *down { Some(*scan_code) } else { None })
            .collect::<Vec<_>>();
        pressed_keys.sort_unstable();
        for scan_code in pressed_keys {
            events.push(InputEvent::Key {
                scan_code,
                state: core_input::KeyState::Up,
            });
        }

        events
    }

    fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
        self.control_actions.clear();
        let _ = self.drain_pending_events();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        self.update_raw_input_runtime_state();

        let mut output = Vec::new();
        let mut pending_move: Option<(i32, i32)> = None;

        for event in self.drain_pending_events() {
            match event {
                HookCaptureEvent::MouseDelta { dx, dy } => {
                    if self.raw_input_enabled {
                        Self::accumulate_pending_move(&mut pending_move, dx, dy);
                    }
                }
                HookCaptureEvent::MousePosition { x, y } => {
                    if let Some((last_x, last_y)) = self.last_cursor {
                        let dx = x - last_x;
                        let dy = y - last_y;
                        if !self.raw_input_enabled || !self.lock_active {
                            Self::accumulate_pending_move(&mut pending_move, dx, dy);
                        }
                    }
                    self.last_cursor = Some((x, y));
                }
                HookCaptureEvent::Input(input_event) => {
                    Self::flush_pending_move(&mut output, &mut pending_move);
                    self.update_pressed_state_and_filter(input_event, &mut output);
                }
                HookCaptureEvent::Control(action) => {
                    self.control_actions.push_back(action);
                }
            }
        }

        Self::flush_pending_move(&mut output, &mut pending_move);

        Ok(output)
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        self.control_actions.drain(..).collect()
    }

    fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        if self.lock_active != active {
            set_hook_lock_active(active)?;
            self.lock_active = active;
        }
        Ok(self.lock_active)
    }

    fn lock_supported(&self) -> bool {
        true
    }

    fn backend_mode(&self) -> &'static str {
        if self.raw_input_enabled {
            "hook_raw"
        } else {
            "hook"
        }
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }
}

#[cfg(windows)]
const VK_LBUTTON_CODE: u16 = 0x01;
#[cfg(windows)]
const VK_RBUTTON_CODE: u16 = 0x02;
#[cfg(windows)]
const VK_MBUTTON_CODE: u16 = 0x04;
#[cfg(windows)]
const VK_XBUTTON1_CODE: u16 = 0x05;
#[cfg(windows)]
const VK_XBUTTON2_CODE: u16 = 0x06;
#[cfg(windows)]
const VK_CONTROL_CODE: u16 = 0x11;
#[cfg(windows)]
const VK_LCONTROL_CODE: u16 = 0xA2;
#[cfg(windows)]
const VK_RCONTROL_CODE: u16 = 0xA3;
#[cfg(windows)]
const XBUTTON1_DATA: u16 = 0x0001;
#[cfg(windows)]
const XBUTTON2_DATA: u16 = 0x0002;
#[cfg(windows)]
const LLKHF_EXTENDED_MASK: u32 = 0x01;
#[cfg(windows)]
const LLKHF_INJECTED_MASK: u32 = 0x10;
#[cfg(windows)]
const LLMHF_INJECTED_MASK: u32 = 0x0000_0001;

#[cfg(windows)]
static HOOK_EVENT_SENDER: OnceLock<Mutex<Option<mpsc::Sender<HookCaptureEvent>>>> = OnceLock::new();
#[cfg(windows)]
static HOOK_RUNTIME_STATE: OnceLock<Mutex<HookRuntimeState>> = OnceLock::new();

#[cfg(windows)]
#[derive(Debug, Default)]
struct HookRuntimeState {
    lock_active: bool,
    left_ctrl_down: bool,
    right_ctrl_down: bool,
    last_ctrl_tap_unix_ms: Option<u64>,
}

#[cfg(windows)]
const CAPTURE_KEY_VIRTUAL_KEYS: &[u16] = &[
    0x08, // backspace
    0x09, // tab
    0x0D, // enter
    0x14, // caps lock
    0x1B, // escape
    0x20, // space
    0x21, // page up
    0x22, // page down
    0x23, // end
    0x24, // home
    0x25, // left
    0x26, // up
    0x27, // right
    0x28, // down
    0x2D, // insert
    0x2E, // delete
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, // 0-9
    0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50,
    0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, // A-Z
    0x5B, // left windows
    0x5C, // right windows
    0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, // numpad 0-9
    0x6A, // numpad *
    0x6B, // numpad +
    0x6D, // numpad -
    0x6E, // numpad .
    0x6F, // numpad /
    0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x7B, // F1-F12
    0x90, // num lock
    0x91, // scroll lock
    0xA0, // left shift
    0xA1, // right shift
    0xA2, // left control
    0xA3, // right control
    0xA4, // left alt
    0xA5, // right alt
    0xBA, // ;
    0xBB, // =
    0xBC, // ,
    0xBD, // -
    0xBE, // .
    0xBF, // /
    0xC0, // `
    0xDB, // [
    0xDC, // \
    0xDD, // ]
    0xDE, // '
];

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
