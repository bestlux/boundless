use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use core_input::{InputEvent, KeyState, MouseButton};

/// How long an attached user-session broker may go without an exchange before
/// the daemon treats it as gone and reports `service_session_unsupported`
/// again. Fail-closed: silence means no interactive input path.
pub(crate) const INPUT_BROKER_STALE_AFTER: Duration = Duration::from_secs(3);
const MAX_BROKER_CAPTURED_EVENTS: usize = 4096;

pub(crate) const INPUT_BROKER_BACKEND_MODE: &str = "user_session_broker";
pub(crate) const SERVICE_SESSION_UNSUPPORTED_BACKEND_MODE: &str = "service_session_unsupported";
pub(crate) const CLIPBOARD_DIRECT_BACKEND_MODE: &str = "direct";
pub(crate) const CLIPBOARD_USER_SESSION_BROKER_MODE: &str = "user_session_broker";
pub(crate) const CLIPBOARD_BROKER_UNAVAILABLE_MODE: &str = "broker_unavailable";

#[derive(Debug, Clone)]
pub struct InputBrokerAttachment {
    pub broker_token: String,
    pub lock_supported: bool,
}

#[derive(Debug, Default)]
struct InputBrokerRelayInner {
    service_session_input: bool,
    allowed_user_sid: Option<String>,
    attachment: Option<InputBrokerAttachment>,
    last_exchange_at: Option<Instant>,
    captured_events: VecDeque<InputEvent>,
    escape_unlock_pending: u32,
    cursor: Option<(i32, i32)>,
    virtual_bounds: Option<(i32, i32, i32, i32)>,
    desired_lock_active: bool,
    reported_lock_active: bool,
    dropped_event_count: u64,
    last_wheel_source_mode: Option<&'static str>,
    last_accepted_clipboard_sequence: Option<u64>,
    pressed_key_scan_codes: Vec<u16>,
    pressed_buttons: Vec<MouseButton>,
}

/// Session-neutral relay between the LocalSystem service daemon and the
/// user-session input broker. The daemon stays the routing/trust authority;
/// the broker only supplies captured events and applies inject frames.
#[derive(Debug, Default)]
pub struct InputBrokerRelay {
    inner: Mutex<InputBrokerRelayInner>,
}

impl InputBrokerRelay {
    fn lock(&self) -> std::sync::MutexGuard<'_, InputBrokerRelayInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn mark_service_session_input(&self) {
        self.lock().service_session_input = true;
    }

    pub(crate) fn service_session_input(&self) -> bool {
        self.lock().service_session_input
    }

    /// Configures the only account SID whose verified pipe clients may act as
    /// the user-session input broker. Unset means every broker attach fails
    /// closed.
    pub(crate) fn set_allowed_user_sid(&self, allowed_user_sid: String) {
        self.lock().allowed_user_sid = Some(allowed_user_sid);
    }

    pub(crate) fn allowed_user_sid(&self) -> Option<String> {
        self.lock().allowed_user_sid.clone()
    }

    pub(crate) fn attach(&self, attachment: InputBrokerAttachment) {
        let mut inner = self.lock();
        inner.attachment = Some(attachment);
        inner.last_exchange_at = Some(Instant::now());
        inner.captured_events.clear();
        inner.escape_unlock_pending = 0;
        inner.cursor = None;
        inner.virtual_bounds = None;
        inner.reported_lock_active = false;
        inner.dropped_event_count = 0;
        inner.last_wheel_source_mode = None;
        inner.last_accepted_clipboard_sequence = None;
        inner.pressed_key_scan_codes.clear();
        inner.pressed_buttons.clear();
    }

    pub(crate) fn detach(&self, broker_token: &str) -> bool {
        let mut inner = self.lock();
        let matches = inner
            .attachment
            .as_ref()
            .is_some_and(|attachment| attachment.broker_token == broker_token);
        if matches {
            inner.attachment = None;
            inner.last_exchange_at = None;
            inner.desired_lock_active = false;
            inner.reported_lock_active = false;
        }
        matches
    }

    pub(crate) fn detach_any(&self) -> bool {
        let mut inner = self.lock();
        let was_attached = inner.attachment.is_some();
        inner.attachment = None;
        inner.last_exchange_at = None;
        was_attached
    }

    fn attachment_fresh(inner: &InputBrokerRelayInner, now: Instant) -> bool {
        inner.attachment.is_some()
            && inner
                .last_exchange_at
                .is_some_and(|last| now.duration_since(last) < INPUT_BROKER_STALE_AFTER)
    }

    pub(crate) fn is_attached_fresh(&self, now: Instant) -> bool {
        Self::attachment_fresh(&self.lock(), now)
    }

    pub(crate) fn attachment(&self) -> Option<InputBrokerAttachment> {
        self.lock().attachment.clone()
    }

    /// Validates the broker token and refreshes the staleness deadline.
    /// A stale attachment is treated as already detached (fail closed).
    pub(crate) fn validate_and_touch(&self, broker_token: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        if !validate_attachment_token(&mut inner, broker_token, now) {
            return false;
        }
        inner.last_exchange_at = Some(now);
        true
    }

    /// Validates a clipboard exchange without extending input-broker
    /// liveness. Clipboard activity must never keep a stalled input broker in
    /// control of the interactive route.
    pub(crate) fn validate_without_touch(&self, broker_token: &str, now: Instant) -> bool {
        validate_attachment_token(&mut self.lock(), broker_token, now)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_broker_observations(
        &self,
        events: Vec<InputEvent>,
        cursor: Option<(i32, i32)>,
        virtual_bounds: Option<(i32, i32, i32, i32)>,
        escape_unlock_count: u32,
        reported_lock_active: bool,
        dropped_event_count: u64,
    ) -> usize {
        let mut inner = self.lock();
        if cursor.is_some() {
            inner.cursor = cursor;
        }
        if virtual_bounds.is_some() {
            inner.virtual_bounds = virtual_bounds;
        }
        inner.escape_unlock_pending = inner
            .escape_unlock_pending
            .saturating_add(escape_unlock_count);
        inner.reported_lock_active = reported_lock_active;
        inner.dropped_event_count = inner
            .dropped_event_count
            .saturating_add(dropped_event_count);

        let mut queued = 0usize;
        for event in events {
            track_pressed_state(&mut inner, &event);
            if inner.captured_events.len() >= MAX_BROKER_CAPTURED_EVENTS {
                inner.captured_events.pop_front();
                inner.dropped_event_count = inner.dropped_event_count.saturating_add(1);
            }
            inner.captured_events.push_back(event);
            queued += 1;
        }
        queued
    }

    pub(crate) fn drain_captured_events(&self) -> Vec<InputEvent> {
        let mut inner = self.lock();
        if !Self::attachment_fresh(&inner, Instant::now()) {
            inner.captured_events.clear();
            return Vec::new();
        }
        inner.captured_events.drain(..).collect()
    }

    pub(crate) fn take_escape_unlock_count(&self) -> u32 {
        std::mem::take(&mut self.lock().escape_unlock_pending)
    }

    pub(crate) fn cursor_position(&self) -> Option<(i32, i32)> {
        self.lock().cursor
    }

    pub(crate) fn virtual_bounds(&self) -> Option<(i32, i32, i32, i32)> {
        self.lock().virtual_bounds
    }

    /// Stores the runtime's requested lock state and returns the last lock
    /// state the broker reported as actually applied in its session.
    pub(crate) fn set_desired_lock_active(&self, active: bool) -> bool {
        let mut inner = self.lock();
        inner.desired_lock_active = active;
        if !Self::attachment_fresh(&inner, Instant::now()) {
            return false;
        }
        inner.reported_lock_active
    }

    pub(crate) fn desired_lock_active(&self) -> bool {
        self.lock().desired_lock_active
    }

    pub(crate) fn lock_supported(&self) -> bool {
        let inner = self.lock();
        Self::attachment_fresh(&inner, Instant::now())
            && inner
                .attachment
                .as_ref()
                .is_some_and(|attachment| attachment.lock_supported)
    }

    pub(crate) fn take_dropped_event_count(&self) -> u64 {
        std::mem::take(&mut self.lock().dropped_event_count)
    }

    /// Returns a source mode only when non-empty wheel observations change
    /// the active mode. Counts are aggregate-only and never expose a Raw
    /// Input device handle or other hardware identity.
    pub(crate) fn observe_wheel_source_counts(
        &self,
        raw_device: u32,
        raw_system: u32,
        hook: u32,
    ) -> Option<&'static str> {
        let present = [raw_device > 0, raw_system > 0, hook > 0];
        let source_count = present.into_iter().filter(|present| *present).count();
        let mode = match source_count {
            0 => return None,
            1 if raw_device > 0 => "raw_device",
            1 if raw_system > 0 => "raw_system",
            1 => "hook_fallback",
            _ => "mixed",
        };

        let mut inner = self.lock();
        if inner.last_wheel_source_mode == Some(mode) {
            return None;
        }
        inner.last_wheel_source_mode = Some(mode);
        Some(mode)
    }

    /// Marks a successfully handled local clipboard sequence and returns
    /// whether this is the first accepted observation of that sequence for
    /// the current broker attachment. Same-sequence response-loss retries are
    /// therefore idempotent.
    pub(crate) fn accept_local_clipboard_sequence(&self, sequence: u64) -> bool {
        let mut inner = self.lock();
        let is_new = inner.last_accepted_clipboard_sequence != Some(sequence);
        inner.last_accepted_clipboard_sequence = Some(sequence);
        is_new
    }

    /// Synthesizes release events for keys/buttons the broker reported as
    /// still pressed, so a capture-target switch or broker loss does not
    /// strand held input on the previous target.
    pub(crate) fn drain_release_events(&self) -> Vec<InputEvent> {
        let mut inner = self.lock();
        let events = release_events_for_pressed_state(&inner);
        inner.pressed_buttons.clear();
        inner.pressed_key_scan_codes.clear();
        events
    }

    pub(crate) fn release_events_snapshot(&self) -> Vec<InputEvent> {
        release_events_for_pressed_state(&self.lock())
    }

    pub(crate) fn clear_pressed_state(&self) {
        let mut inner = self.lock();
        inner.pressed_buttons.clear();
        inner.pressed_key_scan_codes.clear();
    }

    pub(crate) fn reset_capture_stream(&self) {
        let mut inner = self.lock();
        inner.captured_events.clear();
        inner.escape_unlock_pending = 0;
        inner.pressed_key_scan_codes.clear();
        inner.pressed_buttons.clear();
    }
}

#[cfg(test)]
impl InputBrokerRelay {
    pub(crate) fn expire_attachment_for_test(&self) {
        self.lock().last_exchange_at =
            Some(Instant::now() - INPUT_BROKER_STALE_AFTER - Duration::from_millis(1));
    }

    pub(crate) fn last_exchange_at_for_test(&self) -> Option<Instant> {
        self.lock().last_exchange_at
    }
}

fn validate_attachment_token(
    inner: &mut InputBrokerRelayInner,
    broker_token: &str,
    now: Instant,
) -> bool {
    if !InputBrokerRelay::attachment_fresh(inner, now) {
        // Preserve stale attachment identity and pressed state for an
        // authorized cleanup detach or replacement attach. Freshness remains
        // false, so the stale broker cannot regain routing authority.
        return false;
    }
    inner
        .attachment
        .as_ref()
        .is_some_and(|attachment| attachment.broker_token == broker_token)
}

fn track_pressed_state(inner: &mut InputBrokerRelayInner, event: &InputEvent) {
    match event {
        InputEvent::Key { scan_code, state } => match state {
            KeyState::Down => {
                if !inner.pressed_key_scan_codes.contains(scan_code) {
                    inner.pressed_key_scan_codes.push(*scan_code);
                }
            }
            KeyState::Up => inner
                .pressed_key_scan_codes
                .retain(|code| code != scan_code),
        },
        InputEvent::MouseButton { button, state } => match state {
            KeyState::Down => {
                if !inner.pressed_buttons.contains(button) {
                    inner.pressed_buttons.push(*button);
                }
            }
            KeyState::Up => inner.pressed_buttons.retain(|pressed| pressed != button),
        },
        InputEvent::MouseMove { .. }
        | InputEvent::MouseMoveAbsolute { .. }
        | InputEvent::MouseWheel { .. } => {}
    }
}

fn mouse_button_order(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

fn release_events_for_pressed_state(inner: &InputBrokerRelayInner) -> Vec<InputEvent> {
    let mut events = Vec::new();
    let mut buttons = inner.pressed_buttons.clone();
    buttons.sort_by_key(|button| mouse_button_order(*button));
    for button in buttons {
        events.push(InputEvent::MouseButton {
            button,
            state: KeyState::Up,
        });
    }
    let mut scan_codes = inner.pressed_key_scan_codes.clone();
    scan_codes.sort_unstable();
    for scan_code in scan_codes {
        events.push(InputEvent::Key {
            scan_code,
            state: KeyState::Up,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_source_diagnostics_emit_only_on_mode_transitions() {
        let relay = InputBrokerRelay::default();

        assert_eq!(relay.observe_wheel_source_counts(0, 0, 0), None);
        assert_eq!(
            relay.observe_wheel_source_counts(3, 0, 0),
            Some("raw_device")
        );
        assert_eq!(relay.observe_wheel_source_counts(1, 0, 0), None);
        assert_eq!(
            relay.observe_wheel_source_counts(0, 0, 2),
            Some("hook_fallback")
        );
        assert_eq!(relay.observe_wheel_source_counts(1, 0, 1), Some("mixed"));
        assert_eq!(relay.observe_wheel_source_counts(0, 4, 1), None);
    }

    #[test]
    fn broker_attach_resets_wheel_source_diagnostic_mode() {
        let relay = InputBrokerRelay::default();
        assert_eq!(
            relay.observe_wheel_source_counts(1, 0, 0),
            Some("raw_device")
        );
        relay.attach(InputBrokerAttachment {
            broker_token: "test".to_string(),
            lock_supported: true,
        });
        assert_eq!(
            relay.observe_wheel_source_counts(1, 0, 0),
            Some("raw_device")
        );
    }
}
