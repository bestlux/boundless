use std::sync::{Mutex, OnceLock, mpsc};

use super::*;

const VK_LBUTTON_CODE: u16 = 0x01;
const VK_RBUTTON_CODE: u16 = 0x02;
const VK_MBUTTON_CODE: u16 = 0x04;
const VK_XBUTTON1_CODE: u16 = 0x05;
const VK_XBUTTON2_CODE: u16 = 0x06;
const VK_CONTROL_CODE: u16 = 0x11;
const VK_LCONTROL_CODE: u16 = 0xA2;
const VK_RCONTROL_CODE: u16 = 0xA3;

static HOOK_EVENT_SENDER: OnceLock<Mutex<Option<mpsc::Sender<HookCaptureEvent>>>> = OnceLock::new();
static HOOK_RUNTIME_STATE: OnceLock<Mutex<HookRuntimeState>> = OnceLock::new();

#[derive(Debug, Default)]
struct HookRuntimeState {
    lock_active: bool,
    left_ctrl_down: bool,
    right_ctrl_down: bool,
    last_ctrl_tap_unix_ms: Option<u64>,
}

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
