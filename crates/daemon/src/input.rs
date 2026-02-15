use std::time::Duration;

#[cfg(windows)]
use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio::time;
use tracing::warn;

use core_input::{InputEvent, MAX_EVENTS_PER_FRAME};

use crate::state::{AppState, PendingInjectInputFrame};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW,
            SendInput,
        },
        WindowsAndMessaging::{GetCursorPos, XBUTTON1, XBUTTON2},
    },
};

const INPUT_TICK: Duration = Duration::from_millis(5);
const INPUT_CAPTURE_TICK: Duration = Duration::from_millis(8);

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
    let mut inject_ticker = time::interval(INPUT_TICK);
    let mut capture_ticker = time::interval(INPUT_CAPTURE_TICK);
    let mut last_capture_target: Option<String> = None;

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
                    "owner_or_feature_changed",
                )
                .await;
            continue;
        }

        match apply_frame(backend, &frame) {
            Ok(()) => {
                state
                    .record_input_inject_applied(&frame.peer_id, frame.sequence, frame.events.len())
                    .await;
            }
            Err(error) => {
                let message = format!("{error:#}");
                state
                    .record_input_inject_failed(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
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
) {
    let capture_target = state.active_input_capture_target().await;
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

    let Some(peer_id) = capture_target else {
        return;
    };

    match backend.poll_events() {
        Ok(events) => {
            if events.is_empty() {
                return;
            }

            for chunk in events.chunks(MAX_EVENTS_PER_FRAME) {
                if let Err(error) = state.queue_input_events(&peer_id, chunk.to_vec()).await {
                    warn!(
                        peer_id = %peer_id,
                        error = ?error,
                        "failed to queue captured local input frame"
                    );
                    break;
                }
            }
        }
        Err(error) => {
            warn!(error = ?error, "input capture poll failed");
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
        Box::new(WindowsCaptureBackend::default())
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
struct WindowsCaptureBackend {
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, bool>,
    last_button_down: HashMap<u16, bool>,
}

#[cfg(windows)]
impl InputCaptureBackend for WindowsCaptureBackend {
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
fn captured_key_virtual_keys() -> &'static [u16] {
    CAPTURE_KEY_VIRTUAL_KEYS
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
        reset_count: usize,
    }

    impl ScriptedCaptureBackend {
        fn new(batches: Vec<Vec<InputEvent>>, release_events: Vec<InputEvent>) -> Self {
            Self {
                batches: VecDeque::from(batches),
                release_events,
                reset_count: 0,
            }
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
            Ok(self.batches.pop_front().unwrap_or_default())
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

        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;
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
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;
        let reset_after_set = backend.reset_count;
        assert!(
            reset_after_set >= 1,
            "initial target activation should reset capture backend"
        );

        state.clear_input_capture_target().await;
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;
        assert!(
            backend.reset_count > reset_after_set,
            "clearing target should reset capture backend"
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
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;

        state
            .set_input_capture_target(Some(&peer_two))
            .await
            .expect("switch target");
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;

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
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;

        state.clear_input_capture_target().await;
        capture_and_queue_outgoing_frames(&state, &mut backend, &mut last_target).await;

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
