use std::sync::{Arc, Mutex, MutexGuard};

use core_input::InputEvent;

#[cfg(windows)]
use anyhow::{Context, Result, bail};
#[cfg(windows)]
use core_input::{KeySemantics, KeyState, MouseButton};

#[cfg(windows)]
mod hook_capture;
#[cfg(windows)]
mod hook_pump;

#[cfg(windows)]
pub use hook_capture::{
    CaptureRuntime, HookCaptureEvent, HookControlAction, captured_key_virtual_keys,
    mouse_button_from_virtual_key, mouse_button_virtual_keys, raw_mouse_relative_delta,
    raw_mouse_wheel_events, release_active_hook_lock, virtual_key_for_mouse_button,
};
#[cfg(windows)]
pub use hook_pump::{HookInputPump, WheelSourceCounts};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, POINT},
    Security::{GetTokenInformation, TOKEN_QUERY, TokenSessionId},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
            MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
            MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

#[cfg(windows)]
const XBUTTON1: u32 = 0x0001;

#[cfg(windows)]
const XBUTTON2: u32 = 0x0002;

#[cfg(windows)]
pub const VK_NUMLOCK_CODE: u16 = 0x90;

/// Process-local Num Lock authority shared by the interactive capture and
/// injection lanes. The hook message thread seeds and updates physical state;
/// successful Boundless injection commits synthetic toggle changes.
#[derive(Debug, Clone)]
pub struct WindowsNumLockState {
    on: Arc<Mutex<bool>>,
}

impl WindowsNumLockState {
    pub fn new(on: bool) -> Self {
        Self {
            on: Arc::new(Mutex::new(on)),
        }
    }

    pub fn is_on(&self) -> bool {
        *self.lock()
    }

    pub fn set(&self, on: bool) {
        *self.lock() = on;
    }

    pub fn toggle(&self) -> bool {
        let mut on = self.lock();
        *on = !*on;
        *on
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.on
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct WindowsInputState {
    num_lock: WindowsNumLockState,
}

#[cfg(windows)]
impl WindowsInputState {
    pub fn new(num_lock: WindowsNumLockState) -> Self {
        Self { num_lock }
    }

    pub fn send_events(&self, events: &[InputEvent]) -> Result<()> {
        self.send_events_with(events, send_input_records)
    }

    fn send_events_with<F>(&self, events: &[InputEvent], sender: F) -> Result<()>
    where
        F: FnOnce(&[INPUT]) -> Result<()>,
    {
        // Serialize physical toggle observations with prepare/send/commit so
        // a concurrent local Num Lock press cannot be overwritten by a stale
        // post-SendInput state commit.
        let mut num_lock_on = self.num_lock.lock();
        let (records, resulting_num_lock_on) = prepare_input_records(events, *num_lock_on);
        sender(&records)?;
        *num_lock_on = resulting_num_lock_on;
        Ok(())
    }
}

pub fn high_word(value: u32) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

#[cfg(windows)]
pub fn signed_high_word(value: u32) -> i32 {
    i16::from_ne_bytes(high_word(value).to_ne_bytes()) as i32
}

#[cfg(windows)]
pub fn cursor_position() -> Result<Option<(i32, i32)>> {
    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point as *mut POINT) };
    if ok == 0 {
        return Ok(None);
    }
    Ok(Some((point.x, point.y)))
}

/// Inclusive virtual-screen bounds `(left, top, right, bottom)` for the
/// calling process's window station, or `None` when metrics are unavailable
/// (for example from a non-interactive session).
#[cfg(windows)]
pub fn virtual_screen_bounds() -> Option<(i32, i32, i32, i32)> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return None;
    }

    Some((
        left,
        top,
        left.saturating_add(width.saturating_sub(1)),
        top.saturating_add(height.saturating_sub(1)),
    ))
}

#[cfg(windows)]
pub fn current_process_session_id() -> Result<u32> {
    let mut token = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(std::io::Error::last_os_error()).context("OpenProcessToken failed");
    }

    let mut session_id = 0u32;
    let mut returned_len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenSessionId,
            &mut session_id as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
            &mut returned_len,
        )
    };
    let close_result = unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("GetTokenInformation failed");
    }
    if close_result == 0 {
        return Err(std::io::Error::last_os_error()).context("CloseHandle failed");
    }
    Ok(session_id)
}

#[cfg(windows)]
pub fn current_process_can_use_interactive_input() -> Result<bool> {
    Ok(current_process_session_id()? != 0)
}

#[cfg(windows)]
pub fn is_virtual_key_down(vk: u16) -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    (state as u16 & 0x8000) != 0
}

#[cfg(windows)]
pub fn vk_to_scan_code(vk: u16) -> Option<u16> {
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) } as u16;
    if scan == 0 { None } else { Some(scan) }
}

#[cfg(windows)]
pub fn input_records_for_event_with_num_lock_state(
    event: &InputEvent,
    initial_num_lock_on: bool,
) -> Vec<INPUT> {
    input_records_for_events_with_num_lock_state(std::slice::from_ref(event), initial_num_lock_on)
}

#[cfg(windows)]
pub fn input_records_for_events_with_num_lock_state(
    events: &[InputEvent],
    initial_num_lock_on: bool,
) -> Vec<INPUT> {
    prepare_input_records(events, initial_num_lock_on).0
}

#[cfg(windows)]
fn prepare_input_records(events: &[InputEvent], initial_num_lock_on: bool) -> (Vec<INPUT>, bool) {
    let mut records = Vec::new();
    let mut destination_num_lock_on = initial_num_lock_on;
    for event in events {
        append_input_records_for_event(event, &mut destination_num_lock_on, &mut records);
    }
    (records, destination_num_lock_on)
}

#[cfg(windows)]
fn append_input_records_for_event(
    event: &InputEvent,
    destination_num_lock_on: &mut bool,
    records: &mut Vec<INPUT>,
) {
    match event {
        InputEvent::MouseMove { dx, dy } => {
            if *dx != 0 || *dy != 0 {
                records.push(mouse_input(*dx, *dy, 0, MOUSEEVENTF_MOVE));
            }
        }
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            records.push(mouse_input(
                i32::from(*x_norm),
                i32::from(*y_norm),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        InputEvent::MouseButton { button, state } => {
            let (flags, mouse_data) = match (button, state) {
                (MouseButton::Left, KeyState::Down) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, KeyState::Up) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, KeyState::Down) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, KeyState::Up) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, KeyState::Down) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, KeyState::Up) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::X1, KeyState::Down) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (MouseButton::X1, KeyState::Up) => (MOUSEEVENTF_XUP, XBUTTON1),
                (MouseButton::X2, KeyState::Down) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (MouseButton::X2, KeyState::Up) => (MOUSEEVENTF_XUP, XBUTTON2),
            };

            records.push(mouse_input(0, 0, mouse_data, flags));
        }
        InputEvent::MouseWheel { delta_x, delta_y } => {
            if *delta_y != 0 {
                records.push(mouse_input(0, 0, *delta_y as u32, MOUSEEVENTF_WHEEL));
            }
            if *delta_x != 0 {
                records.push(mouse_input(0, 0, *delta_x as u32, MOUSEEVENTF_HWHEEL));
            }
        }
        InputEvent::Key {
            scan_code,
            state,
            semantics,
        } => {
            let key_up = matches!(state, KeyState::Up);
            match semantics {
                KeySemantics::Physical => records.push(keyboard_input(*scan_code, key_up)),
                KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } if *virtual_key == VK_NUMLOCK_CODE => {
                    if !key_up && *destination_num_lock_on != *num_lock_on {
                        records.push(keyboard_virtual_key_input(VK_NUMLOCK_CODE, false));
                        records.push(keyboard_virtual_key_input(VK_NUMLOCK_CODE, true));
                        *destination_num_lock_on = *num_lock_on;
                    }
                }
                KeySemantics::Windows { num_lock_on, .. } => {
                    if keypad_scan_uses_num_lock(*scan_code)
                        && *destination_num_lock_on != *num_lock_on
                    {
                        records.push(keyboard_virtual_key_input(VK_NUMLOCK_CODE, false));
                        records.push(keyboard_virtual_key_input(VK_NUMLOCK_CODE, true));
                        *destination_num_lock_on = *num_lock_on;
                    }
                    records.push(keyboard_input(*scan_code, key_up));
                }
            }
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
fn keyboard_virtual_key_input(virtual_key: u16, key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: virtual_key,
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(windows)]
pub fn send_input_records(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }

    send_input_records_with_sender(inputs, |records| {
        let sent = unsafe {
            SendInput(
                records.len() as u32,
                records.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if sent == 0 {
            return Err(std::io::Error::last_os_error()).context("SendInput returned 0");
        }
        Ok(sent)
    })
}

#[cfg(windows)]
pub fn send_input_records_with_sender<F>(inputs: &[INPUT], mut sender: F) -> Result<()>
where
    F: FnMut(&[INPUT]) -> Result<u32>,
{
    let mut offset = 0usize;
    while offset < inputs.len() {
        let chunk = &inputs[offset..];
        let sent = sender(chunk).with_context(|| format!("send input record at index {offset}"))?;
        let sent = sent as usize;

        if sent == 0 {
            bail!(
                "partial send at index {offset}: sent 0 / {} input records",
                chunk.len()
            );
        }
        if sent > chunk.len() {
            bail!(
                "invalid send count at index {offset}: sent {sent} / {} input records",
                chunk.len()
            );
        }

        offset += sent;
    }
    Ok(())
}

#[cfg(windows)]
fn is_extended_scan_code(scan_code: u16) -> bool {
    matches!(scan_code & 0xFF00, 0xE000 | 0xE100)
}

#[cfg(windows)]
fn keypad_scan_uses_num_lock(scan_code: u16) -> bool {
    if is_extended_scan_code(scan_code) {
        return false;
    }
    matches!(scan_code & 0x00FF, 0x47..=0x49 | 0x4B..=0x4D | 0x4F..=0x53)
}

pub fn input_event_kind(event: &InputEvent) -> &'static str {
    match event {
        InputEvent::MouseMove { .. } => "mouse_move",
        InputEvent::MouseMoveAbsolute { .. } => "mouse_move_absolute",
        InputEvent::MouseButton { .. } => "mouse_button",
        InputEvent::MouseWheel { .. } => "mouse_wheel",
        InputEvent::Key { .. } => "key",
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn windows_key(
        scan_code: u16,
        virtual_key: u16,
        num_lock_on: bool,
        state: KeyState,
    ) -> InputEvent {
        InputEvent::Key {
            scan_code,
            state,
            semantics: KeySemantics::Windows {
                virtual_key,
                num_lock_on,
            },
        }
    }

    fn keyboard_record(input: &INPUT) -> KEYBDINPUT {
        assert_eq!(input.r#type, INPUT_KEYBOARD);
        unsafe { input.Anonymous.ki }
    }

    #[test]
    fn signed_high_resolution_wheel_deltas_reach_send_input_records() {
        for delta in [1, -1, 40, -40, 120, -120] {
            let records = input_records_for_event_with_num_lock_state(
                &InputEvent::MouseWheel {
                    delta_x: delta,
                    delta_y: delta,
                },
                false,
            );
            assert_eq!(records.len(), 2);

            let vertical = unsafe { records[0].Anonymous.mi };
            assert_eq!(vertical.mouseData as i32, delta);
            assert_eq!(vertical.dwFlags, MOUSEEVENTF_WHEEL);

            let horizontal = unsafe { records[1].Anonymous.mi };
            assert_eq!(horizontal.mouseData as i32, delta);
            assert_eq!(horizontal.dwFlags, MOUSEEVENTF_HWHEEL);
        }
    }

    #[test]
    fn keypad_digit_injection_honors_all_source_destination_num_lock_states() {
        const VK_NUMPAD7: u16 = 0x67;
        const NUMPAD7_SCAN: u16 = 0x47;

        for source_num_lock_on in [false, true] {
            for destination_num_lock_on in [false, true] {
                let virtual_key = if source_num_lock_on { VK_NUMPAD7 } else { 0x24 };
                let records = input_records_for_events_with_num_lock_state(
                    &[windows_key(
                        NUMPAD7_SCAN,
                        virtual_key,
                        source_num_lock_on,
                        KeyState::Down,
                    )],
                    destination_num_lock_on,
                );

                let keypad = keyboard_record(records.last().expect("keypad record"));
                assert_eq!(keypad.wScan, NUMPAD7_SCAN);
                assert_eq!(keypad.dwFlags & KEYEVENTF_SCANCODE, KEYEVENTF_SCANCODE);
                if source_num_lock_on == destination_num_lock_on {
                    assert_eq!(records.len(), 1);
                } else {
                    assert_eq!(records.len(), 3);
                    let toggle_down = keyboard_record(&records[0]);
                    let toggle_up = keyboard_record(&records[1]);
                    assert_eq!(toggle_down.wVk, VK_NUMLOCK_CODE);
                    assert_eq!(toggle_down.dwFlags & KEYEVENTF_KEYUP, 0);
                    assert_eq!(toggle_up.wVk, VK_NUMLOCK_CODE);
                    assert_eq!(toggle_up.dwFlags & KEYEVENTF_KEYUP, KEYEVENTF_KEYUP);
                }
            }
        }
    }

    #[test]
    fn captured_num_lock_and_repeated_keypad_events_toggle_destination_once() {
        const VK_NUMPAD1: u16 = 0x61;
        let events = [
            windows_key(0x45, VK_NUMLOCK_CODE, true, KeyState::Down),
            windows_key(0x45, VK_NUMLOCK_CODE, true, KeyState::Up),
            windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down),
            windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down),
            windows_key(0x4F, VK_NUMPAD1, true, KeyState::Up),
        ];

        let records = input_records_for_events_with_num_lock_state(&events, false);
        assert_eq!(records.len(), 5, "one toggle pair plus down/repeat/up");
        assert_eq!(keyboard_record(&records[0]).wVk, VK_NUMLOCK_CODE);
        assert_eq!(keyboard_record(&records[1]).wVk, VK_NUMLOCK_CODE);
        for record in &records[2..] {
            let key = keyboard_record(record);
            assert_eq!(key.wScan, 0x4F);
            assert_ne!(key.dwFlags & KEYEVENTF_SCANCODE, 0);
        }
        assert_eq!(
            keyboard_record(&records[4]).dwFlags & KEYEVENTF_KEYUP,
            KEYEVENTF_KEYUP
        );
    }

    #[test]
    fn remote_num_lock_state_persists_into_the_next_injected_frame() {
        const VK_NUMPAD1: u16 = 0x61;
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());

        input
            .send_events_with(
                &[
                    windows_key(0x45, VK_NUMLOCK_CODE, true, KeyState::Down),
                    windows_key(0x45, VK_NUMLOCK_CODE, true, KeyState::Up),
                ],
                |records| {
                    assert_eq!(records.len(), 2, "first frame toggles Num Lock once");
                    Ok(())
                },
            )
            .expect("commit first injected frame");
        assert!(num_lock.is_on());

        input
            .send_events_with(
                &[windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down)],
                |records| {
                    assert_eq!(
                        records.len(),
                        1,
                        "the next frame reuses committed destination state"
                    );
                    assert_eq!(keyboard_record(&records[0]).wScan, 0x4F);
                    Ok(())
                },
            )
            .expect("inject keypad frame");
    }

    #[test]
    fn failed_injected_frame_does_not_commit_num_lock_state() {
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());

        let error = input
            .send_events_with(
                &[windows_key(0x45, VK_NUMLOCK_CODE, true, KeyState::Down)],
                |_records| Err(anyhow::anyhow!("injected failure")),
            )
            .expect_err("failed SendInput must surface");

        assert!(error.to_string().contains("injected failure"));
        assert!(!num_lock.is_on());
    }

    #[test]
    fn existing_extended_keypad_and_navigation_identity_is_preserved() {
        const VK_RETURN: u16 = 0x0D;
        const VK_DIVIDE: u16 = 0x6F;
        const VK_HOME: u16 = 0x24;
        let records = input_records_for_events_with_num_lock_state(
            &[
                windows_key(0x1C, VK_RETURN, true, KeyState::Down),
                windows_key(0xE01C, VK_RETURN, true, KeyState::Down),
                windows_key(0xE035, VK_DIVIDE, true, KeyState::Down),
                windows_key(0xE047, VK_HOME, true, KeyState::Down),
            ],
            false,
        );

        assert_eq!(
            records.len(),
            4,
            "non-ambiguous keys must not toggle Num Lock"
        );
        let main_enter = keyboard_record(&records[0]);
        let keypad_enter = keyboard_record(&records[1]);
        let keypad_divide = keyboard_record(&records[2]);
        let dedicated_home = keyboard_record(&records[3]);
        assert_eq!(main_enter.wScan, 0x1C);
        assert_eq!(main_enter.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);
        for (record, scan) in [
            (keypad_enter, 0x1C),
            (keypad_divide, 0x35),
            (dedicated_home, 0x47),
        ] {
            assert_eq!(record.wScan, scan);
            assert_eq!(
                record.dwFlags & KEYEVENTF_EXTENDEDKEY,
                KEYEVENTF_EXTENDEDKEY
            );
        }
    }
}
