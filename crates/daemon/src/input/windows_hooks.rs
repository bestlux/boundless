use super::*;

const XBUTTON1_DATA: u16 = 0x0001;
const XBUTTON2_DATA: u16 = 0x0002;
const LLKHF_EXTENDED_MASK: u32 = 0x01;
const LLKHF_INJECTED_MASK: u32 = 0x10;
const LLMHF_INJECTED_MASK: u32 = 0x0000_0001;

pub(super) unsafe fn install_keyboard_hook() -> Result<HHOOK> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW keyboard");
    }
    Ok(hook)
}

#[cfg(windows)]
pub(super) unsafe fn install_mouse_hook() -> Result<HHOOK> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW mouse");
    }
    Ok(hook)
}

#[cfg(windows)]
pub(super) unsafe fn run_hook_message_loop() {
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
