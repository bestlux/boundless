use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use core_input::{InputEvent, KeyState, MouseButton};
use tracing::warn;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::{
            GetRawInputData, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
            RAWMOUSE, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW,
            HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
            WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
            WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
};

const VK_LBUTTON_CODE: u16 = 0x01;
const VK_RBUTTON_CODE: u16 = 0x02;
const VK_MBUTTON_CODE: u16 = 0x04;
const VK_XBUTTON1_CODE: u16 = 0x05;
const VK_XBUTTON2_CODE: u16 = 0x06;
const VK_CONTROL_CODE: u16 = 0x11;
const VK_LCONTROL_CODE: u16 = 0xA2;
const VK_RCONTROL_CODE: u16 = 0xA3;
const XBUTTON1_DATA: u16 = 0x0001;
const XBUTTON2_DATA: u16 = 0x0002;
const LLKHF_EXTENDED_MASK: u32 = 0x01;
const LLKHF_INJECTED_MASK: u32 = 0x10;
const LLMHF_INJECTED_MASK: u32 = 0x0000_0001;
const RAW_INPUT_USAGE_PAGE_GENERIC: u16 = 0x01;
const RAW_INPUT_USAGE_MOUSE: u16 = 0x02;
const ESCAPE_DOUBLE_CTRL_WINDOW_MS: u64 = 400;
const STATIC_WINDOW_CLASS_NAME: [u16; 7] = [83, 84, 65, 84, 73, 67, 0];
const EMPTY_WINDOW_NAME: [u16; 1] = [0];

pub const HOOK_EVENT_QUEUE_CAP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookControlAction {
    EscapeUnlock,
}

#[derive(Debug, Clone)]
pub enum HookCaptureEvent {
    MouseDelta { dx: i32, dy: i32 },
    MousePosition { x: i32, y: i32 },
    Input(InputEvent),
    Control(HookControlAction),
}

#[derive(Debug, Default)]
struct HookRuntimeState {
    left_ctrl_down: bool,
    right_ctrl_down: bool,
    last_ctrl_tap_at: Option<Instant>,
}

type HookWakeNotifier = Arc<dyn Fn(&'static str) + Send + Sync + 'static>;

const CAPTURE_KEY_VIRTUAL_KEYS: &[u16] = &[
    0x08, 0x09, 0x0D, 0x14, 0x1B, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D,
    0x2E, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x41, 0x42, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50, 0x51, 0x52, 0x53,
    0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65,
    0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6D, 0x6E, 0x6F, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7A, 0x7B, 0x90, 0x91, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xBA,
    0xBB, 0xBC, 0xBD, 0xBE, 0xBF, 0xC0, 0xDB, 0xDC, 0xDD, 0xDE,
];

static HOOK_EVENT_SENDER: OnceLock<Mutex<Option<mpsc::SyncSender<HookCaptureEvent>>>> =
    OnceLock::new();
static HOOK_WAKE_NOTIFIER: OnceLock<Mutex<Option<HookWakeNotifier>>> = OnceLock::new();
static HOOK_RUNTIME_STATE: OnceLock<Mutex<HookRuntimeState>> = OnceLock::new();
static HOOK_LOCK_ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK_DROPPED_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct HookSenderGuard;

impl Drop for HookSenderGuard {
    fn drop(&mut self) {
        let _ = set_hook_event_sender(None);
        let _ = set_hook_wake_notifier(None);
    }
}

fn hook_sender_cell() -> &'static Mutex<Option<mpsc::SyncSender<HookCaptureEvent>>> {
    HOOK_EVENT_SENDER.get_or_init(|| Mutex::new(None))
}

fn hook_wake_notifier_cell() -> &'static Mutex<Option<HookWakeNotifier>> {
    HOOK_WAKE_NOTIFIER.get_or_init(|| Mutex::new(None))
}

fn hook_runtime_state_cell() -> &'static Mutex<HookRuntimeState> {
    HOOK_RUNTIME_STATE.get_or_init(|| Mutex::new(HookRuntimeState::default()))
}

pub fn set_hook_event_sender(sender: Option<mpsc::SyncSender<HookCaptureEvent>>) -> Result<()> {
    let mut guard = hook_sender_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook sender mutex poisoned"))?;
    *guard = sender;
    Ok(())
}

pub fn set_hook_wake_notifier(notifier: Option<HookWakeNotifier>) -> Result<()> {
    let mut guard = hook_wake_notifier_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook wake notifier mutex poisoned"))?;
    *guard = notifier;
    Ok(())
}

pub fn send_hook_event(event: HookCaptureEvent, source: &'static str) {
    let sender = hook_sender_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(sender) = sender {
        match sender.try_send(event) {
            Ok(()) => {
                if let Some(notifier) = hook_wake_notifier_cell()
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().cloned())
                {
                    notifier(source);
                }
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                HOOK_DROPPED_EVENT_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub fn take_hook_dropped_event_count() -> u64 {
    HOOK_DROPPED_EVENT_COUNT.swap(0, Ordering::Relaxed)
}

pub fn set_hook_lock_active(active: bool) -> Result<()> {
    HOOK_LOCK_ACTIVE.store(active, Ordering::Relaxed);
    let mut state = hook_runtime_state_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("hook runtime state mutex poisoned"))?;
    if !active {
        state.left_ctrl_down = false;
        state.right_ctrl_down = false;
        state.last_ctrl_tap_at = None;
    }
    Ok(())
}

pub fn is_hook_lock_active() -> bool {
    HOOK_LOCK_ACTIVE.load(Ordering::Relaxed)
}

pub fn update_escape_state_for_key(vk_code: u16, key_state: KeyState) -> bool {
    if vk_code != VK_CONTROL_CODE && vk_code != VK_LCONTROL_CODE && vk_code != VK_RCONTROL_CODE {
        return false;
    }

    let now = Instant::now();
    let mut state = match hook_runtime_state_cell().lock() {
        Ok(state) => state,
        Err(_) => return false,
    };
    if !HOOK_LOCK_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }

    let is_down = matches!(key_state, KeyState::Down);
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
            let triggered = state.last_ctrl_tap_at.is_some_and(|previous| {
                now.duration_since(previous) <= Duration::from_millis(ESCAPE_DOUBLE_CTRL_WINDOW_MS)
            });
            state.last_ctrl_tap_at = Some(now);
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

pub fn mouse_button_virtual_keys() -> [(u16, MouseButton); 5] {
    [
        (VK_LBUTTON_CODE, MouseButton::Left),
        (VK_RBUTTON_CODE, MouseButton::Right),
        (VK_MBUTTON_CODE, MouseButton::Middle),
        (VK_XBUTTON1_CODE, MouseButton::X1),
        (VK_XBUTTON2_CODE, MouseButton::X2),
    ]
}

pub fn mouse_button_from_virtual_key(vk: u16) -> Option<MouseButton> {
    match vk {
        VK_LBUTTON_CODE => Some(MouseButton::Left),
        VK_RBUTTON_CODE => Some(MouseButton::Right),
        VK_MBUTTON_CODE => Some(MouseButton::Middle),
        VK_XBUTTON1_CODE => Some(MouseButton::X1),
        VK_XBUTTON2_CODE => Some(MouseButton::X2),
        _ => None,
    }
}

pub fn virtual_key_for_mouse_button(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => VK_LBUTTON_CODE,
        MouseButton::Right => VK_RBUTTON_CODE,
        MouseButton::Middle => VK_MBUTTON_CODE,
        MouseButton::X1 => VK_XBUTTON1_CODE,
        MouseButton::X2 => VK_XBUTTON2_CODE,
    }
}

pub fn captured_key_virtual_keys() -> &'static [u16] {
    CAPTURE_KEY_VIRTUAL_KEYS
}

/// # Safety
/// The returned hook handle must only be used on a thread that pumps the Win32
/// message loop and eventually unhooked with `unhook_windows_hook`.
pub unsafe fn install_keyboard_hook() -> Result<isize> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW keyboard");
    }
    Ok(hook as isize)
}

/// # Safety
/// The returned hook handle must only be used on a thread that pumps the Win32
/// message loop and eventually unhooked with `unhook_windows_hook`.
pub unsafe fn install_mouse_hook() -> Result<isize> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), module, 0) };
    if hook.is_null() {
        return Err(std::io::Error::last_os_error()).context("SetWindowsHookExW mouse");
    }
    Ok(hook as isize)
}

/// # Safety
/// Must run on the thread that installed the low-level hooks so Win32 dispatches
/// hook callbacks against the expected thread-local message loop.
pub unsafe fn run_hook_message_loop() -> Result<()> {
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg as *mut MSG, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("GetMessageW hook message loop");
        }
        if result == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg as *const MSG);
            DispatchMessageW(&msg as *const MSG);
        }
    }
    Ok(())
}

pub fn unhook_windows_hook(hook: isize) {
    let handle = hook as HHOOK;
    if !handle.is_null() {
        unsafe {
            let _ = UnhookWindowsHookEx(handle);
        }
    }
}

pub fn post_thread_quit(thread_id: u32) {
    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
    }
}

pub fn spawn_raw_input_thread() -> Result<(u32, JoinHandle<()>)> {
    let (startup_tx, startup_rx) = mpsc::channel::<Result<u32>>();
    let thread = thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hwnd = match create_raw_input_window() {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = startup_tx.send(Err(error));
                return;
            }
        };

        if let Err(error) = register_raw_input_mouse_device(hwnd) {
            let _ = startup_tx.send(Err(error));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }

        let _ = startup_tx.send(Ok(thread_id));
        unsafe {
            if let Err(error) = run_raw_input_message_loop() {
                warn!(error = ?error, "raw input message loop exited with error");
            }
            let _ = DestroyWindow(hwnd);
        }
    });

    let thread_id = match startup_rx.recv() {
        Ok(Ok(thread_id)) => thread_id,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = thread.join();
            return Err(anyhow::anyhow!("raw input startup channel closed"));
        }
    };

    Ok((thread_id, thread))
}

fn create_raw_input_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            STATIC_WINDOW_CLASS_NAME.as_ptr(),
            EMPTY_WINDOW_NAME.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error()).context("CreateWindowExW raw input window");
    }
    Ok(hwnd)
}

fn register_raw_input_mouse_device(hwnd: HWND) -> Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
        usUsage: RAW_INPUT_USAGE_MOUSE,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    let ok = unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("RegisterRawInputDevices mouse");
    }
    Ok(())
}

unsafe fn run_raw_input_message_loop() -> Result<()> {
    let mut warned_once = false;
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg as *mut MSG, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(std::io::Error::last_os_error())
                .context("GetMessageW raw input message loop");
        }
        if result == 0 {
            break;
        }

        if msg.message == WM_INPUT {
            match process_raw_input_message(msg.lParam) {
                Ok(()) => warned_once = false,
                Err(error) => {
                    if !warned_once {
                        warn!(error = ?error, "raw input message processing failed");
                        warned_once = true;
                    }
                }
            }
            continue;
        }

        unsafe {
            TranslateMessage(&msg as *const MSG);
            DispatchMessageW(&msg as *const MSG);
        }
    }
    Ok(())
}

fn process_raw_input_message(lparam: LPARAM) -> Result<()> {
    if !is_hook_lock_active() {
        return Ok(());
    }

    let hrawinput = lparam as *mut core::ffi::c_void;
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut raw_size = 0u32;
    let query_size = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            std::ptr::null_mut(),
            &mut raw_size as *mut u32,
            header_size,
        )
    };
    if query_size == u32::MAX {
        return Err(std::io::Error::last_os_error()).context("GetRawInputData query size");
    }
    if raw_size < header_size {
        return Ok(());
    }

    let mut buffer = vec![0u8; raw_size as usize];
    let read_size = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            buffer.as_mut_ptr().cast(),
            &mut raw_size as *mut u32,
            header_size,
        )
    };
    if read_size == u32::MAX {
        return Err(std::io::Error::last_os_error()).context("GetRawInputData read payload");
    }
    if read_size < header_size {
        return Ok(());
    }

    let raw = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<RAWINPUT>()) };
    if raw.header.dwType != RIM_TYPEMOUSE {
        return Ok(());
    }

    let mouse = unsafe { raw.data.mouse };
    if let Some((dx, dy)) = raw_mouse_relative_delta(&mouse) {
        send_hook_event(HookCaptureEvent::MouseDelta { dx, dy }, "raw_input");
    }

    Ok(())
}

pub fn raw_mouse_relative_delta(mouse: &RAWMOUSE) -> Option<(i32, i32)> {
    if (mouse.usFlags & MOUSE_MOVE_ABSOLUTE) != 0 {
        return None;
    }
    if mouse.lLastX == 0 && mouse.lLastY == 0 {
        return None;
    }
    Some((mouse.lLastX, mouse.lLastY))
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut lock_active = false;
    if code == HC_ACTION as i32 {
        let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if (keyboard.flags & LLKHF_INJECTED_MASK) == 0 {
            lock_active = is_hook_lock_active();
            let state = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(KeyState::Down),
                WM_KEYUP | WM_SYSKEYUP => Some(KeyState::Up),
                _ => None,
            };

            if let Some(state) = state {
                let mut scan_code = keyboard.scanCode as u16;
                if (keyboard.flags & LLKHF_EXTENDED_MASK) != 0 {
                    scan_code |= 0xE000;
                }
                send_hook_event(
                    HookCaptureEvent::Input(InputEvent::Key { scan_code, state }),
                    "keyboard_hook",
                );

                if lock_active && update_escape_state_for_key(keyboard.vkCode as u16, state) {
                    send_hook_event(
                        HookCaptureEvent::Control(HookControlAction::EscapeUnlock),
                        "keyboard_hook",
                    );
                }
            }
        }
    }

    if lock_active {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut lock_active = false;
    if code == HC_ACTION as i32 {
        let mouse = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if (mouse.flags & LLMHF_INJECTED_MASK) == 0 {
            lock_active = is_hook_lock_active();
            match wparam as u32 {
                WM_MOUSEMOVE => {
                    send_hook_event(
                        HookCaptureEvent::MousePosition {
                            x: mouse.pt.x,
                            y: mouse.pt.y,
                        },
                        "mouse_hook",
                    );
                }
                WM_LBUTTONDOWN => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Down,
                    }),
                    "mouse_hook",
                ),
                WM_LBUTTONUP => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Left,
                        state: KeyState::Up,
                    }),
                    "mouse_hook",
                ),
                WM_RBUTTONDOWN => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Right,
                        state: KeyState::Down,
                    }),
                    "mouse_hook",
                ),
                WM_RBUTTONUP => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Right,
                        state: KeyState::Up,
                    }),
                    "mouse_hook",
                ),
                WM_MBUTTONDOWN => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Middle,
                        state: KeyState::Down,
                    }),
                    "mouse_hook",
                ),
                WM_MBUTTONUP => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseButton {
                        button: MouseButton::Middle,
                        state: KeyState::Up,
                    }),
                    "mouse_hook",
                ),
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let button = match crate::input::high_word(mouse.mouseData) {
                        XBUTTON1_DATA => Some(MouseButton::X1),
                        XBUTTON2_DATA => Some(MouseButton::X2),
                        _ => None,
                    };
                    if let Some(button) = button {
                        send_hook_event(
                            HookCaptureEvent::Input(InputEvent::MouseButton {
                                button,
                                state: if (wparam as u32) == WM_XBUTTONDOWN {
                                    KeyState::Down
                                } else {
                                    KeyState::Up
                                },
                            }),
                            "mouse_hook",
                        );
                    }
                }
                WM_MOUSEWHEEL => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseWheel {
                        delta_x: 0,
                        delta_y: crate::input::signed_high_word(mouse.mouseData),
                    }),
                    "mouse_hook",
                ),
                WM_MOUSEHWHEEL => send_hook_event(
                    HookCaptureEvent::Input(InputEvent::MouseWheel {
                        delta_x: crate::input::signed_high_word(mouse.mouseData),
                        delta_y: 0,
                    }),
                    "mouse_hook",
                ),
                _ => {}
            }
        }
    }

    if lock_active {
        return 1;
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}
