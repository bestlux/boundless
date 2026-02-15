use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time;
use tracing::warn;

use core_input::InputEvent;

use crate::state::{AppState, PendingInjectInputFrame};

#[cfg(windows)]
use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
        SendInput,
    },
    WindowsAndMessaging::{XBUTTON1, XBUTTON2},
};

const INPUT_TICK: Duration = Duration::from_millis(5);

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "input runtime stopped");
        }
    });
}

async fn run(state: AppState) -> Result<()> {
    let mut backend = input_backend();
    let mut ticker = time::interval(INPUT_TICK);

    loop {
        ticker.tick().await;
        drain_pending_inject_frames(&state, backend.as_mut()).await;
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

trait InputBackend: Send {
    fn apply(&mut self, event: &InputEvent) -> Result<()>;
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

#[cfg(not(windows))]
struct NoopInputBackend;

#[cfg(not(windows))]
impl InputBackend for NoopInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        Ok(())
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
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan_code,
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

    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };

    if sent != inputs.len() as u32 {
        let error = std::io::Error::last_os_error();
        return Err(error).context(format!("sent {sent} / {} input records", inputs.len()));
    }

    Ok(())
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
    #[cfg(not(windows))]
    use super::*;
    #[cfg(windows)]
    use super::*;
    #[cfg(not(windows))]
    use crate::state::AppState;
    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
    struct CountingBackend {
        applied: usize,
    }

    #[cfg(not(windows))]
    impl InputBackend for CountingBackend {
        fn apply(&mut self, _event: &InputEvent) -> Result<()> {
            self.applied += 1;
            Ok(())
        }
    }

    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
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
}
