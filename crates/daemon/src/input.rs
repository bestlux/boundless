use std::{collections::VecDeque, time::Duration};

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
use tracing::{info, warn};

use chrono::Utc;

use core_input::{EasyMouseMode, InputEvent, MAX_EVENTS_PER_FRAME, SwitchDirection};

use crate::state::{AppState, CaptureHandoffTarget, PendingInjectInputFrame, TransportEventRecord};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, POINT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW,
            SendInput,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HC_ACTION, HHOOK,
            KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, SetWindowsHookExW,
            TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
            WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
            WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
            WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
        },
    },
};

const INPUT_TICK: Duration = Duration::from_millis(5);
const INPUT_CAPTURE_TICK: Duration = Duration::from_millis(8);
const EDGE_PRESSURE_THRESHOLD: i32 = 300;
const ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS: u64 = 600;
#[cfg(windows)]
const ESCAPE_DOUBLE_CTRL_WINDOW_MS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureControlAction {
    EscapeUnlock,
}

#[derive(Debug, Default)]
struct EdgeSwitchState {
    last_direction: Option<SwitchDirection>,
    x_pressure: i32,
    y_pressure: i32,
    suppress_until_unix_ms: Option<u64>,
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

    if !events.is_empty() {
        for chunk in events.chunks(MAX_EVENTS_PER_FRAME) {
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

fn edge_switch_direction_from_motion(
    events: &[InputEvent],
    state: &mut EdgeSwitchState,
    wrap_mouse: bool,
) -> Option<SwitchDirection> {
    let now_ms = unix_now_ms();
    if state
        .suppress_until_unix_ms
        .is_some_and(|until| now_ms < until)
    {
        state.last_direction = None;
        state.x_pressure = 0;
        state.y_pressure = 0;
        return None;
    }
    state.suppress_until_unix_ms = None;

    for event in events {
        let InputEvent::MouseMove { dx, dy } = event else {
            continue;
        };

        if *dx != 0 {
            if state.x_pressure.signum() != dx.signum() {
                state.x_pressure = 0;
            }
            state.x_pressure = state
                .x_pressure
                .saturating_add(*dx)
                .clamp(-EDGE_PRESSURE_THRESHOLD * 2, EDGE_PRESSURE_THRESHOLD * 2);
        }

        if *dy != 0 {
            if state.y_pressure.signum() != dy.signum() {
                state.y_pressure = 0;
            }
            state.y_pressure = state
                .y_pressure
                .saturating_add(*dy)
                .clamp(-EDGE_PRESSURE_THRESHOLD * 2, EDGE_PRESSURE_THRESHOLD * 2);
        }
    }

    let x_abs = state.x_pressure.abs();
    let y_abs = state.y_pressure.abs();

    if x_abs >= EDGE_PRESSURE_THRESHOLD && x_abs >= y_abs {
        return Some(if state.x_pressure < 0 {
            SwitchDirection::Left
        } else {
            SwitchDirection::Right
        });
    }
    if wrap_mouse && y_abs >= EDGE_PRESSURE_THRESHOLD && y_abs >= x_abs {
        return Some(if state.y_pressure < 0 {
            SwitchDirection::Up
        } else {
            SwitchDirection::Down
        });
    }

    None
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn maybe_handoff_capture_target_from_motion(
    state: &AppState,
    events: &[InputEvent],
    current_target: Option<&str>,
    edge_switch_state: &mut EdgeSwitchState,
) {
    let (mode, wrap_mouse) = state.edge_switch_policy().await;
    if !matches!(mode, EasyMouseMode::Enable) {
        edge_switch_state.last_direction = None;
        edge_switch_state.x_pressure = 0;
        edge_switch_state.y_pressure = 0;
        return;
    }

    let direction = edge_switch_direction_from_motion(events, edge_switch_state, wrap_mouse);
    let Some(direction) = direction else {
        edge_switch_state.last_direction = None;
        return;
    };

    if edge_switch_state.last_direction == Some(direction) {
        return;
    }
    edge_switch_state.last_direction = Some(direction);

    let Some(next_target) = state
        .capture_handoff_target_for_direction(current_target, direction)
        .await
    else {
        return;
    };

    match next_target {
        CaptureHandoffTarget::Peer(next_peer_id) => {
            if current_target == Some(next_peer_id.as_str()) {
                return;
            }
            match state.set_input_capture_target(Some(&next_peer_id)).await {
                Ok(Some(peer_id)) => {
                    let previous_target = current_target.unwrap_or("local");
                    info!(
                        direction = ?direction,
                        previous_target = %previous_target,
                        next_target = %peer_id,
                        "edge switch capture handoff applied"
                    );
                    record_local_input_runtime_event(
                        state,
                        "input_handoff",
                        &format!("direction={direction:?} from={previous_target} to={peer_id}"),
                        &peer_id,
                    )
                    .await;
                    edge_switch_state.x_pressure = 0;
                    edge_switch_state.y_pressure = 0;
                }
                Ok(None) => {}
                Err(error) => {
                    let previous_target = current_target.unwrap_or("local");
                    warn!(
                        direction = ?direction,
                        previous_target = %previous_target,
                        next_target = %next_peer_id,
                        error = ?error,
                        "failed to apply edge switch capture handoff"
                    );
                }
            }
        }
        CaptureHandoffTarget::Local => {
            if current_target.is_none() {
                return;
            }
            let previous_target = current_target.unwrap_or("local");
            state.clear_input_capture_target().await;
            info!(
                direction = ?direction,
                previous_target = %previous_target,
                "edge switch capture handoff returned to local"
            );
            record_local_input_runtime_event(
                state,
                "input_handoff",
                &format!("direction={direction:?} from={previous_target} to=local"),
                "none",
            )
            .await;
            edge_switch_state.x_pressure = 0;
            edge_switch_state.y_pressure = 0;
        }
    }
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
}

#[cfg(windows)]
#[derive(Debug, Clone)]
enum HookCaptureEvent {
    MousePosition { x: i32, y: i32 },
    Input(InputEvent),
    Control(CaptureControlAction),
}

#[cfg(windows)]
struct WindowsHookCaptureBackend {
    event_rx: mpsc::Receiver<HookCaptureEvent>,
    hook_thread_id: u32,
    hook_thread: Option<JoinHandle<()>>,
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

        Ok(Self {
            event_rx,
            hook_thread_id,
            hook_thread: Some(hook_thread),
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
                if prior != Some(is_down) {
                    output.push(InputEvent::Key { scan_code, state });
                }
            }
            InputEvent::MouseMove { .. } | InputEvent::MouseWheel { .. } => output.push(event),
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsHookCaptureBackend {
    fn drop(&mut self) {
        if self.lock_active {
            let _ = set_hook_lock_active(false);
            self.lock_active = false;
        }
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
        let mut output = Vec::new();

        for event in self.drain_pending_events() {
            match event {
                HookCaptureEvent::MousePosition { x, y } => {
                    if let Some((last_x, last_y)) = self.last_cursor {
                        let dx = x - last_x;
                        let dy = y - last_y;
                        if dx != 0 || dy != 0 {
                            output.push(InputEvent::MouseMove { dx, dy });
                        }
                    }
                    self.last_cursor = Some((x, y));
                }
                HookCaptureEvent::Input(input_event) => {
                    self.update_pressed_state_and_filter(input_event, &mut output);
                }
                HookCaptureEvent::Control(action) => {
                    self.control_actions.push_back(action);
                }
            }
        }

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

#[cfg(windows)]
fn mouse_button_virtual_keys() -> [(u16, core_input::MouseButton); 5] {
    [
        (VK_LBUTTON_CODE, core_input::MouseButton::Left),
        (VK_RBUTTON_CODE, core_input::MouseButton::Right),
        (VK_MBUTTON_CODE, core_input::MouseButton::Middle),
        (VK_XBUTTON1_CODE, core_input::MouseButton::X1),
        (VK_XBUTTON2_CODE, core_input::MouseButton::X2),
    ]
}

#[cfg(windows)]
fn mouse_button_from_virtual_key(vk: u16) -> Option<core_input::MouseButton> {
    match vk {
        VK_LBUTTON_CODE => Some(core_input::MouseButton::Left),
        VK_RBUTTON_CODE => Some(core_input::MouseButton::Right),
        VK_MBUTTON_CODE => Some(core_input::MouseButton::Middle),
        VK_XBUTTON1_CODE => Some(core_input::MouseButton::X1),
        VK_XBUTTON2_CODE => Some(core_input::MouseButton::X2),
        _ => None,
    }
}

#[cfg(windows)]
fn virtual_key_for_mouse_button(button: core_input::MouseButton) -> u16 {
    match button {
        core_input::MouseButton::Left => VK_LBUTTON_CODE,
        core_input::MouseButton::Right => VK_RBUTTON_CODE,
        core_input::MouseButton::Middle => VK_MBUTTON_CODE,
        core_input::MouseButton::X1 => VK_XBUTTON1_CODE,
        core_input::MouseButton::X2 => VK_XBUTTON2_CODE,
    }
}

#[cfg(windows)]
fn captured_key_virtual_keys() -> &'static [u16] {
    CAPTURE_KEY_VIRTUAL_KEYS
}

#[cfg(windows)]
struct HookSenderGuard;

#[cfg(windows)]
impl Drop for HookSenderGuard {
    fn drop(&mut self) {
        let _ = set_hook_event_sender(None);
    }
}

#[cfg(windows)]
fn hook_sender_cell() -> &'static Mutex<Option<mpsc::Sender<HookCaptureEvent>>> {
    HOOK_EVENT_SENDER.get_or_init(|| Mutex::new(None))
}

#[cfg(windows)]
fn set_hook_event_sender(sender: Option<mpsc::Sender<HookCaptureEvent>>) -> Result<()> {
    let mut guard = hook_sender_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook sender mutex poisoned"))?;
    *guard = sender;
    Ok(())
}

#[cfg(windows)]
fn send_hook_event(event: HookCaptureEvent) {
    let sender = hook_sender_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(sender) = sender {
        let _ = sender.send(event);
    }
}

#[cfg(windows)]
fn hook_runtime_state_cell() -> &'static Mutex<HookRuntimeState> {
    HOOK_RUNTIME_STATE.get_or_init(|| Mutex::new(HookRuntimeState::default()))
}

#[cfg(windows)]
fn set_hook_lock_active(active: bool) -> Result<()> {
    let mut state = hook_runtime_state_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook runtime state mutex poisoned"))?;
    state.lock_active = active;
    if !active {
        state.left_ctrl_down = false;
        state.right_ctrl_down = false;
        state.last_ctrl_tap_unix_ms = None;
    }
    Ok(())
}

#[cfg(windows)]
fn is_hook_lock_active() -> bool {
    hook_runtime_state_cell()
        .lock()
        .map(|state| state.lock_active)
        .unwrap_or(false)
}

#[cfg(windows)]
fn update_escape_state_for_key(vk_code: u16, key_state: core_input::KeyState) -> bool {
    if vk_code != VK_CONTROL_CODE && vk_code != VK_LCONTROL_CODE && vk_code != VK_RCONTROL_CODE {
        return false;
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let mut state = match hook_runtime_state_cell().lock() {
        Ok(state) => state,
        Err(_) => return false,
    };
    if !state.lock_active {
        return false;
    }

    let is_down = matches!(key_state, core_input::KeyState::Down);
    let was_down = match vk_code {
        VK_LCONTROL_CODE => state.left_ctrl_down,
        VK_RCONTROL_CODE => state.right_ctrl_down,
        _ => state.left_ctrl_down || state.right_ctrl_down,
    };

    if is_down {
        match vk_code {
            VK_LCONTROL_CODE => state.left_ctrl_down = true,
            VK_RCONTROL_CODE => state.right_ctrl_down = true,
            _ => {
                state.left_ctrl_down = true;
                state.right_ctrl_down = true;
            }
        }

        if !was_down {
            let triggered = state.last_ctrl_tap_unix_ms.is_some_and(|previous| {
                now_ms.saturating_sub(previous) <= ESCAPE_DOUBLE_CTRL_WINDOW_MS
            });
            state.last_ctrl_tap_unix_ms = Some(now_ms);
            return triggered;
        }
    } else {
        match vk_code {
            VK_LCONTROL_CODE => state.left_ctrl_down = false,
            VK_RCONTROL_CODE => state.right_ctrl_down = false,
            _ => {
                state.left_ctrl_down = false;
                state.right_ctrl_down = false;
            }
        }
    }

    false
}

#[cfg(windows)]
unsafe fn install_keyboard_hook() -> Result<HHOOK> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW keyboard");
    }
    Ok(hook)
}

#[cfg(windows)]
unsafe fn install_mouse_hook() -> Result<HHOOK> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW mouse");
    }
    Ok(hook)
}

#[cfg(windows)]
unsafe fn run_hook_message_loop() {
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg as *mut MSG, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg as *const MSG);
            DispatchMessageW(&msg as *const MSG);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut lock_active = false;
    if code == HC_ACTION as i32 {
        let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if (keyboard.flags & LLKHF_INJECTED_MASK) == 0 {
            lock_active = is_hook_lock_active();
            let state = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(core_input::KeyState::Down),
                WM_KEYUP | WM_SYSKEYUP => Some(core_input::KeyState::Up),
                _ => None,
            };

            if let Some(state) = state {
                let mut scan_code = keyboard.scanCode as u16;
                if (keyboard.flags & LLKHF_EXTENDED_MASK) != 0 {
                    scan_code |= 0xE000;
                }
                send_hook_event(HookCaptureEvent::Input(InputEvent::Key {
                    scan_code,
                    state,
                }));

                if lock_active && update_escape_state_for_key(keyboard.vkCode as u16, state) {
                    send_hook_event(HookCaptureEvent::Control(
                        CaptureControlAction::EscapeUnlock,
                    ));
                }
            }
        }
    }

    if lock_active {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(windows)]
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut lock_active = false;
    if code == HC_ACTION as i32 {
        let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if (mouse.flags & LLMHF_INJECTED_MASK) == 0 {
            lock_active = is_hook_lock_active();
            match wparam as u32 {
                WM_MOUSEMOVE => {
                    send_hook_event(HookCaptureEvent::MousePosition {
                        x: mouse.pt.x,
                        y: mouse.pt.y,
                    });
                }
                WM_LBUTTONDOWN => {
                    send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: core_input::MouseButton::Left,
                        state: core_input::KeyState::Down,
                    }))
                }
                WM_LBUTTONUP => send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                    button: core_input::MouseButton::Left,
                    state: core_input::KeyState::Up,
                })),
                WM_RBUTTONDOWN => {
                    send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: core_input::MouseButton::Right,
                        state: core_input::KeyState::Down,
                    }))
                }
                WM_RBUTTONUP => send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                    button: core_input::MouseButton::Right,
                    state: core_input::KeyState::Up,
                })),
                WM_MBUTTONDOWN => {
                    send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: core_input::MouseButton::Middle,
                        state: core_input::KeyState::Down,
                    }))
                }
                WM_MBUTTONUP => send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                    button: core_input::MouseButton::Middle,
                    state: core_input::KeyState::Up,
                })),
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let button = match high_word(mouse.mouseData) {
                        XBUTTON1_DATA => Some(core_input::MouseButton::X1),
                        XBUTTON2_DATA => Some(core_input::MouseButton::X2),
                        _ => None,
                    };
                    if let Some(button) = button {
                        send_hook_event(HookCaptureEvent::Input(InputEvent::MouseButton {
                            button,
                            state: if (wparam as u32) == WM_XBUTTONDOWN {
                                core_input::KeyState::Down
                            } else {
                                core_input::KeyState::Up
                            },
                        }));
                    }
                }
                WM_MOUSEWHEEL => send_hook_event(HookCaptureEvent::Input(InputEvent::MouseWheel {
                    delta_x: 0,
                    delta_y: signed_high_word(mouse.mouseData),
                })),
                WM_MOUSEHWHEEL => {
                    send_hook_event(HookCaptureEvent::Input(InputEvent::MouseWheel {
                        delta_x: signed_high_word(mouse.mouseData),
                        delta_y: 0,
                    }))
                }
                _ => {}
            }
        }
    }

    if lock_active {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(windows)]
fn high_word(value: u32) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

#[cfg(windows)]
fn signed_high_word(value: u32) -> i32 {
    i16::from_ne_bytes(high_word(value).to_ne_bytes()) as i32
}

#[cfg(windows)]
fn cursor_position() -> Result<Option<(i32, i32)>> {
    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point as *mut POINT) };
    if ok == 0 {
        return Ok(None);
    }
    Ok(Some((point.x, point.y)))
}

#[cfg(windows)]
fn is_virtual_key_down(vk: u16) -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    (state as u16 & 0x8000) != 0
}

#[cfg(windows)]
fn vk_to_scan_code(vk: u16) -> Option<u16> {
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) } as u16;
    if scan == 0 { None } else { Some(scan) }
}

#[cfg(windows)]
fn input_records_for_event(event: &InputEvent) -> Vec<INPUT> {
    match event {
        InputEvent::MouseMove { dx, dy } => {
            if *dx == 0 && *dy == 0 {
                Vec::new()
            } else {
                vec![mouse_input(*dx, *dy, 0, MOUSEEVENTF_MOVE)]
            }
        }
        InputEvent::MouseButton { button, state } => {
            let (flags, mouse_data) = match (button, state) {
                (core_input::MouseButton::Left, core_input::KeyState::Down) => {
                    (MOUSEEVENTF_LEFTDOWN, 0)
                }
                (core_input::MouseButton::Left, core_input::KeyState::Up) => {
                    (MOUSEEVENTF_LEFTUP, 0)
                }
                (core_input::MouseButton::Right, core_input::KeyState::Down) => {
                    (MOUSEEVENTF_RIGHTDOWN, 0)
                }
                (core_input::MouseButton::Right, core_input::KeyState::Up) => {
                    (MOUSEEVENTF_RIGHTUP, 0)
                }
                (core_input::MouseButton::Middle, core_input::KeyState::Down) => {
                    (MOUSEEVENTF_MIDDLEDOWN, 0)
                }
                (core_input::MouseButton::Middle, core_input::KeyState::Up) => {
                    (MOUSEEVENTF_MIDDLEUP, 0)
                }
                (core_input::MouseButton::X1, core_input::KeyState::Down) => {
                    (MOUSEEVENTF_XDOWN, XBUTTON1 as u32)
                }
                (core_input::MouseButton::X1, core_input::KeyState::Up) => {
                    (MOUSEEVENTF_XUP, XBUTTON1 as u32)
                }
                (core_input::MouseButton::X2, core_input::KeyState::Down) => {
                    (MOUSEEVENTF_XDOWN, XBUTTON2 as u32)
                }
                (core_input::MouseButton::X2, core_input::KeyState::Up) => {
                    (MOUSEEVENTF_XUP, XBUTTON2 as u32)
                }
            };

            vec![mouse_input(0, 0, mouse_data, flags)]
        }
        InputEvent::MouseWheel { delta_x, delta_y } => {
            let mut records = Vec::with_capacity(2);
            if *delta_y != 0 {
                records.push(mouse_input(0, 0, *delta_y as u32, MOUSEEVENTF_WHEEL));
            }
            if *delta_x != 0 {
                records.push(mouse_input(0, 0, *delta_x as u32, MOUSEEVENTF_HWHEEL));
            }
            records
        }
        InputEvent::Key { scan_code, state } => {
            vec![keyboard_input(
                *scan_code,
                matches!(state, core_input::KeyState::Up),
            )]
        }
    }
}

#[cfg(windows)]
fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn keyboard_input(scan_code: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    let mut normalized_scan_code = scan_code;
    if is_extended_scan_code(scan_code) {
        flags |= KEYEVENTF_EXTENDEDKEY;
        normalized_scan_code = scan_code & 0x00FF;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: normalized_scan_code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
fn send_input_records(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    send_input_records_with_sender(inputs, |record| {
        let sent = unsafe { SendInput(1, record.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(std::io::Error::last_os_error()).context("SendInput returned 0");
        }
        Ok(sent)
    })
}

#[cfg(windows)]
fn send_input_records_with_sender<F>(inputs: &[INPUT], mut sender: F) -> Result<()>
where
    F: FnMut(&[INPUT]) -> Result<u32>,
{
    for (index, input) in inputs.iter().enumerate() {
        let sent = sender(std::slice::from_ref(input))
            .with_context(|| format!("send input record at index {index}"))?;
        if sent != 1 {
            bail!("partial send at index {index}: sent {sent} / 1 input records");
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_extended_scan_code(scan_code: u16) -> bool {
    matches!(scan_code & 0xFF00, 0xE000 | 0xE100)
}

#[cfg(windows)]
fn input_event_kind(event: &InputEvent) -> &'static str {
    match event {
        InputEvent::MouseMove { .. } => "mouse_move",
        InputEvent::MouseButton { .. } => "mouse_button",
        InputEvent::MouseWheel { .. } => "mouse_wheel",
        InputEvent::Key { .. } => "key",
    }
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
        assert!(
            right_outgoing.is_empty(),
            "trigger frame should remain on previous target; new target gets subsequent frames"
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
}
