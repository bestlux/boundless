use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use core_input::{InputEvent, KeySemantics, KeyState, MouseButton};
use tracing::warn;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::{
            GetCurrentInputMessageSource, GetRawInputData, GetRegisteredRawInputDevices,
            IMO_INJECTED, INPUT_MESSAGE_SOURCE,
            KeyboardAndMouse::{GetDoubleClickTime, GetKeyState},
            MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE,
            RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW,
            HC_ACTION, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
            PostThreadMessageW, RI_KEY_BREAK, RI_KEY_E0, RI_MOUSE_HWHEEL, RI_MOUSE_WHEEL,
            SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
            WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
            WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
            WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
};

use super::{
    BOUNDLESS_INJECTED_INPUT_MARKER, VK_NUMLOCK_CODE, WindowsNumLockState, is_virtual_key_down,
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
const RAW_INPUT_USAGE_KEYBOARD: u16 = 0x06;
const ESCAPE_DOUBLE_CTRL_MIN_WINDOW_MS: u64 = 800;
const ESCAPE_DOUBLE_CTRL_MAX_WINDOW_MS: u64 = 1_200;
const RAW_INPUT_REGISTRATION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const STATIC_WINDOW_CLASS_NAME: [u16; 7] = [83, 84, 65, 84, 73, 67, 0];
const EMPTY_WINDOW_NAME: [u16; 1] = [0];

pub const HOOK_EVENT_QUEUE_CAP: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookControlAction {
    EscapeUnlock,
    LeaseExpiredUnlock,
    DetectorUnavailableUnlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelCaptureSource {
    RawDevice,
    RawSystem,
    Hook,
}

impl WheelCaptureSource {
    pub fn is_raw(self) -> bool {
        matches!(self, Self::RawDevice | Self::RawSystem)
    }
}

#[derive(Debug, Clone)]
pub struct CapturedWheelEvent {
    pub delta_x: i32,
    pub delta_y: i32,
    pub source: WheelCaptureSource,
    /// Win32 message timestamp. Matching raw/hook copies share this value;
    /// repeated physical wheel inputs remain distinct queue entries.
    pub message_time_ms: u32,
    pub observed_at: Instant,
}

#[derive(Debug, Clone)]
pub enum HookCaptureEvent {
    MouseDelta { dx: i32, dy: i32 },
    MousePosition { x: i32, y: i32 },
    Input(InputEvent),
    Wheel(CapturedWheelEvent),
}

pub struct CaptureRuntime {
    core: Arc<CaptureRuntimeCore>,
    event_rx: mpsc::Receiver<HookCaptureEvent>,
    hook_thread_id: u32,
    hook_thread: Option<JoinHandle<()>>,
    raw_input_thread_id: Option<u32>,
    raw_input_thread: Option<JoinHandle<()>>,
    raw_input_hwnd: Option<isize>,
    raw_input_registration_checked_at: Instant,
    raw_input_enabled: bool,
    lock_watchdog: Option<JoinHandle<()>>,
}

struct CaptureRuntimeCore {
    event_tx: Mutex<Option<SyncSender<HookCaptureEvent>>>,
    wake_notifier: Mutex<Option<HookWakeNotifier>>,
    escape_detector_state: Mutex<EscapeDetectorState>,
    keyboard_state: Mutex<KeyboardRuntimeState>,
    num_lock_state: WindowsNumLockState,
    lock_lease: Mutex<HookLockLease>,
    lock_active: AtomicBool,
    keyboard_hook_degraded: AtomicBool,
    last_keyboard_hook_observation: Mutex<Option<KeyboardHookObservation>>,
    escape_unlock_pending: AtomicU64,
    lease_expired_unlock_pending: AtomicU64,
    detector_unavailable_unlock_pending: AtomicU64,
    safety_unlock_generation: AtomicU64,
    safety_unlock_in_progress: AtomicU64,
    lock_watchdog_stop: AtomicBool,
    dropped_event_count: AtomicU64,
}

#[derive(Debug, Default)]
struct HookRuntimeState {
    left_ctrl_down_at: Option<Instant>,
    right_ctrl_down_at: Option<Instant>,
    current_ctrl_tap_valid: bool,
    last_ctrl_tap_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct KeyboardRuntimeState {
    num_lock_down: bool,
}

#[derive(Debug, Default)]
struct HookLockLease {
    timeout: Option<Duration>,
    last_renewed_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeDetectorSource {
    RawKeyboard,
    KeyboardHook,
}

impl EscapeDetectorSource {
    fn label(self) -> &'static str {
        match self {
            Self::RawKeyboard => "raw_keyboard",
            Self::KeyboardHook => "keyboard_hook",
        }
    }
}

#[derive(Debug)]
struct EscapeDetectorState {
    source: EscapeDetectorSource,
    gesture: HookRuntimeState,
}

impl EscapeDetectorState {
    fn new(source: EscapeDetectorSource) -> Self {
        Self {
            source,
            gesture: HookRuntimeState::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyboardHookObservation {
    message_time_ms: u32,
    state: KeyState,
}

#[derive(Debug, Clone, Copy)]
enum SafetyUnlockCause {
    Escape,
    LeaseExpired,
    DetectorUnavailable,
}

type HookWakeNotifier = Arc<dyn Fn(&'static str) + Send + Sync + 'static>;

const CAPTURE_KEY_VIRTUAL_KEYS: &[u16] = &[
    0x08, 0x09, 0x0D, 0x14, 0x1B, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46,
    0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56,
    0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x6F, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79,
    0x7A, 0x7B, 0x90, 0x91, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF,
    0xC0, 0xDB, 0xDC, 0xDD, 0xDE,
];

static ACTIVE_CAPTURE_RUNTIME: OnceLock<Mutex<Option<Weak<CaptureRuntimeCore>>>> = OnceLock::new();

impl CaptureRuntime {
    pub fn start<F>(wake_notifier: F) -> Result<Self>
    where
        F: Fn(&'static str) + Send + Sync + 'static,
    {
        let (event_tx, event_rx) = mpsc::sync_channel::<HookCaptureEvent>(HOOK_EVENT_QUEUE_CAP);
        let (startup_tx, startup_rx) = mpsc::channel::<Result<u32>>();
        let core = Arc::new(CaptureRuntimeCore {
            event_tx: Mutex::new(Some(event_tx)),
            wake_notifier: Mutex::new(Some(Arc::new(wake_notifier))),
            escape_detector_state: Mutex::new(EscapeDetectorState::new(
                EscapeDetectorSource::KeyboardHook,
            )),
            keyboard_state: Mutex::new(KeyboardRuntimeState::default()),
            num_lock_state: WindowsNumLockState::new(false),
            lock_lease: Mutex::new(HookLockLease::default()),
            lock_active: AtomicBool::new(false),
            keyboard_hook_degraded: AtomicBool::new(false),
            last_keyboard_hook_observation: Mutex::new(None),
            escape_unlock_pending: AtomicU64::new(0),
            lease_expired_unlock_pending: AtomicU64::new(0),
            detector_unavailable_unlock_pending: AtomicU64::new(0),
            safety_unlock_generation: AtomicU64::new(0),
            safety_unlock_in_progress: AtomicU64::new(0),
            lock_watchdog_stop: AtomicBool::new(false),
            dropped_event_count: AtomicU64::new(0),
        });

        activate_capture_runtime(&core)?;

        let hook_core = Arc::clone(&core);
        let hook_thread = thread::spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            let keyboard_hook = unsafe { install_keyboard_hook() };
            let mouse_hook = unsafe { install_mouse_hook() };
            match (keyboard_hook, mouse_hook) {
                (Ok(keyboard_hook), Ok(mouse_hook)) => {
                    // This thread owns the low-level keyboard hook and pumps
                    // its Win32 message queue. Seed toggle state here rather
                    // than from a Tokio/supervisor thread whose key state can
                    // be stale indefinitely.
                    seed_num_lock_state_from_message_lane(&hook_core);
                    let _ = startup_tx.send(Ok(thread_id));
                    if let Err(error) = unsafe { run_hook_message_loop() } {
                        warn!(error = ?error, "hook message loop exited with error");
                    }
                    unhook_windows_hook(keyboard_hook);
                    unhook_windows_hook(mouse_hook);
                }
                (keyboard, mouse) => {
                    if let Ok(hook) = keyboard.as_ref() {
                        unhook_windows_hook(*hook);
                    }
                    if let Ok(hook) = mouse.as_ref() {
                        unhook_windows_hook(*hook);
                    }
                    let error = keyboard
                        .err()
                        .or_else(|| mouse.err())
                        .unwrap_or_else(|| anyhow::anyhow!("failed to install capture hooks"));
                    let _ = startup_tx.send(Err(error));
                }
            }
        });

        let hook_thread_id = match startup_rx.recv().context("hook startup channel closed")? {
            Ok(thread_id) => thread_id,
            Err(error) => {
                let _ = clear_active_capture_runtime(&core);
                let _ = hook_thread.join();
                return Err(error);
            }
        };
        let (raw_input_thread_id, raw_input_thread, raw_input_hwnd, raw_input_enabled) =
            match spawn_raw_input_thread() {
                Ok((thread_id, hwnd, thread)) => {
                    set_raw_keyboard_escape_enabled_for(&core, true);
                    (Some(thread_id), Some(thread), Some(hwnd), true)
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        "raw input capture unavailable; falling back to low-level hooks"
                    );
                    (None, None, None, false)
                }
            };
        Ok(Self {
            core,
            event_rx,
            hook_thread_id,
            hook_thread: Some(hook_thread),
            raw_input_thread_id,
            raw_input_thread,
            raw_input_hwnd,
            raw_input_registration_checked_at: Instant::now(),
            raw_input_enabled,
            lock_watchdog: None,
        })
    }

    #[doc(hidden)]
    pub fn from_test_parts(
        event_rx: mpsc::Receiver<HookCaptureEvent>,
        raw_input_enabled: bool,
    ) -> Self {
        Self {
            core: Arc::new(CaptureRuntimeCore {
                event_tx: Mutex::new(None),
                wake_notifier: Mutex::new(None),
                escape_detector_state: Mutex::new(EscapeDetectorState::new(if raw_input_enabled {
                    EscapeDetectorSource::RawKeyboard
                } else {
                    EscapeDetectorSource::KeyboardHook
                })),
                keyboard_state: Mutex::new(KeyboardRuntimeState::default()),
                num_lock_state: WindowsNumLockState::new(false),
                lock_lease: Mutex::new(HookLockLease::default()),
                lock_active: AtomicBool::new(false),
                keyboard_hook_degraded: AtomicBool::new(false),
                last_keyboard_hook_observation: Mutex::new(None),
                escape_unlock_pending: AtomicU64::new(0),
                lease_expired_unlock_pending: AtomicU64::new(0),
                detector_unavailable_unlock_pending: AtomicU64::new(0),
                safety_unlock_generation: AtomicU64::new(0),
                safety_unlock_in_progress: AtomicU64::new(0),
                lock_watchdog_stop: AtomicBool::new(false),
                dropped_event_count: AtomicU64::new(0),
            }),
            event_rx,
            hook_thread_id: 0,
            hook_thread: None,
            raw_input_thread_id: None,
            raw_input_thread: None,
            raw_input_hwnd: None,
            raw_input_registration_checked_at: Instant::now(),
            raw_input_enabled,
            lock_watchdog: None,
        }
    }

    pub fn drain_events(&mut self) -> Vec<HookCaptureEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    pub fn refresh(&mut self) -> bool {
        let finished = self
            .raw_input_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished());
        if finished {
            if let Some(thread) = self.raw_input_thread.take() {
                let _ = thread.join();
            }
            self.raw_input_thread_id = None;
            self.raw_input_hwnd = None;
            self.raw_input_enabled = false;
            set_raw_keyboard_escape_enabled_for(&self.core, false);
            warn!("raw input capture thread exited; using low-level hook fallback");
            return false;
        }

        let Some(hwnd) = self.raw_input_hwnd else {
            // Test runtimes and platforms without a live raw-input thread keep
            // their explicitly supplied backend state.
            return self.raw_input_enabled;
        };
        if self.raw_input_registration_checked_at.elapsed() < RAW_INPUT_REGISTRATION_CHECK_INTERVAL
        {
            return self.raw_input_enabled;
        }
        self.raw_input_registration_checked_at = Instant::now();

        let hwnd = hwnd as HWND;
        match raw_input_registration_owned(hwnd) {
            Ok(true) => {
                self.raw_input_enabled = true;
                set_raw_keyboard_escape_enabled_for(&self.core, true);
            }
            Ok(false) => match register_raw_input_devices(hwnd) {
                Ok(()) => {
                    self.raw_input_enabled = true;
                    set_raw_keyboard_escape_enabled_for(&self.core, true);
                    warn!("raw input registration was replaced and has been reclaimed");
                }
                Err(error) => {
                    self.raw_input_enabled = false;
                    set_raw_keyboard_escape_enabled_for(&self.core, false);
                    warn!(error = ?error, "failed to reclaim raw input registration");
                }
            },
            Err(error) => {
                self.raw_input_enabled = false;
                set_raw_keyboard_escape_enabled_for(&self.core, false);
                warn!(error = ?error, "failed to verify raw input registration ownership");
            }
        }
        self.raw_input_enabled
    }

    pub fn raw_input_enabled(&self) -> bool {
        self.raw_input_enabled
    }

    pub fn keyboard_hook_degraded(&self) -> bool {
        self.core.keyboard_hook_degraded.load(Ordering::Acquire)
    }

    pub fn num_lock_state(&self) -> WindowsNumLockState {
        self.core.num_lock_state.clone()
    }

    pub fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        set_hook_lock_active_for(&self.core, active)
    }

    pub fn safety_unlock_generation(&self) -> u64 {
        self.core.safety_unlock_generation.load(Ordering::SeqCst)
    }

    /// Applies a daemon-requested relock only if no local safety unlock or
    /// shutdown unlock occurred since `expected_generation` was sampled. A
    /// post-store generation check closes the final check/store race.
    pub fn set_lock_active_if_safety_generation(
        &mut self,
        active: bool,
        expected_generation: u64,
    ) -> Result<bool> {
        set_hook_lock_active_if_generation_for(&self.core, active, expected_generation, || {})
    }

    /// Enables a fail-open lease for the local hook lock. The caller must
    /// renew the lease after every successful broker exchange; a stalled IPC
    /// path can therefore never strand local input indefinitely.
    pub fn enable_lock_lease(&mut self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            anyhow::bail!("hook lock lease timeout must be greater than zero");
        }

        {
            let mut lease = self
                .core
                .lock_lease
                .lock()
                .map_err(|_| anyhow::anyhow!("hook lock lease mutex poisoned"))?;
            lease.timeout = Some(timeout);
            lease.last_renewed_at = self.lock_active().then(Instant::now);
        }

        if self.lock_watchdog.is_none() {
            self.core.lock_watchdog_stop.store(false, Ordering::Release);
            let core = Arc::clone(&self.core);
            self.lock_watchdog = Some(
                thread::Builder::new()
                    .name("boundless-input-lock-watchdog".to_string())
                    .spawn(move || run_lock_watchdog(core))
                    .context("spawn input lock watchdog")?,
            );
        }
        Ok(())
    }

    pub fn renew_lock_lease(&self) -> bool {
        renew_hook_lock_lease_for(&self.core, Instant::now())
    }

    pub fn drain_control_actions(&mut self) -> Vec<HookControlAction> {
        let escape_count = self.core.escape_unlock_pending.swap(0, Ordering::AcqRel);
        let lease_expired_count = self
            .core
            .lease_expired_unlock_pending
            .swap(0, Ordering::AcqRel);
        let detector_unavailable_count = self
            .core
            .detector_unavailable_unlock_pending
            .swap(0, Ordering::AcqRel);
        let mut actions = Vec::with_capacity(
            escape_count
                .saturating_add(lease_expired_count)
                .saturating_add(detector_unavailable_count)
                .min(usize::MAX as u64) as usize,
        );
        actions.extend((0..escape_count).map(|_| HookControlAction::EscapeUnlock));
        actions.extend((0..lease_expired_count).map(|_| HookControlAction::LeaseExpiredUnlock));
        actions.extend(
            (0..detector_unavailable_count).map(|_| HookControlAction::DetectorUnavailableUnlock),
        );
        actions
    }

    pub fn lock_active(&self) -> bool {
        self.core.lock_active.load(Ordering::Relaxed)
    }

    pub fn take_dropped_event_count(&mut self) -> u64 {
        self.core.dropped_event_count.swap(0, Ordering::Relaxed)
    }
}

impl Drop for CaptureRuntime {
    fn drop(&mut self) {
        if self.lock_active() {
            let _ = set_hook_lock_active_for(&self.core, false);
        }
        self.core.lock_watchdog_stop.store(true, Ordering::Release);
        if let Some(thread) = self.lock_watchdog.take() {
            let _ = thread.join();
        }
        if let Some(thread_id) = self.raw_input_thread_id {
            post_thread_quit(thread_id);
        }
        if let Some(thread) = self.raw_input_thread.take() {
            let _ = thread.join();
        }
        self.raw_input_thread_id = None;
        self.raw_input_hwnd = None;
        self.raw_input_enabled = false;
        set_raw_keyboard_escape_enabled_for(&self.core, false);
        post_thread_quit(self.hook_thread_id);
        if let Some(thread) = self.hook_thread.take() {
            let _ = thread.join();
        }

        let _ = clear_active_capture_runtime(&self.core);
    }
}

fn active_capture_runtime_cell() -> &'static Mutex<Option<Weak<CaptureRuntimeCore>>> {
    ACTIVE_CAPTURE_RUNTIME.get_or_init(|| Mutex::new(None))
}

fn activate_capture_runtime(core: &Arc<CaptureRuntimeCore>) -> Result<()> {
    let mut guard = active_capture_runtime_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("capture runtime registry mutex poisoned"))?;
    if guard
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|current| !Arc::ptr_eq(&current, core))
    {
        anyhow::bail!("capture runtime already active");
    }
    *guard = Some(Arc::downgrade(core));
    Ok(())
}

fn clear_active_capture_runtime(core: &Arc<CaptureRuntimeCore>) -> Result<()> {
    let mut guard = active_capture_runtime_cell()
        .lock()
        .map_err(|_| anyhow::anyhow!("capture runtime registry mutex poisoned"))?;
    if guard
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|current| Arc::ptr_eq(&current, core))
    {
        *guard = None;
    }
    Ok(())
}

fn active_capture_runtime() -> Option<Arc<CaptureRuntimeCore>> {
    active_capture_runtime_cell()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(Weak::upgrade))
}

fn with_active_capture_runtime<T>(f: impl FnOnce(&CaptureRuntimeCore) -> T) -> Option<T> {
    active_capture_runtime().as_deref().map(f)
}

pub fn send_hook_event(event: HookCaptureEvent, source: &'static str) {
    let sender = with_active_capture_runtime(|core| {
        core.event_tx
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    })
    .flatten();
    if let Some(sender) = sender {
        match sender.try_send(event) {
            Ok(()) => {
                if let Some(notifier) = with_active_capture_runtime(|core| {
                    core.wake_notifier
                        .lock()
                        .ok()
                        .and_then(|guard| guard.as_ref().cloned())
                })
                .flatten()
                {
                    notifier(source);
                }
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                let _ = with_active_capture_runtime(|core| {
                    core.dropped_event_count.fetch_add(1, Ordering::Relaxed)
                });
            }
        }
    }
}

pub fn is_hook_lock_active() -> bool {
    with_active_capture_runtime(|core| core.lock_active.load(Ordering::Relaxed)).unwrap_or(false)
}

fn escape_detector_source(runtime: &CaptureRuntimeCore) -> Option<EscapeDetectorSource> {
    runtime
        .escape_detector_state
        .lock()
        .ok()
        .map(|state| state.source)
}

fn escape_detector_available(runtime: &CaptureRuntimeCore) -> bool {
    match escape_detector_source(runtime) {
        Some(EscapeDetectorSource::RawKeyboard) => true,
        Some(EscapeDetectorSource::KeyboardHook) => {
            !runtime.keyboard_hook_degraded.load(Ordering::Acquire)
        }
        None => false,
    }
}

fn fail_open_if_escape_detector_unavailable(runtime: &CaptureRuntimeCore) {
    if !escape_detector_available(runtime) && runtime.lock_active.load(Ordering::Acquire) {
        let _ = force_unlock_for_arc(runtime, Some(SafetyUnlockCause::DetectorUnavailable));
    }
}

fn set_raw_keyboard_escape_enabled_for(runtime: &CaptureRuntimeCore, enabled: bool) {
    let next_source = if enabled {
        EscapeDetectorSource::RawKeyboard
    } else {
        EscapeDetectorSource::KeyboardHook
    };
    let changed = match runtime.escape_detector_state.lock() {
        Ok(mut state) if state.source != next_source => {
            state.source = next_source;
            state.gesture = HookRuntimeState::default();
            true
        }
        Ok(_) => false,
        Err(_) => true,
    };
    if changed {
        if let Ok(mut observation) = runtime.last_keyboard_hook_observation.lock() {
            *observation = None;
        }
        eprintln!(
            "boundless_input_escape_detector source={}",
            next_source.label()
        );
    }

    fail_open_if_escape_detector_unavailable(runtime);
}

fn record_keyboard_hook_observation(
    runtime: &CaptureRuntimeCore,
    vk_code: u16,
    state: KeyState,
    message_time_ms: u32,
) {
    if !is_control_virtual_key(vk_code) {
        return;
    }
    if let Ok(mut observation) = runtime.last_keyboard_hook_observation.lock() {
        *observation = Some(KeyboardHookObservation {
            message_time_ms,
            state,
        });
    }
}

fn observe_raw_keyboard_hook_health(
    runtime: &CaptureRuntimeCore,
    vk_code: u16,
    state: KeyState,
    message_time_ms: u32,
) {
    if !is_control_virtual_key(vk_code) {
        return;
    }
    let hook_observed = runtime
        .last_keyboard_hook_observation
        .lock()
        .ok()
        .and_then(|observation| *observation)
        .is_some_and(|observation| {
            observation.message_time_ms == message_time_ms && observation.state == state
        });
    if !hook_observed && !runtime.keyboard_hook_degraded.swap(true, Ordering::AcqRel) {
        eprintln!("boundless_input_keyboard_hook state=degraded detector=raw_keyboard");
        warn!(
            "low-level keyboard hook missed physical Control input; Raw Input remains authoritative for emergency unlock"
        );
    }
    fail_open_if_escape_detector_unavailable(runtime);
}

fn is_control_virtual_key(vk_code: u16) -> bool {
    vk_code == VK_CONTROL_CODE || vk_code == VK_LCONTROL_CODE || vk_code == VK_RCONTROL_CODE
}

fn update_escape_state_for_key_at(
    runtime: &CaptureRuntimeCore,
    source: EscapeDetectorSource,
    vk_code: u16,
    key_state: KeyState,
    now: Instant,
    double_tap_window: Duration,
) -> bool {
    if !runtime.lock_active.load(Ordering::Relaxed) {
        return false;
    }
    let mut detector = match runtime.escape_detector_state.lock() {
        Ok(state) if state.source == source => state,
        Ok(_) => return false,
        Err(_) => return false,
    };
    let state = &mut detector.gesture;

    if !is_control_virtual_key(vk_code) {
        if matches!(key_state, KeyState::Down) {
            state.current_ctrl_tap_valid = false;
            state.last_ctrl_tap_at = None;
        }
        return false;
    }

    if state
        .left_ctrl_down_at
        .is_some_and(|down_at| now.saturating_duration_since(down_at) > double_tap_window)
        || state
            .right_ctrl_down_at
            .is_some_and(|down_at| now.saturating_duration_since(down_at) > double_tap_window)
    {
        state.left_ctrl_down_at = None;
        state.right_ctrl_down_at = None;
        state.current_ctrl_tap_valid = false;
    }

    let vk_code = if vk_code == VK_CONTROL_CODE {
        VK_LCONTROL_CODE
    } else {
        vk_code
    };
    let other_down = match vk_code {
        VK_LCONTROL_CODE => state.right_ctrl_down_at.is_some(),
        VK_RCONTROL_CODE => state.left_ctrl_down_at.is_some(),
        _ => return false,
    };
    let current_down = match vk_code {
        VK_LCONTROL_CODE => &mut state.left_ctrl_down_at,
        VK_RCONTROL_CODE => &mut state.right_ctrl_down_at,
        _ => return false,
    };

    if matches!(key_state, KeyState::Down) {
        if current_down.is_some() {
            return false;
        }
        *current_down = Some(now);
        if other_down {
            state.current_ctrl_tap_valid = false;
        }
        if !other_down {
            state.current_ctrl_tap_valid = true;
        }
        return false;
    }

    let Some(down_at) = current_down.take() else {
        return false;
    };
    if !other_down {
        if state.current_ctrl_tap_valid {
            let triggered = state.last_ctrl_tap_at.is_some_and(|previous| {
                down_at.saturating_duration_since(previous) <= double_tap_window
            });
            state.last_ctrl_tap_at = (!triggered).then_some(down_at);
            state.current_ctrl_tap_valid = false;
            return triggered;
        }
        state.current_ctrl_tap_valid = false;
    }

    false
}

fn key_semantics_for_hook_event(vk_code: u16, key_state: KeyState) -> KeySemantics {
    let num_lock_on = with_active_capture_runtime(|runtime| {
        let Ok(mut state) = runtime.keyboard_state.lock() else {
            return runtime.num_lock_state.is_on();
        };
        update_num_lock_state_for_key(&mut state, &runtime.num_lock_state, vk_code, key_state)
    })
    .unwrap_or(false);

    KeySemantics::Windows {
        virtual_key: vk_code,
        num_lock_on,
    }
}

fn should_observe_external_injected_num_lock(flags: u32, extra_info: usize, vk_code: u16) -> bool {
    (flags & LLKHF_INJECTED_MASK) != 0
        && extra_info != BOUNDLESS_INJECTED_INPUT_MARKER
        && vk_code == VK_NUMLOCK_CODE
}

fn update_num_lock_state_for_key(
    state: &mut KeyboardRuntimeState,
    num_lock_state: &WindowsNumLockState,
    vk_code: u16,
    key_state: KeyState,
) -> bool {
    if vk_code == VK_NUMLOCK_CODE {
        match key_state {
            KeyState::Down if !state.num_lock_down => {
                state.num_lock_down = true;
                return num_lock_state.toggle();
            }
            KeyState::Down => {}
            KeyState::Up => state.num_lock_down = false,
        }
    }
    num_lock_state.is_on()
}

fn num_lock_state_from_message_lane() -> bool {
    let state = unsafe { GetKeyState(i32::from(VK_NUMLOCK_CODE)) };
    (state as u16 & 0x0001) != 0
}

fn seed_num_lock_state_from_message_lane(runtime: &CaptureRuntimeCore) {
    runtime
        .num_lock_state
        .set(num_lock_state_from_message_lane());
    if let Ok(mut state) = runtime.keyboard_state.lock() {
        // Preserve the edge detector when Boundless starts while Num Lock is
        // already held; the next autorepeat is not a fresh toggle.
        state.num_lock_down = is_virtual_key_down(VK_NUMLOCK_CODE);
    }
}

/// Detects the emergency gesture and releases the local hook lock before
/// publishing any reconciliation action. This remains safe when the bounded
/// hook event queue is full or the daemon/RPC path is stalled.
pub fn try_escape_unlock_for_key(vk_code: u16, key_state: KeyState) -> bool {
    let Some(runtime) = active_capture_runtime() else {
        return false;
    };
    try_escape_unlock_for_key_from_source_at(
        &runtime,
        EscapeDetectorSource::KeyboardHook,
        vk_code,
        key_state,
        Instant::now(),
        escape_double_ctrl_window(unsafe { GetDoubleClickTime() }),
    )
}

fn try_escape_unlock_for_key_from_source_at(
    runtime: &CaptureRuntimeCore,
    source: EscapeDetectorSource,
    vk_code: u16,
    key_state: KeyState,
    now: Instant,
    double_tap_window: Duration,
) -> bool {
    if !update_escape_state_for_key_at(runtime, source, vk_code, key_state, now, double_tap_window)
    {
        return false;
    }
    force_unlock_for_arc(runtime, Some(SafetyUnlockCause::Escape))
}

/// Releases the active hook lock synchronously without publishing a daemon
/// reconciliation action. Tray shutdown invokes this before cancellation or
/// cleanup RPCs so local input cannot remain captive behind a hung broker.
pub fn release_active_hook_lock() -> bool {
    active_capture_runtime().is_some_and(|runtime| force_unlock_for_arc(&runtime, None))
}

fn escape_double_ctrl_window(system_double_click_ms: u32) -> Duration {
    Duration::from_millis(u64::from(system_double_click_ms).clamp(
        ESCAPE_DOUBLE_CTRL_MIN_WINDOW_MS,
        ESCAPE_DOUBLE_CTRL_MAX_WINDOW_MS,
    ))
}

fn set_hook_lock_active_for(core: &Arc<CaptureRuntimeCore>, active: bool) -> Result<bool> {
    set_hook_lock_active_for_arc(core.as_ref(), active)
}

fn set_hook_lock_active_for_arc(core: &CaptureRuntimeCore, active: bool) -> Result<bool> {
    let mut state = match core.escape_detector_state.lock() {
        Ok(state) => state,
        Err(error) => {
            drop(error);
            let _ = force_unlock_for_arc(core, Some(SafetyUnlockCause::DetectorUnavailable));
            return Ok(false);
        }
    };
    let detector_available = state.source == EscapeDetectorSource::RawKeyboard
        || !core.keyboard_hook_degraded.load(Ordering::Acquire);
    if active && !detector_available {
        core.lock_active.store(false, Ordering::Release);
        state.gesture = HookRuntimeState::default();
        drop(state);
        if let Ok(mut lease) = core.lock_lease.lock() {
            lease.last_renewed_at = None;
        }
        return Ok(false);
    }

    if !active {
        // Unlock first: local input must fail open even if later cleanup state
        // is poisoned or otherwise unavailable.
        core.lock_active.store(false, Ordering::Release);
        state.gesture = HookRuntimeState::default();
    }
    drop(state);

    let mut lease = core
        .lock_lease
        .lock()
        .map_err(|_| anyhow::anyhow!("hook lock lease mutex poisoned"))?;
    lease.last_renewed_at = (active && lease.timeout.is_some()).then(Instant::now);
    drop(lease);
    if active {
        // Publish capture only after control-state cleanup and the safety
        // lease are initialized.
        core.lock_active.store(true, Ordering::Release);
    }
    Ok(active)
}

fn renew_hook_lock_lease_for(core: &CaptureRuntimeCore, now: Instant) -> bool {
    if !core.lock_active.load(Ordering::Acquire) {
        return false;
    }
    let Ok(mut lease) = core.lock_lease.lock() else {
        let _ = force_unlock_for_arc(core, Some(SafetyUnlockCause::LeaseExpired));
        return false;
    };
    if lease.timeout.is_none() {
        return false;
    }
    lease.last_renewed_at = Some(now);
    true
}

fn force_unlock_for_arc(core: &CaptureRuntimeCore, cause: Option<SafetyUnlockCause>) -> bool {
    force_unlock_for_arc_with_hook(core, cause, || {})
}

fn force_unlock_for_arc_with_hook(
    core: &CaptureRuntimeCore,
    cause: Option<SafetyUnlockCause>,
    after_lock_release: impl FnOnce(),
) -> bool {
    // Publish the invalidation before touching the lock. The in-progress
    // counter prevents a relock from sampling the new generation while this
    // unlock is still cleaning up its local state.
    core.safety_unlock_in_progress
        .fetch_add(1, Ordering::SeqCst);
    core.safety_unlock_generation.fetch_add(1, Ordering::SeqCst);
    let was_active = core.lock_active.swap(false, Ordering::SeqCst);
    after_lock_release();

    if let Ok(mut state) = core.escape_detector_state.lock() {
        state.gesture = HookRuntimeState::default();
    }
    if let Ok(mut state) = core.keyboard_state.lock() {
        state.num_lock_down = false;
    }
    if let Ok(mut lease) = core.lock_lease.lock() {
        lease.last_renewed_at = None;
    }

    match (cause, was_active) {
        (Some(SafetyUnlockCause::Escape), true) => {
            core.escape_unlock_pending.fetch_add(1, Ordering::AcqRel);
            let detector = escape_detector_source(core)
                .map(EscapeDetectorSource::label)
                .unwrap_or("unavailable");
            eprintln!("boundless_input_safety_unlock cause=escape detector={detector}");
        }
        (Some(SafetyUnlockCause::LeaseExpired), true) => {
            core.lease_expired_unlock_pending
                .fetch_add(1, Ordering::AcqRel);
            eprintln!("boundless_input_safety_unlock cause=lease_expired");
        }
        (Some(SafetyUnlockCause::DetectorUnavailable), true) => {
            core.detector_unavailable_unlock_pending
                .fetch_add(1, Ordering::AcqRel);
            eprintln!("boundless_input_safety_unlock cause=detector_unavailable");
        }
        _ => {}
    }
    if cause.is_some()
        && was_active
        && let Ok(notifier) = core.wake_notifier.lock()
        && let Some(notifier) = notifier.as_ref()
    {
        notifier("input_safety_unlock");
    }
    core.safety_unlock_in_progress
        .fetch_sub(1, Ordering::SeqCst);
    was_active
}

fn set_hook_lock_active_if_generation_for(
    core: &Arc<CaptureRuntimeCore>,
    active: bool,
    expected_generation: u64,
    after_store: impl FnOnce(),
) -> Result<bool> {
    if !active {
        return set_hook_lock_active_for(core, false);
    }
    if core.safety_unlock_generation.load(Ordering::SeqCst) != expected_generation
        || core.safety_unlock_in_progress.load(Ordering::SeqCst) != 0
    {
        return Ok(false);
    }

    if !set_hook_lock_active_for(core, true)? {
        return Ok(false);
    }
    after_store();
    if core.safety_unlock_generation.load(Ordering::SeqCst) != expected_generation
        || core.safety_unlock_in_progress.load(Ordering::SeqCst) != 0
    {
        let _ = set_hook_lock_active_for(core, false);
        return Ok(false);
    }
    Ok(true)
}

fn expire_hook_lock_lease_if_needed(core: &CaptureRuntimeCore, now: Instant) -> bool {
    if !core.lock_active.load(Ordering::Acquire) {
        return false;
    }
    let expired = match core.lock_lease.lock() {
        Ok(lease) => match (lease.timeout, lease.last_renewed_at) {
            (Some(timeout), Some(last_renewed_at)) => {
                now.saturating_duration_since(last_renewed_at) >= timeout
            }
            (Some(_), None) => true,
            (None, _) => false,
        },
        Err(_) => true,
    };
    expired && force_unlock_for_arc(core, Some(SafetyUnlockCause::LeaseExpired))
}

fn run_lock_watchdog(core: Arc<CaptureRuntimeCore>) {
    const WATCHDOG_POLL: Duration = Duration::from_millis(20);
    while !core.lock_watchdog_stop.load(Ordering::Acquire) {
        let _ = expire_hook_lock_lease_if_needed(&core, Instant::now());
        thread::sleep(WATCHDOG_POLL);
    }
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

pub fn spawn_raw_input_thread() -> Result<(u32, isize, JoinHandle<()>)> {
    let (startup_tx, startup_rx) = mpsc::channel::<Result<(u32, isize)>>();
    let thread = thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hwnd = match create_raw_input_window() {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = startup_tx.send(Err(error));
                return;
            }
        };

        if let Err(error) = register_raw_input_devices(hwnd) {
            let _ = startup_tx.send(Err(error));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }

        let _ = startup_tx.send(Ok((thread_id, hwnd as isize)));
        unsafe {
            if let Err(error) = run_raw_input_message_loop() {
                warn!(error = ?error, "raw input message loop exited with error");
            }
            let _ = DestroyWindow(hwnd);
        }
    });

    let (thread_id, hwnd) = match startup_rx.recv() {
        Ok(Ok(startup)) => startup,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = thread.join();
            return Err(anyhow::anyhow!("raw input startup channel closed"));
        }
    };

    Ok((thread_id, hwnd, thread))
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

fn register_raw_input_devices(hwnd: HWND) -> Result<()> {
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
            usUsage: RAW_INPUT_USAGE_MOUSE,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
            usUsage: RAW_INPUT_USAGE_KEYBOARD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
    ];
    let ok = unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("RegisterRawInputDevices mouse and keyboard");
    }
    Ok(())
}

fn raw_input_target_owned(devices: &[RAWINPUTDEVICE], hwnd: HWND, usage: u16) -> bool {
    devices.iter().any(|device| {
        device.usUsagePage == RAW_INPUT_USAGE_PAGE_GENERIC
            && device.usUsage == usage
            && device.hwndTarget == hwnd
    })
}

fn raw_input_targets_owned(devices: &[RAWINPUTDEVICE], hwnd: HWND) -> bool {
    raw_input_target_owned(devices, hwnd, RAW_INPUT_USAGE_MOUSE)
        && raw_input_target_owned(devices, hwnd, RAW_INPUT_USAGE_KEYBOARD)
}

fn registered_raw_input_devices() -> Result<Vec<RAWINPUTDEVICE>> {
    let device_size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
    let mut count = 0u32;
    let result = unsafe {
        GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count as *mut u32, device_size)
    };
    if result == u32::MAX {
        return Err(std::io::Error::last_os_error())
            .context("GetRegisteredRawInputDevices query count");
    }
    if count == 0 {
        return Ok(Vec::new());
    }

    let empty = RAWINPUTDEVICE {
        usUsagePage: 0,
        usUsage: 0,
        dwFlags: 0,
        hwndTarget: std::ptr::null_mut(),
    };
    let mut devices = vec![empty; count as usize];
    let result = unsafe {
        GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count as *mut u32, device_size)
    };
    if result == u32::MAX {
        return Err(std::io::Error::last_os_error())
            .context("GetRegisteredRawInputDevices read registrations");
    }
    devices.truncate((result as usize).min(devices.len()));
    Ok(devices)
}

fn raw_input_registration_owned(hwnd: HWND) -> Result<bool> {
    registered_raw_input_devices().map(|devices| raw_input_targets_owned(&devices, hwnd))
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
            match process_raw_input_message(msg.lParam, msg.time) {
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

fn process_raw_input_message(lparam: LPARAM, message_time_ms: u32) -> Result<()> {
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
    if raw.header.dwType == RIM_TYPEKEYBOARD {
        let keyboard = unsafe { raw.data.keyboard };
        if let Some((vk_code, state)) = raw_keyboard_key(&keyboard) {
            if current_input_message_is_injected() {
                return Ok(());
            }
            let _ = with_active_capture_runtime(|runtime| {
                observe_raw_keyboard_hook_health(runtime, vk_code, state, message_time_ms);
                let _ = try_escape_unlock_for_key_from_source_at(
                    runtime,
                    EscapeDetectorSource::RawKeyboard,
                    vk_code,
                    state,
                    Instant::now(),
                    escape_double_ctrl_window(unsafe { GetDoubleClickTime() }),
                );
            });
        }
        return Ok(());
    }
    if raw.header.dwType != RIM_TYPEMOUSE {
        return Ok(());
    }

    let mouse = unsafe { raw.data.mouse };
    if let Some((dx, dy)) = raw_mouse_relative_delta(&mouse) {
        send_hook_event(HookCaptureEvent::MouseDelta { dx, dy }, "raw_input");
    }
    let source = raw_wheel_source_for_device(raw.header.hDevice);
    for event in raw_mouse_wheel_events(&mouse) {
        if let InputEvent::MouseWheel { delta_x, delta_y } = event {
            send_hook_event(
                HookCaptureEvent::Wheel(CapturedWheelEvent {
                    delta_x,
                    delta_y,
                    source,
                    message_time_ms,
                    observed_at: Instant::now(),
                }),
                "raw_input",
            );
        }
    }

    Ok(())
}

fn current_input_message_is_injected() -> bool {
    let mut source = INPUT_MESSAGE_SOURCE::default();
    let ok = unsafe { GetCurrentInputMessageSource(&mut source as *mut INPUT_MESSAGE_SOURCE) };
    ok != 0 && source.originId == IMO_INJECTED
}

fn raw_keyboard_key(keyboard: &RAWKEYBOARD) -> Option<(u16, KeyState)> {
    if keyboard.VKey == u8::MAX as u16 {
        return None;
    }
    let vk_code = match keyboard.VKey {
        VK_CONTROL_CODE if (u32::from(keyboard.Flags) & RI_KEY_E0) != 0 => VK_RCONTROL_CODE,
        VK_CONTROL_CODE => VK_LCONTROL_CODE,
        code => code,
    };
    let state = if (u32::from(keyboard.Flags) & RI_KEY_BREAK) != 0 {
        KeyState::Up
    } else {
        KeyState::Down
    };
    Some((vk_code, state))
}

fn hook_control_virtual_key(vk_code: u16, flags: u32) -> u16 {
    match vk_code {
        VK_CONTROL_CODE if (flags & LLKHF_EXTENDED_MASK) != 0 => VK_RCONTROL_CODE,
        VK_CONTROL_CODE => VK_LCONTROL_CODE,
        code => code,
    }
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

/// Extracts signed high-resolution wheel deltas from Raw Input. Values are
/// intentionally not normalized to WHEEL_DELTA (120): precision touchpads and
/// high-resolution wheels may emit smaller increments that destination apps
/// need to accumulate.
pub fn raw_mouse_wheel_events(mouse: &RAWMOUSE) -> Vec<InputEvent> {
    let buttons = unsafe { mouse.Anonymous.Anonymous };
    let flags = u32::from(buttons.usButtonFlags);
    let delta = i16::from_ne_bytes(buttons.usButtonData.to_ne_bytes()) as i32;
    if delta == 0 {
        return Vec::new();
    }

    let mut events = Vec::with_capacity(2);
    if (flags & RI_MOUSE_WHEEL) != 0 {
        events.push(InputEvent::MouseWheel {
            delta_x: 0,
            delta_y: delta,
        });
    }
    if (flags & RI_MOUSE_HWHEEL) != 0 {
        events.push(InputEvent::MouseWheel {
            delta_x: delta,
            delta_y: 0,
        });
    }
    events
}

fn raw_wheel_source_for_device(device: *mut core::ffi::c_void) -> WheelCaptureSource {
    if device.is_null() {
        WheelCaptureSource::RawSystem
    } else {
        WheelCaptureSource::RawDevice
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let mut lock_active = false;
    if code == HC_ACTION as i32 {
        let keyboard = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let state = match wparam as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(KeyState::Down),
            WM_KEYUP | WM_SYSKEYUP => Some(KeyState::Up),
            _ => None,
        };

        if let Some(state) = state {
            let vk_code = keyboard.vkCode as u16;
            if should_observe_external_injected_num_lock(
                keyboard.flags,
                keyboard.dwExtraInfo,
                vk_code,
            ) {
                // External OSK/remapper/MWB input is not relayed, preserving
                // the existing loop guard, but its Num Lock transition still
                // updates the process-local destination authority.
                let _ = key_semantics_for_hook_event(vk_code, state);
            } else if (keyboard.flags & LLKHF_INJECTED_MASK) == 0 {
                lock_active = is_hook_lock_active();
                let control_vk = hook_control_virtual_key(vk_code, keyboard.flags);
                let _ = with_active_capture_runtime(|runtime| {
                    record_keyboard_hook_observation(runtime, control_vk, state, keyboard.time);
                });
                if lock_active && try_escape_unlock_for_key(control_vk, state) {
                    lock_active = false;
                }
                let mut scan_code = keyboard.scanCode as u16;
                if (keyboard.flags & LLKHF_EXTENDED_MASK) != 0 {
                    scan_code |= 0xE000;
                }
                send_hook_event(
                    HookCaptureEvent::Input(InputEvent::Key {
                        scan_code,
                        state,
                        semantics: key_semantics_for_hook_event(vk_code, state),
                    }),
                    "keyboard_hook",
                );
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
                    HookCaptureEvent::Wheel(CapturedWheelEvent {
                        delta_x: 0,
                        delta_y: crate::input::signed_high_word(mouse.mouseData),
                        source: WheelCaptureSource::Hook,
                        message_time_ms: mouse.time,
                        observed_at: Instant::now(),
                    }),
                    "mouse_hook",
                ),
                WM_MOUSEHWHEEL => send_hook_event(
                    HookCaptureEvent::Wheel(CapturedWheelEvent {
                        delta_x: crate::input::signed_high_word(mouse.mouseData),
                        delta_y: 0,
                        source: WheelCaptureSource::Hook,
                        message_time_ms: mouse.time,
                        observed_at: Instant::now(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, OnceLock};
    use windows_sys::Win32::UI::Input::{RAWMOUSE_0, RAWMOUSE_0_0};

    static REGISTRY_TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

    fn registry_test_guard() -> &'static Mutex<()> {
        REGISTRY_TEST_GUARD.get_or_init(|| Mutex::new(()))
    }

    fn test_runtime_core() -> Arc<CaptureRuntimeCore> {
        Arc::new(CaptureRuntimeCore {
            event_tx: Mutex::new(None),
            wake_notifier: Mutex::new(None),
            escape_detector_state: Mutex::new(EscapeDetectorState::new(
                EscapeDetectorSource::KeyboardHook,
            )),
            keyboard_state: Mutex::new(KeyboardRuntimeState::default()),
            num_lock_state: WindowsNumLockState::new(false),
            lock_lease: Mutex::new(HookLockLease::default()),
            lock_active: AtomicBool::new(false),
            keyboard_hook_degraded: AtomicBool::new(false),
            last_keyboard_hook_observation: Mutex::new(None),
            escape_unlock_pending: AtomicU64::new(0),
            lease_expired_unlock_pending: AtomicU64::new(0),
            detector_unavailable_unlock_pending: AtomicU64::new(0),
            safety_unlock_generation: AtomicU64::new(0),
            safety_unlock_in_progress: AtomicU64::new(0),
            lock_watchdog_stop: AtomicBool::new(false),
            dropped_event_count: AtomicU64::new(0),
        })
    }

    fn reset_active_runtime_for_test() {
        if let Ok(mut guard) = active_capture_runtime_cell().lock() {
            *guard = None;
        }
    }

    fn raw_mouse_with_wheel(button_flags: u16, delta: i16) -> RAWMOUSE {
        RAWMOUSE {
            Anonymous: RAWMOUSE_0 {
                Anonymous: RAWMOUSE_0_0 {
                    usButtonFlags: button_flags,
                    usButtonData: u16::from_ne_bytes(delta.to_ne_bytes()),
                },
            },
            ..Default::default()
        }
    }

    #[test]
    fn active_runtime_registry_rejects_second_live_runtime() {
        let _guard = registry_test_guard().lock().expect("test guard");
        reset_active_runtime_for_test();

        let first = test_runtime_core();
        activate_capture_runtime(&first).expect("first runtime should activate");

        let second = test_runtime_core();
        let err = activate_capture_runtime(&second).expect_err("second runtime must be rejected");
        assert!(err.to_string().contains("already active"));

        clear_active_capture_runtime(&first).expect("cleanup");
    }

    #[test]
    fn active_runtime_registry_allows_reactivation_after_clear() {
        let _guard = registry_test_guard().lock().expect("test guard");
        reset_active_runtime_for_test();

        let first = test_runtime_core();
        activate_capture_runtime(&first).expect("first runtime should activate");
        clear_active_capture_runtime(&first).expect("clear first runtime");

        let second = test_runtime_core();
        activate_capture_runtime(&second).expect("second runtime should activate");
        clear_active_capture_runtime(&second).expect("clear second runtime");
    }

    #[test]
    fn raw_input_preserves_signed_high_resolution_vertical_wheel_delta() {
        let mouse = raw_mouse_with_wheel(RI_MOUSE_WHEEL as u16, 30);
        assert_eq!(
            raw_mouse_wheel_events(&mouse),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: 30,
            }]
        );

        let mouse = raw_mouse_with_wheel(RI_MOUSE_WHEEL as u16, -45);
        assert_eq!(
            raw_mouse_wheel_events(&mouse),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: -45,
            }]
        );
    }

    #[test]
    fn raw_input_extracts_horizontal_wheel_without_normalizing() {
        let mouse = raw_mouse_with_wheel(RI_MOUSE_HWHEEL as u16, -17);
        assert_eq!(
            raw_mouse_wheel_events(&mouse),
            vec![InputEvent::MouseWheel {
                delta_x: -17,
                delta_y: 0,
            }]
        );
        assert!(raw_mouse_wheel_events(&RAWMOUSE::default()).is_empty());
    }

    #[test]
    fn raw_input_ownership_requires_our_mouse_and_keyboard_targets() {
        let our_hwnd = 1usize as HWND;
        let other_hwnd = 2usize as HWND;
        let registrations = [
            RAWINPUTDEVICE {
                usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
                usUsage: RAW_INPUT_USAGE_MOUSE,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: other_hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
                usUsage: 0x06,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: our_hwnd,
            },
        ];
        assert!(!raw_input_targets_owned(&registrations, our_hwnd));

        let registrations = [
            RAWINPUTDEVICE {
                usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
                usUsage: RAW_INPUT_USAGE_MOUSE,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: our_hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
                usUsage: RAW_INPUT_USAGE_KEYBOARD,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: our_hwnd,
            },
        ];
        assert!(raw_input_targets_owned(&registrations, our_hwnd));
    }

    #[test]
    fn raw_keyboard_normalizes_control_side_and_key_state() {
        let mut keyboard = RAWKEYBOARD {
            VKey: VK_CONTROL_CODE,
            ..Default::default()
        };
        assert_eq!(
            raw_keyboard_key(&keyboard),
            Some((VK_LCONTROL_CODE, KeyState::Down))
        );

        keyboard.Flags = RI_KEY_E0 as u16;
        assert_eq!(
            raw_keyboard_key(&keyboard),
            Some((VK_RCONTROL_CODE, KeyState::Down))
        );

        keyboard.Flags = (RI_KEY_E0 | RI_KEY_BREAK) as u16;
        assert_eq!(
            raw_keyboard_key(&keyboard),
            Some((VK_RCONTROL_CODE, KeyState::Up))
        );

        keyboard.VKey = 0x41;
        keyboard.Flags = 0;
        assert_eq!(raw_keyboard_key(&keyboard), Some((0x41, KeyState::Down)));
        keyboard.VKey = u8::MAX as u16;
        assert_eq!(raw_keyboard_key(&keyboard), None);

        assert_eq!(
            hook_control_virtual_key(VK_CONTROL_CODE, 0),
            VK_LCONTROL_CODE
        );
        assert_eq!(
            hook_control_virtual_key(VK_CONTROL_CODE, LLKHF_EXTENDED_MASK),
            VK_RCONTROL_CODE
        );
    }

    #[test]
    fn raw_keyboard_is_authoritative_and_hook_copies_do_not_double_count() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("lock");
        set_raw_keyboard_escape_enabled_for(&core, true);
        let start = Instant::now();
        let window = Duration::from_millis(800);

        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + Duration::from_millis(200),
            window,
        ));
        assert!(try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(220),
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + Duration::from_millis(200),
            window,
        ));

        assert!(!core.lock_active.load(Ordering::Acquire));
        assert_eq!(core.escape_unlock_pending.load(Ordering::Acquire), 1);
    }

    #[test]
    fn control_chords_and_altgr_do_not_prime_emergency_unlock() {
        for chord_key in [0x43, 0xA5] {
            let core = test_runtime_core();
            set_raw_keyboard_escape_enabled_for(&core, true);
            set_hook_lock_active_for(&core, true).expect("lock");
            let start = Instant::now();
            let window = Duration::from_millis(800);

            for (vk_code, state, offset_ms) in [
                // Ctrl+C / AltGr chord.
                (VK_LCONTROL_CODE, KeyState::Down, 0),
                (chord_key, KeyState::Down, 10),
                (chord_key, KeyState::Up, 20),
                (VK_LCONTROL_CODE, KeyState::Up, 30),
                // A second chord inside the gesture window is still not a tap.
                (VK_LCONTROL_CODE, KeyState::Down, 100),
                (chord_key, KeyState::Down, 110),
                (chord_key, KeyState::Up, 120),
                (VK_LCONTROL_CODE, KeyState::Up, 130),
                // A chord after a bare tap invalidates that completed tap too.
                (VK_LCONTROL_CODE, KeyState::Down, 200),
                (VK_LCONTROL_CODE, KeyState::Up, 220),
                (VK_LCONTROL_CODE, KeyState::Down, 300),
                (chord_key, KeyState::Down, 310),
                (chord_key, KeyState::Up, 320),
                (VK_LCONTROL_CODE, KeyState::Up, 330),
                // Only two final bare taps prime the gesture.
                (VK_LCONTROL_CODE, KeyState::Down, 400),
                (VK_LCONTROL_CODE, KeyState::Up, 420),
                (VK_LCONTROL_CODE, KeyState::Down, 500),
            ] {
                assert!(!try_escape_unlock_for_key_from_source_at(
                    &core,
                    EscapeDetectorSource::RawKeyboard,
                    vk_code,
                    state,
                    start + Duration::from_millis(offset_ms),
                    window,
                ));
            }
            assert!(core.lock_active.load(Ordering::Acquire));
            assert!(try_escape_unlock_for_key_from_source_at(
                &core,
                EscapeDetectorSource::RawKeyboard,
                VK_LCONTROL_CODE,
                KeyState::Up,
                start + Duration::from_millis(520),
                window,
            ));
        }
    }

    #[test]
    fn raw_keyboard_unlocks_when_low_level_hook_is_silent() {
        for vk_code in [VK_LCONTROL_CODE, VK_RCONTROL_CODE] {
            let core = test_runtime_core();
            set_hook_lock_active_for(&core, true).expect("lock");
            set_raw_keyboard_escape_enabled_for(&core, true);
            let start = Instant::now();
            let window = Duration::from_millis(800);

            observe_raw_keyboard_hook_health(&core, vk_code, KeyState::Down, 41);
            assert!(core.keyboard_hook_degraded.load(Ordering::Acquire));
            assert!(!try_escape_unlock_for_key_from_source_at(
                &core,
                EscapeDetectorSource::RawKeyboard,
                vk_code,
                KeyState::Down,
                start,
                window,
            ));
            assert!(!try_escape_unlock_for_key_from_source_at(
                &core,
                EscapeDetectorSource::RawKeyboard,
                vk_code,
                KeyState::Up,
                start + Duration::from_millis(20),
                window,
            ));
            assert!(!try_escape_unlock_for_key_from_source_at(
                &core,
                EscapeDetectorSource::RawKeyboard,
                vk_code,
                KeyState::Down,
                start + Duration::from_millis(200),
                window,
            ));
            assert!(try_escape_unlock_for_key_from_source_at(
                &core,
                EscapeDetectorSource::RawKeyboard,
                vk_code,
                KeyState::Up,
                start + Duration::from_millis(220),
                window,
            ));
            assert_eq!(core.escape_unlock_pending.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn losing_raw_after_hook_degradation_fails_open_and_blocks_relock() {
        let core = test_runtime_core();
        set_raw_keyboard_escape_enabled_for(&core, true);
        assert!(set_hook_lock_active_for(&core, true).expect("lock with raw detector"));

        observe_raw_keyboard_hook_health(&core, VK_LCONTROL_CODE, KeyState::Down, 50);
        assert!(core.keyboard_hook_degraded.load(Ordering::Acquire));
        assert!(core.lock_active.load(Ordering::Acquire));

        set_raw_keyboard_escape_enabled_for(&core, false);
        assert!(!core.lock_active.load(Ordering::Acquire));
        assert!(core.keyboard_hook_degraded.load(Ordering::Acquire));
        assert_eq!(
            core.detector_unavailable_unlock_pending
                .load(Ordering::Acquire),
            1
        );
        assert!(
            !set_hook_lock_active_for(&core, true).expect("reject known-dead hook detector"),
            "capture must remain fail open without a reliable escape detector"
        );
        assert!(!core.lock_active.load(Ordering::Acquire));

        set_raw_keyboard_escape_enabled_for(&core, true);
        assert!(core.keyboard_hook_degraded.load(Ordering::Acquire));
        assert!(set_hook_lock_active_for(&core, true).expect("raw detector recovered"));
    }

    #[test]
    fn switching_detector_source_discards_partial_gesture() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("lock");
        set_raw_keyboard_escape_enabled_for(&core, true);
        let start = Instant::now();
        let window = Duration::from_millis(800);

        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::RawKeyboard,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));

        set_raw_keyboard_escape_enabled_for(&core, false);
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + Duration::from_millis(100),
            window,
        ));
        assert!(core.lock_active.load(Ordering::Acquire));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(120),
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + Duration::from_millis(200),
            window,
        ));
        assert!(try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(220),
            window,
        ));
    }

    #[test]
    fn source_transition_and_observation_cannot_mix_completed_taps() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("lock");
        let start = Instant::now();
        let window = Duration::from_millis(800);
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));

        let held_state = core
            .escape_detector_state
            .lock()
            .expect("hold detector transition boundary");
        let barrier = Arc::new(Barrier::new(3));
        let transition_core = Arc::clone(&core);
        let transition_barrier = Arc::clone(&barrier);
        let transition = std::thread::spawn(move || {
            transition_barrier.wait();
            set_raw_keyboard_escape_enabled_for(&transition_core, true);
        });
        let observation_core = Arc::clone(&core);
        let observation_barrier = Arc::clone(&barrier);
        let observation = std::thread::spawn(move || {
            observation_barrier.wait();
            let down_triggered = try_escape_unlock_for_key_from_source_at(
                &observation_core,
                EscapeDetectorSource::RawKeyboard,
                VK_LCONTROL_CODE,
                KeyState::Down,
                start + Duration::from_millis(200),
                window,
            );
            let up_triggered = try_escape_unlock_for_key_from_source_at(
                &observation_core,
                EscapeDetectorSource::RawKeyboard,
                VK_LCONTROL_CODE,
                KeyState::Up,
                start + Duration::from_millis(220),
                window,
            );
            down_triggered || up_triggered
        });
        barrier.wait();
        drop(held_state);

        transition.join().expect("source transition");
        assert!(!observation.join().expect("raw observation"));
        assert!(core.lock_active.load(Ordering::Acquire));
        assert_eq!(core.escape_unlock_pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn missing_control_key_up_expires_without_poisoning_next_gesture() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("lock");
        let start = Instant::now();
        let window = Duration::from_millis(800);

        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + window + Duration::from_millis(1),
            window,
        ));
        assert!(core.lock_active.load(Ordering::Acquire));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + window + Duration::from_millis(20),
            window,
        ));
        assert!(!try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + window + Duration::from_millis(200),
            window,
        ));
        assert!(try_escape_unlock_for_key_from_source_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + window + Duration::from_millis(220),
            window,
        ));
    }

    #[test]
    fn captured_num_lock_toggles_once_per_physical_press() {
        let mut state = KeyboardRuntimeState::default();
        let num_lock_state = WindowsNumLockState::new(false);

        assert!(update_num_lock_state_for_key(
            &mut state,
            &num_lock_state,
            VK_NUMLOCK_CODE,
            KeyState::Down
        ));
        assert!(
            update_num_lock_state_for_key(
                &mut state,
                &num_lock_state,
                VK_NUMLOCK_CODE,
                KeyState::Down
            ),
            "repeat keydown must not toggle again"
        );
        assert!(update_num_lock_state_for_key(
            &mut state,
            &num_lock_state,
            VK_NUMLOCK_CODE,
            KeyState::Up
        ));
        assert!(!update_num_lock_state_for_key(
            &mut state,
            &num_lock_state,
            VK_NUMLOCK_CODE,
            KeyState::Down
        ));
    }

    #[test]
    fn external_injected_num_lock_updates_authority_but_boundless_input_does_not() {
        let mut state = KeyboardRuntimeState::default();
        let num_lock_state = WindowsNumLockState::new(false);

        assert!(should_observe_external_injected_num_lock(
            LLKHF_INJECTED_MASK,
            0,
            VK_NUMLOCK_CODE,
        ));
        if should_observe_external_injected_num_lock(LLKHF_INJECTED_MASK, 0, VK_NUMLOCK_CODE) {
            update_num_lock_state_for_key(
                &mut state,
                &num_lock_state,
                VK_NUMLOCK_CODE,
                KeyState::Down,
            );
        }
        assert!(num_lock_state.is_on());

        assert!(!should_observe_external_injected_num_lock(
            LLKHF_INJECTED_MASK,
            BOUNDLESS_INJECTED_INPUT_MARKER,
            VK_NUMLOCK_CODE,
        ));
        assert!(!should_observe_external_injected_num_lock(
            LLKHF_INJECTED_MASK,
            0,
            0x61,
        ));
        assert!(!should_observe_external_injected_num_lock(
            0,
            0,
            VK_NUMLOCK_CODE,
        ));
    }

    #[test]
    fn handoff_does_not_turn_held_num_lock_repeat_into_a_fresh_toggle() {
        let core = test_runtime_core();
        core.num_lock_state.set(true);
        core.keyboard_state
            .lock()
            .expect("keyboard state")
            .num_lock_down = true;

        set_hook_lock_active_for_arc(&core, true).expect("enable hook lock");
        let mut state = core.keyboard_state.lock().expect("keyboard state");
        assert!(update_num_lock_state_for_key(
            &mut state,
            &core.num_lock_state,
            VK_NUMLOCK_CODE,
            KeyState::Down,
        ));
        assert!(state.num_lock_down);
    }

    #[test]
    fn polling_key_set_includes_keypad_separator() {
        const VK_SEPARATOR_CODE: u16 = 0x6C;
        assert!(captured_key_virtual_keys().contains(&VK_SEPARATOR_CODE));
    }

    #[test]
    fn double_control_unlocks_before_bounded_event_delivery() {
        let _guard = registry_test_guard().lock().expect("test guard");
        reset_active_runtime_for_test();

        let core = test_runtime_core();
        let (event_tx, _event_rx) = mpsc::sync_channel(1);
        event_tx
            .send(HookCaptureEvent::MouseDelta { dx: 1, dy: 0 })
            .expect("fill event queue");
        *core.event_tx.lock().expect("event sender") = Some(event_tx);
        set_hook_lock_active_for(&core, true).expect("lock");
        activate_capture_runtime(&core).expect("activate");
        let start = Instant::now();
        let window = Duration::from_millis(800);

        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + Duration::from_millis(200),
            window,
        ));
        assert!(update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(220),
            window,
        ));
        assert!(force_unlock_for_arc(&core, Some(SafetyUnlockCause::Escape)));
        assert!(!core.lock_active.load(Ordering::Acquire));
        assert_eq!(core.escape_unlock_pending.load(Ordering::Acquire), 1);

        clear_active_capture_runtime(&core).expect("cleanup");
    }

    #[test]
    fn control_taps_outside_window_do_not_unlock() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("lock");
        let start = Instant::now();
        let window = Duration::from_millis(800);

        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_RCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_RCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(20),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_RCONTROL_CODE,
            KeyState::Down,
            start + window + Duration::from_millis(1),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_RCONTROL_CODE,
            KeyState::Up,
            start + window + Duration::from_millis(20),
            window,
        ));
        assert!(core.lock_active.load(Ordering::Acquire));
    }

    #[test]
    fn escape_window_honors_system_setting_with_safe_clamps() {
        assert_eq!(escape_double_ctrl_window(400), Duration::from_millis(800));
        assert_eq!(escape_double_ctrl_window(900), Duration::from_millis(900));
        assert_eq!(
            escape_double_ctrl_window(5_000),
            Duration::from_millis(1_200)
        );
    }

    #[test]
    fn escape_window_boundary_is_inclusive_but_not_unbounded() {
        let core = test_runtime_core();
        let start = Instant::now();
        let window = Duration::from_millis(800);
        set_hook_lock_active_for(&core, true).expect("lock");
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(10),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + window,
            window,
        ));
        assert!(update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + window + Duration::from_millis(10),
            window,
        ));

        set_hook_lock_active_for(&core, false).expect("unlock");
        set_hook_lock_active_for(&core, true).expect("relock");
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start,
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + Duration::from_millis(10),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Down,
            start + window + Duration::from_millis(1),
            window,
        ));
        assert!(!update_escape_state_for_key_at(
            &core,
            EscapeDetectorSource::KeyboardHook,
            VK_LCONTROL_CODE,
            KeyState::Up,
            start + window + Duration::from_millis(11),
            window,
        ));
    }

    #[test]
    fn expired_lock_lease_fails_open_and_records_one_action() {
        let core = test_runtime_core();
        {
            let mut lease = core.lock_lease.lock().expect("lease");
            lease.timeout = Some(Duration::from_secs(2));
        }
        let start = Instant::now();
        set_hook_lock_active_for_arc(&core, true).expect("lock");
        {
            let mut lease = core.lock_lease.lock().expect("lease");
            lease.last_renewed_at = Some(start);
        }

        assert!(!expire_hook_lock_lease_if_needed(
            &core,
            start + Duration::from_millis(1_999)
        ));
        assert!(core.lock_active.load(Ordering::Acquire));
        assert!(expire_hook_lock_lease_if_needed(
            &core,
            start + Duration::from_secs(2)
        ));
        assert!(!core.lock_active.load(Ordering::Acquire));
        assert_eq!(core.lease_expired_unlock_pending.load(Ordering::Acquire), 1);
        assert!(!expire_hook_lock_lease_if_needed(
            &core,
            start + Duration::from_secs(3)
        ));
        assert_eq!(
            core.lease_expired_unlock_pending.load(Ordering::Acquire),
            1,
            "one lease transition must produce one bounded action"
        );
    }

    #[test]
    fn relock_generation_rejects_safety_unlock_before_store() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("initial lock");
        let expected_generation = core.safety_unlock_generation.load(Ordering::SeqCst);
        assert!(force_unlock_for_arc(&core, Some(SafetyUnlockCause::Escape)));

        assert!(
            !set_hook_lock_active_if_generation_for(&core, true, expected_generation, || {})
                .expect("guarded relock"),
            "a safety generation change before the store must inhibit relock"
        );
        assert!(!core.lock_active.load(Ordering::Acquire));
    }

    #[test]
    fn guarded_relock_propagates_detector_policy_refusal() {
        let core = test_runtime_core();
        core.keyboard_hook_degraded.store(true, Ordering::Release);
        let expected_generation = core.safety_unlock_generation.load(Ordering::SeqCst);
        let after_store_called = AtomicBool::new(false);

        assert!(
            !set_hook_lock_active_if_generation_for(&core, true, expected_generation, || {
                after_store_called.store(true, Ordering::Release);
            })
            .expect("guarded relock"),
            "an unavailable emergency-unlock detector must refuse the guarded relock"
        );
        assert!(!core.lock_active.load(Ordering::Acquire));
        assert!(
            !after_store_called.load(Ordering::Acquire),
            "the post-store hook must not run when detector policy rejected the store"
        );
    }

    #[test]
    fn relock_generation_post_store_guard_closes_final_race() {
        let core = test_runtime_core();
        let expected_generation = core.safety_unlock_generation.load(Ordering::SeqCst);

        assert!(
            !set_hook_lock_active_if_generation_for(&core, true, expected_generation, || {
                assert!(force_unlock_for_arc(&core, Some(SafetyUnlockCause::Escape)));
            })
            .expect("guarded relock"),
            "an unlock racing after the store must win the post-store guard"
        );
        assert!(!core.lock_active.load(Ordering::Acquire));
    }

    #[test]
    fn relock_is_rejected_while_unlock_cleanup_is_in_progress() {
        let core = test_runtime_core();
        set_hook_lock_active_for(&core, true).expect("initial lock");
        let expected_generation = core.safety_unlock_generation.load(Ordering::SeqCst);

        assert!(force_unlock_for_arc_with_hook(
            &core,
            Some(SafetyUnlockCause::Escape),
            || {
                assert!(
                    !set_hook_lock_active_if_generation_for(
                        &core,
                        true,
                        expected_generation,
                        || {}
                    )
                    .expect("guarded relock"),
                    "a relock racing between lock release and unlock cleanup must fail open"
                );
                assert!(!core.lock_active.load(Ordering::Acquire));
            }
        ));
        assert_eq!(core.safety_unlock_in_progress.load(Ordering::SeqCst), 0);
        assert!(!core.lock_active.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_unlock_inhibits_pending_relock_even_when_already_unlocked() {
        let core = test_runtime_core();
        let expected_generation = core.safety_unlock_generation.load(Ordering::SeqCst);
        assert!(!force_unlock_for_arc(&core, None));

        assert!(
            !set_hook_lock_active_if_generation_for(&core, true, expected_generation, || {})
                .expect("guarded relock"),
            "shutdown must invalidate a pending relock even if the hook was momentarily unlocked"
        );
        assert!(!core.lock_active.load(Ordering::Acquire));
    }
}
