use super::*;

pub(super) fn mouse_button_virtual_keys() -> [(u16, core_input::MouseButton); 5] {
    [
        (VK_LBUTTON_CODE, core_input::MouseButton::Left),
        (VK_RBUTTON_CODE, core_input::MouseButton::Right),
        (VK_MBUTTON_CODE, core_input::MouseButton::Middle),
        (VK_XBUTTON1_CODE, core_input::MouseButton::X1),
        (VK_XBUTTON2_CODE, core_input::MouseButton::X2),
    ]
}

pub(super) fn mouse_button_from_virtual_key(vk: u16) -> Option<core_input::MouseButton> {
    match vk {
        VK_LBUTTON_CODE => Some(core_input::MouseButton::Left),
        VK_RBUTTON_CODE => Some(core_input::MouseButton::Right),
        VK_MBUTTON_CODE => Some(core_input::MouseButton::Middle),
        VK_XBUTTON1_CODE => Some(core_input::MouseButton::X1),
        VK_XBUTTON2_CODE => Some(core_input::MouseButton::X2),
        _ => None,
    }
}

pub(super) fn virtual_key_for_mouse_button(button: core_input::MouseButton) -> u16 {
    match button {
        core_input::MouseButton::Left => VK_LBUTTON_CODE,
        core_input::MouseButton::Right => VK_RBUTTON_CODE,
        core_input::MouseButton::Middle => VK_MBUTTON_CODE,
        core_input::MouseButton::X1 => VK_XBUTTON1_CODE,
        core_input::MouseButton::X2 => VK_XBUTTON2_CODE,
    }
}

pub(super) fn captured_key_virtual_keys() -> &'static [u16] {
    CAPTURE_KEY_VIRTUAL_KEYS
}

pub(super) struct HookSenderGuard;

impl Drop for HookSenderGuard {
    fn drop(&mut self) {
        let _ = set_hook_event_sender(None);
    }
}

fn hook_sender_cell() -> &'static Mutex<Option<mpsc::Sender<HookCaptureEvent>>> {
    HOOK_EVENT_SENDER.get_or_init(|| Mutex::new(None))
}

pub(super) fn set_hook_event_sender(sender: Option<mpsc::Sender<HookCaptureEvent>>) -> Result<()> {
    let mut guard = hook_sender_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook sender mutex poisoned"))?;
    *guard = sender;
    Ok(())
}

pub(super) fn send_hook_event(event: HookCaptureEvent) {
    let sender = hook_sender_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(sender) = sender {
        let _ = sender.send(event);
    }
}

fn hook_runtime_state_cell() -> &'static Mutex<HookRuntimeState> {
    HOOK_RUNTIME_STATE.get_or_init(|| Mutex::new(HookRuntimeState::default()))
}

pub(super) fn set_hook_lock_active(active: bool) -> Result<()> {
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

pub(super) fn is_hook_lock_active() -> bool {
    hook_runtime_state_cell()
        .lock()
        .map(|state| state.lock_active)
        .unwrap_or(false)
}

pub(super) fn update_escape_state_for_key(vk_code: u16, key_state: core_input::KeyState) -> bool {
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
