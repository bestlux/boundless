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
            GetAsyncKeyState, GetKeyState, GetKeyboardState, INPUT, INPUT_0, INPUT_KEYBOARD,
            INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MapVirtualKeyW, SendInput,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, MSG, PM_NOREMOVE, PeekMessageW, SM_CXVIRTUALSCREEN,
            SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

#[cfg(windows)]
const XBUTTON1: u32 = 0x0001;

#[cfg(windows)]
const XBUTTON2: u32 = 0x0002;

#[cfg(windows)]
pub const VK_NUMLOCK_CODE: u16 = 0x90;

/// `dwExtraInfo` marker applied to every Boundless `SendInput` record. The
/// low-level hook uses it to distinguish our own injected Num Lock records
/// from OSK/remapper input that must still update local toggle authority.
#[cfg(windows)]
pub(crate) const BOUNDLESS_INJECTED_INPUT_MARKER: usize = 0x424E_4453;

#[derive(Debug)]
struct WindowsNumLockAuthority {
    on: bool,
    #[cfg(windows)]
    boundless_key_down: bool,
}

/// Process-local Num Lock authority shared by the interactive capture and
/// injection lanes. The hook message thread seeds and updates physical state;
/// successful Boundless injection commits synthetic toggle changes.
#[derive(Debug, Clone)]
pub struct WindowsNumLockState {
    authority: Arc<Mutex<WindowsNumLockAuthority>>,
}

impl WindowsNumLockState {
    pub fn new(on: bool) -> Self {
        Self {
            authority: Arc::new(Mutex::new(WindowsNumLockAuthority {
                on,
                #[cfg(windows)]
                boundless_key_down: false,
            })),
        }
    }

    pub fn is_on(&self) -> bool {
        self.lock().on
    }

    pub fn set(&self, on: bool) {
        self.lock().on = on;
    }

    pub fn toggle(&self) -> bool {
        let mut authority = self.lock();
        authority.on = !authority.on;
        authority.on
    }

    fn lock(&self) -> MutexGuard<'_, WindowsNumLockAuthority> {
        self.authority
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct PreparedNumLockAuthority {
    on: bool,
    boundless_key_down: bool,
}

#[cfg(windows)]
#[derive(Default)]
struct PreparedInputRecords {
    records: Vec<INPUT>,
    authority_after_record: Vec<PreparedNumLockAuthority>,
    committed_events_after_record: Vec<usize>,
    event_record_ranges: Vec<(usize, usize)>,
    committed_events_before_first_record: usize,
    committed_events: usize,
}

#[cfg(windows)]
impl PreparedInputRecords {
    fn push(&mut self, record: INPUT, authority: PreparedNumLockAuthority) {
        self.records.push(record);
        self.authority_after_record.push(authority);
        self.committed_events_after_record
            .push(self.committed_events);
    }

    fn finish_event(&mut self, first_record_index: usize) {
        self.committed_events = self.committed_events.saturating_add(1);
        if self.records.len() > first_record_index {
            *self
                .committed_events_after_record
                .last_mut()
                .expect("event appended at least one input record") = self.committed_events;
        } else if let Some(committed_after_last_record) =
            self.committed_events_after_record.last_mut()
        {
            // A source event that intentionally emits no INPUT records is
            // committed at the preceding record boundary.
            *committed_after_last_record = self.committed_events;
        } else {
            // Preserve an exact prefix when one or more zero-record events
            // precede the first record-producing event.
            self.committed_events_before_first_record = self.committed_events;
        }
    }
}

/// Result of one source-event injection batch. `committed_event_count` is an
/// exact prefix of the supplied events and remains valid even when `error` is
/// present, allowing callers to retry only the uncommitted suffix.
#[cfg(windows)]
#[derive(Debug)]
pub struct InputSendOutcome {
    pub committed_event_count: usize,
    pub remaining_events: Vec<InputEvent>,
    pub error: Option<anyhow::Error>,
}

#[cfg(windows)]
impl InputSendOutcome {
    fn success(committed_event_count: usize) -> Self {
        Self {
            committed_event_count,
            remaining_events: Vec::new(),
            error: None,
        }
    }

    fn failure(
        committed_event_count: usize,
        remaining_events: Vec<InputEvent>,
        error: anyhow::Error,
    ) -> Self {
        Self {
            committed_event_count,
            remaining_events,
            error: Some(error),
        }
    }

    pub fn into_result(self) -> Result<usize> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.committed_event_count),
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct WindowsInputState {
    num_lock: WindowsNumLockState,
}

/// Native Windows injector plus the exact successfully committed held-input
/// ledger. This is the common implementation used by the ordinary tray
/// adapter and the dedicated elevated helper.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct TrackedWindowsInput {
    windows_input: WindowsInputState,
    held: core_input::HeldInputState,
}

#[cfg(windows)]
impl TrackedWindowsInput {
    pub fn new(num_lock: WindowsNumLockState) -> Self {
        Self::with_windows_input(WindowsInputState::new(num_lock))
    }

    pub fn with_windows_input(windows_input: WindowsInputState) -> Self {
        Self {
            windows_input,
            held: core_input::HeldInputState::default(),
        }
    }

    pub fn send_events(&mut self, events: &[InputEvent]) -> InputSendOutcome {
        let outcome = self.windows_input.send_events(events);
        let committed = outcome.committed_event_count.min(events.len());
        self.held.observe(&events[..committed]);
        outcome
    }

    pub fn held_down_events(&self) -> Vec<InputEvent> {
        self.held.held_down_events()
    }

    pub fn is_idle(&self) -> bool {
        self.held.is_empty() && !self.windows_input.has_pending_native_cleanup()
    }

    /// Attempts one exact fail-open release pass. Partial native completion is
    /// retained in the held ledger so a watchdog can retry only what remains.
    pub fn release_all(&mut self) -> InputSendOutcome {
        let releases = self.held.release_events();
        self.send_events(&releases)
    }

    pub fn has_pending_native_cleanup(&self) -> bool {
        self.windows_input.has_pending_native_cleanup()
    }

    #[cfg(test)]
    fn send_events_with_sender<F>(&mut self, events: &[InputEvent], sender: F) -> InputSendOutcome
    where
        F: FnMut(&[INPUT]) -> Result<u32>,
    {
        let outcome = self.windows_input.send_events_with_sender(events, sender);
        let committed = outcome.committed_event_count.min(events.len());
        self.held.observe(&events[..committed]);
        outcome
    }
}

#[cfg(windows)]
impl WindowsInputState {
    pub fn new(num_lock: WindowsNumLockState) -> Self {
        Self { num_lock }
    }

    /// Reports whether a partially sent Boundless Num Lock toggle still owes
    /// Windows its synthetic key-up. Callers that survive an injection lane
    /// restart must retain this state until that cleanup succeeds.
    pub fn has_pending_native_cleanup(&self) -> bool {
        self.num_lock.lock().boundless_key_down
    }

    pub fn send_events(&self, events: &[InputEvent]) -> InputSendOutcome {
        self.send_events_with(events, send_input_records_once)
    }

    pub fn send_events_with_sender<F>(&self, events: &[InputEvent], sender: F) -> InputSendOutcome
    where
        F: FnMut(&[INPUT]) -> Result<u32>,
    {
        self.send_events_with(events, sender)
    }

    fn send_events_with<F>(&self, events: &[InputEvent], mut sender: F) -> InputSendOutcome
    where
        F: FnMut(&[INPUT]) -> Result<u32>,
    {
        // Serialize physical toggle observations with prepare/send/commit so
        // a concurrent local Num Lock press cannot be overwritten by a stale
        // post-SendInput state commit.
        let mut authority = self.num_lock.lock();
        if let Err(error) = release_pending_boundless_num_lock(&mut authority, &mut sender)
            .context("release pending Boundless Num Lock key before input batch")
        {
            return InputSendOutcome::failure(0, events.to_vec(), error);
        }
        let prepared = prepare_input_records(events, authority.on);
        send_prepared_input_records(events, &prepared, &mut authority, &mut sender)
    }
}

/// Reads Num Lock from a fresh thread that owns a Win32 message queue.
///
/// `GetKeyboardState` is message-queue state, so querying it from the daemon's
/// Tokio worker would not be a truthful fallback for the hook message lane.
/// Creating the queue with `PeekMessageW` gives the fallback its own supported
/// message lane, while `GetKeyboardState` lets us fail closed if Windows cannot
/// provide the seed instead of silently assuming that Num Lock is off.
#[cfg(windows)]
pub fn num_lock_state_from_dedicated_message_lane() -> Result<bool> {
    std::thread::Builder::new()
        .name("boundless-num-lock-seed".to_string())
        .spawn(|| -> Result<bool> {
            // SAFETY: A zero-initialized MSG is valid for PeekMessageW output.
            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            // SAFETY: `message` is writable for the duration of the call and
            // a null HWND requests messages for the current thread. Even when
            // no message is available, PeekMessageW creates this thread's
            // message queue before GetKeyState reads its keyboard state.
            unsafe {
                PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
            }
            let mut keyboard_state = [0u8; 256];
            // SAFETY: The buffer contains one writable byte for every virtual
            // key as required by GetKeyboardState.
            if unsafe { GetKeyboardState(keyboard_state.as_mut_ptr()) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("read Num Lock from dedicated Win32 message lane");
            }
            Ok((keyboard_state[usize::from(VK_NUMLOCK_CODE)] & 0x01) != 0)
        })
        .context("spawn dedicated Win32 message lane for Num Lock seeding")?
        .join()
        .map_err(|_| anyhow::anyhow!("dedicated Num Lock message lane panicked"))?
}

#[cfg(windows)]
pub(crate) fn num_lock_state_from_current_message_lane() -> bool {
    // SAFETY: GetKeyState accepts any virtual-key code. Callers ensure the
    // current thread owns the Win32 message queue whose state is authoritative.
    let state = unsafe { GetKeyState(i32::from(VK_NUMLOCK_CODE)) };
    (state as u16 & 0x0001) != 0
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
    prepare_input_records(events, initial_num_lock_on).records
}

#[cfg(windows)]
fn prepare_input_records(events: &[InputEvent], initial_num_lock_on: bool) -> PreparedInputRecords {
    let mut prepared = PreparedInputRecords::default();
    let mut authority = PreparedNumLockAuthority {
        on: initial_num_lock_on,
        boundless_key_down: false,
    };
    for event in events {
        let first_record_index = prepared.records.len();
        append_input_records_for_event(event, &mut authority, &mut prepared);
        prepared.finish_event(first_record_index);
        prepared
            .event_record_ranges
            .push((first_record_index, prepared.records.len()));
    }
    prepared
}

#[cfg(windows)]
fn append_input_records_for_event(
    event: &InputEvent,
    authority: &mut PreparedNumLockAuthority,
    prepared: &mut PreparedInputRecords,
) {
    match event {
        InputEvent::MouseMove { dx, dy } => {
            if *dx != 0 || *dy != 0 {
                prepared.push(mouse_input(*dx, *dy, 0, MOUSEEVENTF_MOVE), *authority);
            }
        }
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            prepared.push(
                mouse_input(
                    i32::from(*x_norm),
                    i32::from(*y_norm),
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ),
                *authority,
            );
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

            prepared.push(mouse_input(0, 0, mouse_data, flags), *authority);
        }
        InputEvent::MouseWheel { delta_x, delta_y } => {
            if *delta_y != 0 {
                prepared.push(
                    mouse_input(0, 0, *delta_y as u32, MOUSEEVENTF_WHEEL),
                    *authority,
                );
            }
            if *delta_x != 0 {
                prepared.push(
                    mouse_input(0, 0, *delta_x as u32, MOUSEEVENTF_HWHEEL),
                    *authority,
                );
            }
        }
        InputEvent::Key {
            scan_code,
            state,
            semantics,
        } => {
            let key_up = matches!(state, KeyState::Up);
            match semantics {
                KeySemantics::Physical => {
                    prepared.push(keyboard_input(*scan_code, key_up), *authority);
                }
                KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } if *virtual_key == VK_NUMLOCK_CODE => {
                    if !key_up && authority.on != *num_lock_on {
                        append_num_lock_toggle(*num_lock_on, authority, prepared);
                    }
                }
                KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } => {
                    let ambiguous_keypad = keypad_scan_uses_num_lock(*scan_code);
                    if !key_up && ambiguous_keypad && authority.on != *num_lock_on {
                        append_num_lock_toggle(*num_lock_on, authority, prepared);
                    }
                    if ambiguous_keypad && *virtual_key != 0 {
                        // Num Lock or a temporary modifier may change while
                        // the key is held. Replay every phase using the
                        // captured first-down identity so repeats and release
                        // cannot be remapped by newer destination state.
                        prepared.push(keyboard_virtual_key_input(*virtual_key, key_up), *authority);
                    } else {
                        prepared.push(keyboard_input(*scan_code, key_up), *authority);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn append_num_lock_toggle(
    desired_on: bool,
    authority: &mut PreparedNumLockAuthority,
    prepared: &mut PreparedInputRecords,
) {
    authority.on = desired_on;
    authority.boundless_key_down = true;
    prepared.push(
        keyboard_virtual_key_input(VK_NUMLOCK_CODE, false),
        *authority,
    );
    authority.boundless_key_down = false;
    prepared.push(
        keyboard_virtual_key_input(VK_NUMLOCK_CODE, true),
        *authority,
    );
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
                dwExtraInfo: BOUNDLESS_INJECTED_INPUT_MARKER,
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
                dwExtraInfo: BOUNDLESS_INJECTED_INPUT_MARKER,
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
                dwExtraInfo: BOUNDLESS_INJECTED_INPUT_MARKER,
            },
        },
    }
}

#[cfg(windows)]
fn send_prepared_input_records<F>(
    events: &[InputEvent],
    prepared: &PreparedInputRecords,
    authority: &mut WindowsNumLockAuthority,
    sender: &mut F,
) -> InputSendOutcome
where
    F: FnMut(&[INPUT]) -> Result<u32>,
{
    let mut offset = 0usize;
    let mut committed_event_count = prepared.committed_events_before_first_record;
    while offset < prepared.records.len() {
        let chunk = &prepared.records[offset..];
        let sent =
            match sender(chunk).with_context(|| format!("send input record at index {offset}")) {
                Ok(sent) => sent as usize,
                Err(error) => {
                    let remaining_events =
                        exact_remaining_events(events, prepared, offset, committed_event_count);
                    return finish_failed_input_send(
                        error,
                        committed_event_count,
                        remaining_events,
                        authority,
                        sender,
                    );
                }
            };
        if sent == 0 {
            let error = anyhow::anyhow!(
                "partial send at index {offset}: sent 0 / {} input records",
                chunk.len()
            );
            let remaining_events =
                exact_remaining_events(events, prepared, offset, committed_event_count);
            return finish_failed_input_send(
                error,
                committed_event_count,
                remaining_events,
                authority,
                sender,
            );
        }
        if sent > chunk.len() {
            let error = anyhow::anyhow!(
                "invalid send count at index {offset}: sent {sent} / {} input records",
                chunk.len()
            );
            let remaining_events =
                exact_remaining_events(events, prepared, offset, committed_event_count);
            return finish_failed_input_send(
                error,
                committed_event_count,
                remaining_events,
                authority,
                sender,
            );
        }

        for (next, next_committed_event_count) in prepared.authority_after_record
            [offset..offset + sent]
            .iter()
            .zip(&prepared.committed_events_after_record[offset..offset + sent])
        {
            authority.on = next.on;
            authority.boundless_key_down = next.boundless_key_down;
            committed_event_count = *next_committed_event_count;
        }
        offset += sent;
    }
    InputSendOutcome::success(prepared.committed_events)
}

#[cfg(windows)]
fn exact_remaining_events(
    events: &[InputEvent],
    prepared: &PreparedInputRecords,
    sent_record_count: usize,
    committed_event_count: usize,
) -> Vec<InputEvent> {
    let committed_event_count = committed_event_count.min(events.len());
    let mut remaining = events[committed_event_count..].to_vec();
    let Some(InputEvent::MouseWheel { delta_x, delta_y }) = remaining.first_mut() else {
        return remaining;
    };
    let Some((record_start, record_end)) = prepared
        .event_record_ranges
        .get(committed_event_count)
        .copied()
    else {
        return remaining;
    };
    let mut sent_in_event = sent_record_count
        .saturating_sub(record_start)
        .min(record_end.saturating_sub(record_start));
    if *delta_y != 0 && sent_in_event > 0 {
        *delta_y = 0;
        sent_in_event -= 1;
    }
    if *delta_x != 0 && sent_in_event > 0 {
        *delta_x = 0;
    }
    if *delta_x == 0 && *delta_y == 0 {
        remaining.remove(0);
    }
    remaining
}

#[cfg(windows)]
fn finish_failed_input_send<F>(
    error: anyhow::Error,
    committed_event_count: usize,
    remaining_events: Vec<InputEvent>,
    authority: &mut WindowsNumLockAuthority,
    sender: &mut F,
) -> InputSendOutcome
where
    F: FnMut(&[INPUT]) -> Result<u32>,
{
    match release_pending_boundless_num_lock(authority, sender) {
        Ok(()) => InputSendOutcome::failure(committed_event_count, remaining_events, error),
        Err(cleanup_error) => InputSendOutcome::failure(
            committed_event_count,
            remaining_events,
            anyhow::anyhow!(
                "{error:#}; Boundless Num Lock key-up cleanup failed: {cleanup_error:#}"
            ),
        ),
    }
}

#[cfg(windows)]
fn release_pending_boundless_num_lock<F>(
    authority: &mut WindowsNumLockAuthority,
    sender: &mut F,
) -> Result<()>
where
    F: FnMut(&[INPUT]) -> Result<u32>,
{
    if !authority.boundless_key_down {
        return Ok(());
    }

    let cleanup = [keyboard_virtual_key_input(VK_NUMLOCK_CODE, true)];
    let sent = sender(&cleanup).context("send Boundless Num Lock key-up cleanup")? as usize;
    if sent != cleanup.len() {
        bail!(
            "Num Lock key-up cleanup sent {sent} / {} input records",
            cleanup.len()
        );
    }
    authority.boundless_key_down = false;
    Ok(())
}

#[cfg(windows)]
fn send_input_records_once(records: &[INPUT]) -> Result<u32> {
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
    ambiguous_keypad_virtual_key(scan_code, false).is_some()
}

#[cfg(windows)]
pub(crate) fn ambiguous_keypad_virtual_key(scan_code: u16, num_lock_on: bool) -> Option<u16> {
    if is_extended_scan_code(scan_code) {
        return None;
    }

    Some(match (scan_code & 0x00FF, num_lock_on) {
        (0x47, true) => 0x67,  // VK_NUMPAD7
        (0x47, false) => 0x24, // VK_HOME
        (0x48, true) => 0x68,  // VK_NUMPAD8
        (0x48, false) => 0x26, // VK_UP
        (0x49, true) => 0x69,  // VK_NUMPAD9
        (0x49, false) => 0x21, // VK_PRIOR
        (0x4B, true) => 0x64,  // VK_NUMPAD4
        (0x4B, false) => 0x25, // VK_LEFT
        (0x4C, true) => 0x65,  // VK_NUMPAD5
        (0x4C, false) => 0x0C, // VK_CLEAR
        (0x4D, true) => 0x66,  // VK_NUMPAD6
        (0x4D, false) => 0x27, // VK_RIGHT
        (0x4F, true) => 0x61,  // VK_NUMPAD1
        (0x4F, false) => 0x23, // VK_END
        (0x50, true) => 0x62,  // VK_NUMPAD2
        (0x50, false) => 0x28, // VK_DOWN
        (0x51, true) => 0x63,  // VK_NUMPAD3
        (0x51, false) => 0x22, // VK_NEXT
        (0x52, true) => 0x60,  // VK_NUMPAD0
        (0x52, false) => 0x2D, // VK_INSERT
        (0x53, true) => 0x6E,  // VK_DECIMAL
        (0x53, false) => 0x2E, // VK_DELETE
        _ => return None,
    })
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
                assert_eq!(keypad.wVk, virtual_key);
                assert_eq!(keypad.wScan, 0);
                assert_eq!(keypad.dwFlags & KEYEVENTF_SCANCODE, 0);
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
        for record in &records[2..4] {
            let key = keyboard_record(record);
            assert_eq!(key.wVk, VK_NUMPAD1);
            assert_eq!(key.wScan, 0);
            assert_eq!(key.dwFlags & KEYEVENTF_SCANCODE, 0);
            assert_eq!(key.dwFlags & KEYEVENTF_KEYUP, 0);
        }
        let release = keyboard_record(&records[4]);
        assert_eq!(release.wVk, VK_NUMPAD1);
        assert_eq!(release.wScan, 0);
        assert_eq!(release.dwFlags & KEYEVENTF_SCANCODE, 0);
        assert_eq!(release.dwFlags & KEYEVENTF_KEYUP, KEYEVENTF_KEYUP);
    }

    #[test]
    fn shift_overridden_keypad_identity_survives_modifier_release_before_repeat() {
        const VK_END: u16 = 0x23;
        const VK_LSHIFT: u16 = 0xA0;

        let events = [
            windows_key(0x2A, VK_LSHIFT, true, KeyState::Down),
            windows_key(0x4F, VK_END, true, KeyState::Down),
            windows_key(0x2A, VK_LSHIFT, true, KeyState::Up),
            windows_key(0x4F, VK_END, true, KeyState::Down),
            windows_key(0x4F, VK_END, true, KeyState::Up),
        ];

        let records = input_records_for_events_with_num_lock_state(&events, false);
        assert_eq!(
            records.len(),
            7,
            "one Num Lock reconciliation pair plus the five captured key events"
        );
        let shift_down = keyboard_record(&records[0]);
        assert_eq!(shift_down.wScan, 0x2A);
        assert_ne!(shift_down.dwFlags & KEYEVENTF_SCANCODE, 0);
        assert_eq!(keyboard_record(&records[1]).wVk, VK_NUMLOCK_CODE);
        assert_eq!(keyboard_record(&records[2]).wVk, VK_NUMLOCK_CODE);

        for (index, key_up) in [(3usize, false), (5usize, false), (6usize, true)] {
            let keypad = keyboard_record(&records[index]);
            assert_eq!(
                keypad.wVk, VK_END,
                "ambiguous keypad event {index} must retain its cached virtual-key identity"
            );
            assert_eq!(keypad.wScan, 0);
            assert_eq!(keypad.dwFlags & KEYEVENTF_SCANCODE, 0);
            assert_eq!(
                keypad.dwFlags & KEYEVENTF_KEYUP != 0,
                key_up,
                "ambiguous keypad event {index} transition"
            );
        }

        let shift_up = keyboard_record(&records[4]);
        assert_eq!(shift_up.wScan, 0x2A);
        assert_ne!(shift_up.dwFlags & KEYEVENTF_SCANCODE, 0);
        assert_ne!(shift_up.dwFlags & KEYEVENTF_KEYUP, 0);
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
                    Ok(records.len() as u32)
                },
            )
            .into_result()
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
                    let keypad = keyboard_record(&records[0]);
                    assert_eq!(keypad.wVk, VK_NUMPAD1);
                    assert_eq!(keypad.wScan, 0);
                    assert_eq!(keypad.dwFlags & KEYEVENTF_SCANCODE, 0);
                    Ok(records.len() as u32)
                },
            )
            .into_result()
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
            .into_result()
            .expect_err("failed SendInput must surface");

        assert!(format!("{error:#}").contains("injected failure"));
        assert!(!num_lock.is_on());
    }

    #[test]
    fn partial_send_reports_exact_committed_key_prefix_before_mouse_failure() {
        let num_lock = WindowsNumLockState::new(false);
        let mut input = TrackedWindowsInput::new(num_lock);
        let events = [
            InputEvent::Key {
                scan_code: 30,
                state: KeyState::Down,
                semantics: KeySemantics::Physical,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
        ];
        let mut calls = 0usize;

        let outcome = input.send_events_with_sender(&events, |records| {
            calls += 1;
            match calls {
                1 => {
                    assert_eq!(records.len(), 2);
                    assert_eq!(records[0].r#type, INPUT_KEYBOARD);
                    assert_eq!(records[1].r#type, INPUT_MOUSE);
                    Ok(1)
                }
                2 => {
                    assert_eq!(records.len(), 1);
                    assert_eq!(records[0].r#type, INPUT_MOUSE);
                    Err(anyhow::anyhow!("scripted mouse injection failure"))
                }
                _ => panic!("unexpected send attempt {calls}"),
            }
        });

        assert_eq!(outcome.committed_event_count, 1);
        assert_eq!(outcome.remaining_events, vec![events[1].clone()]);
        assert_eq!(input.held_down_events(), vec![events[0].clone()]);
        let error = outcome.error.expect("partial send must preserve its error");
        assert!(format!("{error:#}").contains("scripted mouse injection failure"));
        assert_eq!(calls, 2);
    }

    #[test]
    fn partial_dual_axis_wheel_retry_contains_only_uncommitted_axis() {
        let input = WindowsInputState::new(WindowsNumLockState::new(false));
        let events = vec![InputEvent::MouseWheel {
            delta_x: 40,
            delta_y: 120,
        }];
        let mut calls = 0usize;
        let outcome = input.send_events_with_sender(&events, |records| {
            calls += 1;
            match calls {
                1 => {
                    assert_eq!(records.len(), 2);
                    assert_eq!(
                        unsafe { records[0].Anonymous.mi }.dwFlags,
                        MOUSEEVENTF_WHEEL
                    );
                    Ok(1)
                }
                2 => Err(anyhow::anyhow!("scripted horizontal failure")),
                _ => panic!("unexpected send attempt {calls}"),
            }
        });

        assert_eq!(outcome.committed_event_count, 0);
        assert_eq!(
            outcome.remaining_events,
            vec![InputEvent::MouseWheel {
                delta_x: 40,
                delta_y: 0,
            }]
        );
        assert!(outcome.error.is_some());

        input
            .send_events_with_sender(&outcome.remaining_events, |records| {
                assert_eq!(records.len(), 1);
                let horizontal = unsafe { records[0].Anonymous.mi };
                assert_eq!(horizontal.dwFlags, MOUSEEVENTF_HWHEEL);
                assert_eq!(horizontal.mouseData as i32, 40);
                Ok(1)
            })
            .into_result()
            .expect("retry only horizontal suffix");
    }

    #[test]
    fn zero_record_events_are_included_in_the_exact_committed_prefix() {
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock);
        let events = [
            InputEvent::MouseMove { dx: 0, dy: 0 },
            InputEvent::Key {
                scan_code: 30,
                state: KeyState::Down,
                semantics: KeySemantics::Physical,
            },
            InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: 0,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
        ];
        let mut calls = 0usize;

        let outcome = input.send_events_with_sender(&events, |records| {
            calls += 1;
            match calls {
                1 => {
                    assert_eq!(records.len(), 2);
                    Ok(1)
                }
                2 => Err(anyhow::anyhow!("scripted suffix failure")),
                _ => panic!("unexpected send attempt {calls}"),
            }
        });

        assert_eq!(
            outcome.committed_event_count, 3,
            "leading and interstitial zero-record events commit at their adjacent record boundary"
        );
        assert!(outcome.error.is_some());
    }

    #[test]
    fn keypad_hold_toggle_and_release_does_not_revert_new_num_lock_authority() {
        const VK_NUMPAD1: u16 = 0x61;
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());

        input
            .send_events_with(
                &[
                    windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down),
                    windows_key(0x45, VK_NUMLOCK_CODE, false, KeyState::Down),
                    windows_key(0x45, VK_NUMLOCK_CODE, false, KeyState::Up),
                    windows_key(0x4F, VK_NUMPAD1, true, KeyState::Up),
                ],
                |records| {
                    assert_eq!(
                        records.len(),
                        6,
                        "two intentional toggle pairs plus keypad down/up"
                    );
                    let release = keyboard_record(records.last().expect("keypad release"));
                    assert_eq!(release.wVk, VK_NUMPAD1);
                    assert_eq!(release.wScan, 0);
                    assert_ne!(release.dwFlags & KEYEVENTF_KEYUP, 0);
                    assert_eq!(release.dwFlags & KEYEVENTF_SCANCODE, 0);
                    assert_eq!(release.dwExtraInfo, BOUNDLESS_INJECTED_INPUT_MARKER);
                    let toggle_down_count = records
                        .iter()
                        .map(keyboard_record)
                        .filter(|key| {
                            key.wVk == VK_NUMLOCK_CODE && key.dwFlags & KEYEVENTF_KEYUP == 0
                        })
                        .count();
                    assert_eq!(toggle_down_count, 2, "release must not add a third toggle");
                    Ok(records.len() as u32)
                },
            )
            .into_result()
            .expect("inject keypad hold/toggle/release sequence");

        assert!(!num_lock.is_on(), "release must retain current authority");
    }

    #[test]
    fn partial_num_lock_toggle_reconciles_prefix_and_cleans_up_before_retry() {
        const VK_NUMPAD1: u16 = 0x61;
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());
        let event = windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down);
        let mut calls = 0usize;

        let error = input
            .send_events_with(std::slice::from_ref(&event), |records| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(records.len(), 3);
                        let toggle_down = keyboard_record(&records[0]);
                        assert_eq!(toggle_down.wVk, VK_NUMLOCK_CODE);
                        assert_eq!(toggle_down.dwFlags & KEYEVENTF_KEYUP, 0);
                        Ok(1)
                    }
                    2 => Err(anyhow::anyhow!("scripted keypad send failure")),
                    3 => {
                        assert_eq!(records.len(), 1, "cleanup is one bounded key-up");
                        let cleanup = keyboard_record(&records[0]);
                        assert_eq!(cleanup.wVk, VK_NUMLOCK_CODE);
                        assert_ne!(cleanup.dwFlags & KEYEVENTF_KEYUP, 0);
                        assert_eq!(cleanup.dwExtraInfo, BOUNDLESS_INJECTED_INPUT_MARKER);
                        Ok(1)
                    }
                    _ => panic!("unexpected send attempt {calls}"),
                }
            })
            .into_result()
            .expect_err("partial frame must remain retryable");

        assert!(error.to_string().contains("index 1"));
        assert_eq!(calls, 3);
        assert!(num_lock.is_on(), "sent toggle-down updates authority");
        assert!(!num_lock.lock().boundless_key_down);

        input
            .send_events_with(std::slice::from_ref(&event), |records| {
                assert_eq!(records.len(), 1, "retry must not toggle Num Lock again");
                let keypad = keyboard_record(&records[0]);
                assert_eq!(keypad.wVk, VK_NUMPAD1);
                assert_eq!(keypad.wScan, 0);
                assert_eq!(keypad.dwFlags & KEYEVENTF_SCANCODE, 0);
                Ok(1)
            })
            .into_result()
            .expect("retry after reconciled prefix");
    }

    #[test]
    fn failed_num_lock_cleanup_is_bounded_and_precedes_the_next_batch() {
        const VK_NUMPAD1: u16 = 0x61;
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());
        let event = windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down);
        let mut calls = 0usize;

        let error = input
            .send_events_with(std::slice::from_ref(&event), |_records| {
                calls += 1;
                match calls {
                    1 => Ok(1),
                    2 => Err(anyhow::anyhow!("scripted frame failure")),
                    3 => Err(anyhow::anyhow!("scripted cleanup failure")),
                    _ => panic!("cleanup must be attempted at most once"),
                }
            })
            .into_result()
            .expect_err("cleanup failure must surface");

        assert_eq!(calls, 3);
        assert!(error.to_string().contains("key-up cleanup failed"));
        assert!(num_lock.lock().boundless_key_down);

        let mut retry_calls = 0usize;
        input
            .send_events_with(std::slice::from_ref(&event), |records| {
                retry_calls += 1;
                assert_eq!(records.len(), 1);
                let key = keyboard_record(&records[0]);
                match retry_calls {
                    1 => {
                        assert_eq!(key.wVk, VK_NUMLOCK_CODE);
                        assert_ne!(key.dwFlags & KEYEVENTF_KEYUP, 0);
                    }
                    2 => {
                        assert_eq!(key.wVk, VK_NUMPAD1);
                        assert_eq!(key.wScan, 0);
                        assert_eq!(key.dwFlags & KEYEVENTF_SCANCODE, 0);
                    }
                    _ => panic!("unexpected retry send attempt {retry_calls}"),
                }
                Ok(1)
            })
            .into_result()
            .expect("next batch cleans up before retrying input");
        assert_eq!(retry_calls, 2);
        assert!(!num_lock.lock().boundless_key_down);
    }

    #[test]
    fn pending_num_lock_cleanup_survives_input_state_clone_and_empty_retry() {
        const VK_NUMPAD1: u16 = 0x61;
        let num_lock = WindowsNumLockState::new(false);
        let input = WindowsInputState::new(num_lock.clone());
        let event = windows_key(0x4F, VK_NUMPAD1, true, KeyState::Down);
        let mut initial_calls = 0usize;
        input
            .send_events_with(std::slice::from_ref(&event), |_records| {
                initial_calls += 1;
                match initial_calls {
                    1 => Ok(1),
                    2 => Err(anyhow::anyhow!("scripted frame failure")),
                    3 => Err(anyhow::anyhow!("scripted cleanup failure")),
                    _ => panic!("unexpected initial send {initial_calls}"),
                }
            })
            .into_result()
            .expect_err("native cleanup remains pending");
        assert!(input.has_pending_native_cleanup());

        let retained = input.clone();
        drop(input);
        let mut retry_calls = 0usize;
        retained
            .send_events_with(&[], |records| {
                retry_calls += 1;
                assert_eq!(records.len(), 1);
                let cleanup = keyboard_record(&records[0]);
                assert_eq!(cleanup.wVk, VK_NUMLOCK_CODE);
                assert_ne!(cleanup.dwFlags & KEYEVENTF_KEYUP, 0);
                Ok(1)
            })
            .into_result()
            .expect("retained state completes native cleanup without payload events");
        assert_eq!(retry_calls, 1);
        assert!(!retained.has_pending_native_cleanup());
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
