use super::*;

pub(super) fn high_word(value: u32) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

#[cfg(windows)]
pub(super) fn signed_high_word(value: u32) -> i32 {
    i16::from_ne_bytes(high_word(value).to_ne_bytes()) as i32
}

#[cfg(windows)]
pub(super) fn cursor_position() -> Result<Option<(i32, i32)>> {
    let mut point = POINT { x: 0, y: 0 };
    let ok = unsafe { GetCursorPos(&mut point as *mut POINT) };
    if ok == 0 {
        return Ok(None);
    }
    Ok(Some((point.x, point.y)))
}

#[cfg(windows)]
pub(super) fn is_virtual_key_down(vk: u16) -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    (state as u16 & 0x8000) != 0
}

#[cfg(windows)]
pub(super) fn vk_to_scan_code(vk: u16) -> Option<u16> {
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) } as u16;
    if scan == 0 { None } else { Some(scan) }
}

#[cfg(windows)]
pub(super) fn input_records_for_event(event: &InputEvent) -> Vec<INPUT> {
    match event {
        InputEvent::MouseMove { dx, dy } => {
            if *dx == 0 && *dy == 0 {
                Vec::new()
            } else {
                vec![mouse_input(*dx, *dy, 0, MOUSEEVENTF_MOVE)]
            }
        }
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            vec![mouse_input(
                i32::from(*x_norm),
                i32::from(*y_norm),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]
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
pub(super) fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> INPUT {
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
pub(super) fn keyboard_input(scan_code: u16, key_up: bool) -> INPUT {
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
pub(super) fn send_input_records(inputs: &[INPUT]) -> Result<()> {
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
pub(super) fn send_input_records_with_sender<F>(inputs: &[INPUT], mut sender: F) -> Result<()>
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
pub(super) fn is_extended_scan_code(scan_code: u16) -> bool {
    matches!(scan_code & 0xFF00, 0xE000 | 0xE100)
}

#[cfg(windows)]
pub(super) fn input_event_kind(event: &InputEvent) -> &'static str {
    match event {
        InputEvent::MouseMove { .. } => "mouse_move",
        InputEvent::MouseMoveAbsolute { .. } => "mouse_move_absolute",
        InputEvent::MouseButton { .. } => "mouse_button",
        InputEvent::MouseWheel { .. } => "mouse_wheel",
        InputEvent::Key { .. } => "key",
    }
}
