use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use core_input::{InputEvent, KeySemantics, KeyState, MouseButton};

use super::PendingInjectInputFrame;

/// How long an attached user-session broker may go without an exchange before
/// the daemon treats it as gone and reports `service_session_unsupported`
/// again. Fail-closed: silence means no interactive input path.
pub(crate) const INPUT_BROKER_STALE_AFTER: Duration = Duration::from_secs(3);
const MAX_BROKER_CAPTURED_EVENTS: usize = 4096;
const MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE: u32 = 64;

pub(crate) const INPUT_BROKER_BACKEND_MODE: &str = "user_session_broker";
pub(crate) const SERVICE_SESSION_UNSUPPORTED_BACKEND_MODE: &str = "service_session_unsupported";
pub(crate) const CLIPBOARD_DIRECT_BACKEND_MODE: &str = "direct";
pub(crate) const CLIPBOARD_USER_SESSION_BROKER_MODE: &str = "user_session_broker";
pub(crate) const CLIPBOARD_BROKER_UNAVAILABLE_MODE: &str = "broker_unavailable";

const ELEVATED_INJECTOR_STATES: &[&str] = &[
    "off",
    "prompting",
    "ready_pending_idle",
    "active",
    "stopping",
    "unavailable",
    "unknown",
];
const ELEVATED_INJECTOR_REASONS: &[&str] = &[
    "none",
    "user_cancelled",
    "not_installed",
    "wrong_path",
    "identity_rejected",
    "signature_invalid",
    "duplicate",
    "protocol_mismatch",
    "ipc_unavailable",
    "heartbeat_expired",
    "parent_exited",
    "inject_failed",
    "shutdown_incomplete",
    "unknown",
];
const ELEVATED_INJECTOR_SIGNATURE_TRUST: &[&str] =
    &["valid", "unsigned_dogfood", "invalid", "unknown"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElevatedInjectorStatus {
    pub state: String,
    pub reason: String,
    pub signature_trust: String,
}

impl Default for ElevatedInjectorStatus {
    fn default() -> Self {
        Self {
            state: "off".to_string(),
            reason: "none".to_string(),
            signature_trust: "unknown".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InputBrokerAttachment {
    pub broker_token: String,
    pub lock_supported: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SafetyUnlockCounts {
    pub escape: u32,
    pub lease_expired: u32,
    pub detector_unavailable: u32,
}

#[derive(Debug, Default)]
struct InputBrokerRelayInner {
    delivery_epoch: String,
    service_session_input: bool,
    allowed_user_sid: Option<String>,
    attachment: Option<InputBrokerAttachment>,
    last_exchange_at: Option<Instant>,
    captured_events: VecDeque<InputEvent>,
    safety_unlock_pending: SafetyUnlockCounts,
    handoff_probe_dx: i32,
    handoff_probe_dy: i32,
    cursor: Option<(i32, i32)>,
    virtual_bounds: Option<(i32, i32, i32, i32)>,
    desired_lock_active: bool,
    reported_lock_active: bool,
    capture_forwarding_authorized: bool,
    dropped_event_count: u64,
    last_wheel_source_mode: Option<&'static str>,
    elevated_injector_status: ElevatedInjectorStatus,
    last_accepted_clipboard_sequence: Option<u64>,
    pressed_keys: Vec<(u16, KeySemantics)>,
    pressed_buttons: Vec<MouseButton>,
    next_inject_batch_id: u64,
    last_acked_inject_batch_id: u64,
    inflight_inject_batch: Option<InputBrokerInjectBatch>,
}

#[derive(Debug, Clone)]
pub(crate) struct InputBrokerInjectBatch {
    pub batch_id: u64,
    pub authorization_generation: u64,
    pub frames: Vec<PendingInjectInputFrame>,
    pub cancelled: bool,
    delivery_uncertain_reported: bool,
}

/// Session-neutral relay between the LocalSystem service daemon and the
/// user-session input broker. The daemon stays the routing/trust authority;
/// the broker only supplies captured events and applies inject frames.
#[derive(Debug)]
pub struct InputBrokerRelay {
    inner: Mutex<InputBrokerRelayInner>,
}

impl Default for InputBrokerRelay {
    fn default() -> Self {
        Self {
            inner: Mutex::new(InputBrokerRelayInner {
                delivery_epoch: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            }),
        }
    }
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
        inner.safety_unlock_pending = SafetyUnlockCounts::default();
        inner.handoff_probe_dx = 0;
        inner.handoff_probe_dy = 0;
        inner.cursor = None;
        inner.virtual_bounds = None;
        inner.reported_lock_active = false;
        inner.capture_forwarding_authorized = false;
        inner.dropped_event_count = 0;
        inner.last_wheel_source_mode = None;
        inner.elevated_injector_status = ElevatedInjectorStatus::default();
        inner.last_accepted_clipboard_sequence = None;
        inner.pressed_keys.clear();
        inner.pressed_buttons.clear();
        // Inject delivery identity is daemon-instance state, not attachment
        // state. Keeping the receipt and in-flight batch across a transient
        // broker reattach lets a surviving tray process prove that it already
        // completed the exact delivery instead of applying it again.
    }

    pub(crate) fn delivery_epoch(&self) -> String {
        self.lock().delivery_epoch.clone()
    }

    /// Atomically validates the attachment and daemon delivery epoch, commits
    /// the tray's exact completed-batch receipt, and only then detaches. On a
    /// mismatched epoch or batch ID no receipt or attachment state changes.
    pub(crate) fn acknowledge_and_detach(
        &self,
        broker_token: &str,
        delivery_epoch: &str,
        acked_inject_batch_id: u64,
    ) -> Result<bool, &'static str> {
        let mut inner = self.lock();
        let matches = inner
            .attachment
            .as_ref()
            .is_some_and(|attachment| attachment.broker_token == broker_token);
        if !matches {
            return Ok(false);
        }
        if inner.delivery_epoch != delivery_epoch {
            return Err("delivery epoch mismatch");
        }
        Self::acknowledge_inject_batch_locked(&mut inner, acked_inject_batch_id)?;
        inner.attachment = None;
        inner.last_exchange_at = None;
        inner.desired_lock_active = false;
        inner.reported_lock_active = false;
        inner.capture_forwarding_authorized = false;
        inner.elevated_injector_status = ElevatedInjectorStatus::default();
        Ok(true)
    }

    pub(crate) fn detach_any(&self) -> bool {
        let mut inner = self.lock();
        let was_attached = inner.attachment.is_some();
        inner.attachment = None;
        inner.last_exchange_at = None;
        inner.desired_lock_active = false;
        inner.reported_lock_active = false;
        inner.capture_forwarding_authorized = false;
        inner.elevated_injector_status = ElevatedInjectorStatus::default();
        // `detach_any` is the destructive safe-reset path. Rotate the epoch
        // whenever delivery state is discarded so a surviving tray cannot
        // apply or acknowledge a pre-reset batch ID against the new state.
        inner.delivery_epoch = uuid::Uuid::new_v4().to_string();
        inner.next_inject_batch_id = 0;
        inner.last_acked_inject_batch_id = 0;
        inner.inflight_inject_batch = None;
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
        lease_expired_unlock_count: u32,
        detector_unavailable_unlock_count: u32,
        handoff_probe: Option<(i32, i32)>,
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
        inner.safety_unlock_pending.escape = inner
            .safety_unlock_pending
            .escape
            .saturating_add(escape_unlock_count)
            .min(MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE);
        inner.safety_unlock_pending.lease_expired = inner
            .safety_unlock_pending
            .lease_expired
            .saturating_add(lease_expired_unlock_count)
            .min(MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE);
        inner.safety_unlock_pending.detector_unavailable = inner
            .safety_unlock_pending
            .detector_unavailable
            .saturating_add(detector_unavailable_unlock_count)
            .min(MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE);
        if let Some((dx, dy)) = handoff_probe {
            inner.handoff_probe_dx = inner.handoff_probe_dx.saturating_add(dx);
            inner.handoff_probe_dy = inner.handoff_probe_dy.saturating_add(dy);
        }
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

    pub(crate) fn take_safety_unlock_counts(&self) -> SafetyUnlockCounts {
        std::mem::take(&mut self.lock().safety_unlock_pending)
    }

    pub(crate) fn drain_handoff_probe(&self) -> Option<InputEvent> {
        let mut inner = self.lock();
        let dx = std::mem::take(&mut inner.handoff_probe_dx);
        let dy = std::mem::take(&mut inner.handoff_probe_dy);
        (dx != 0 || dy != 0).then_some(InputEvent::MouseMove { dx, dy })
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
        if inner.desired_lock_active != active || !active {
            inner.capture_forwarding_authorized = false;
        }
        inner.desired_lock_active = active;
        if !Self::attachment_fresh(&inner, Instant::now()) {
            return false;
        }
        inner.reported_lock_active
    }

    pub(crate) fn desired_lock_active(&self) -> bool {
        self.lock().desired_lock_active
    }

    pub(crate) fn capture_forwarding_authorized(&self) -> bool {
        self.lock().capture_forwarding_authorized
    }

    pub(crate) fn set_capture_forwarding_authorized(&self, authorized: bool) {
        self.lock().capture_forwarding_authorized = authorized;
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

    /// Retains only a bounded, content-free injector capability vocabulary and
    /// returns the normalized status when it changes. A pre-upgrade broker that
    /// sends no status fields maps to the truthful disabled default.
    pub(crate) fn observe_elevated_injector_status(
        &self,
        state: &str,
        reason: &str,
        signature_trust: &str,
    ) -> Option<ElevatedInjectorStatus> {
        let normalized = normalize_elevated_injector_status(state, reason, signature_trust);
        let mut inner = self.lock();
        if inner.elevated_injector_status == normalized {
            return None;
        }
        inner.elevated_injector_status = normalized.clone();
        Some(normalized)
    }

    /// A stale or absent broker can never advertise an active privileged path.
    pub(crate) fn elevated_injector_status(&self) -> ElevatedInjectorStatus {
        let inner = self.lock();
        if !inner.service_session_input || !Self::attachment_fresh(&inner, Instant::now()) {
            return ElevatedInjectorStatus::default();
        }
        inner.elevated_injector_status.clone()
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
        inner.pressed_keys.clear();
        events
    }

    pub(crate) fn release_events_snapshot(&self) -> Vec<InputEvent> {
        release_events_for_pressed_state(&self.lock())
    }

    pub(crate) fn clear_pressed_state(&self) {
        let mut inner = self.lock();
        inner.pressed_buttons.clear();
        inner.pressed_keys.clear();
    }

    pub(crate) fn reset_capture_stream(&self) {
        let mut inner = self.lock();
        inner.captured_events.clear();
        inner.pressed_keys.clear();
        inner.capture_forwarding_authorized = false;
        inner.handoff_probe_dx = 0;
        inner.handoff_probe_dy = 0;
        inner.pressed_buttons.clear();
    }

    pub(crate) fn acknowledge_inject_batch(&self, batch_id: u64) -> Result<(), &'static str> {
        Self::acknowledge_inject_batch_locked(&mut self.lock(), batch_id)
    }

    fn acknowledge_inject_batch_locked(
        inner: &mut InputBrokerRelayInner,
        batch_id: u64,
    ) -> Result<(), &'static str> {
        if batch_id == 0 {
            return Ok(());
        }
        if inner.last_acked_inject_batch_id == batch_id {
            return Ok(());
        }
        let Some(inflight) = inner.inflight_inject_batch.as_ref() else {
            return Err("unknown inject batch acknowledgement");
        };
        if inflight.batch_id != batch_id {
            return Err("out-of-order inject batch acknowledgement");
        }
        inner.inflight_inject_batch = None;
        inner.last_acked_inject_batch_id = batch_id;
        Ok(())
    }

    pub(crate) fn inflight_inject_batch(&self) -> Option<InputBrokerInjectBatch> {
        self.lock().inflight_inject_batch.clone()
    }

    pub(crate) fn stage_inject_batch(
        &self,
        frames: Vec<PendingInjectInputFrame>,
        authorization_generation: u64,
    ) -> InputBrokerInjectBatch {
        let mut inner = self.lock();
        debug_assert!(inner.inflight_inject_batch.is_none());
        inner.next_inject_batch_id = inner.next_inject_batch_id.wrapping_add(1).max(1);
        let batch = InputBrokerInjectBatch {
            batch_id: inner.next_inject_batch_id,
            authorization_generation,
            frames,
            cancelled: false,
            delivery_uncertain_reported: false,
        };
        inner.inflight_inject_batch = Some(batch.clone());
        batch
    }

    pub(crate) fn cancel_inflight_inject_batch(
        &self,
        batch_id: u64,
    ) -> Option<InputBrokerInjectBatch> {
        let mut inner = self.lock();
        let batch = inner.inflight_inject_batch.as_mut()?;
        if batch.batch_id != batch_id {
            return None;
        }
        batch.cancelled = true;
        Some(batch.clone())
    }

    /// Marks a retained batch delivery as uncertain without replaying it.
    /// Repeated reports for the same retained batch are idempotent; any other
    /// ID fails closed. The boolean result identifies the first failure report
    /// independently of cancellation caused by another safety transition.
    pub(crate) fn fail_inflight_inject_batch(
        &self,
        batch_id: u64,
    ) -> Result<(InputBrokerInjectBatch, bool), &'static str> {
        if batch_id == 0 {
            return Err("failed inject batch id must be non-zero");
        }
        let mut inner = self.lock();
        let Some(batch) = inner.inflight_inject_batch.as_mut() else {
            return Err("failed inject batch is not in flight");
        };
        if batch.batch_id != batch_id {
            return Err("failed inject batch id does not match the in-flight batch");
        }
        let first_report = !batch.delivery_uncertain_reported;
        batch.cancelled = true;
        batch.delivery_uncertain_reported = true;
        Ok((batch.clone(), first_report))
    }

    pub(crate) fn take_inflight_inject_frames(&self) -> Vec<PendingInjectInputFrame> {
        self.lock()
            .inflight_inject_batch
            .take()
            .filter(|batch| !batch.cancelled)
            .map(|batch| batch.frames)
            .unwrap_or_default()
    }
}

fn normalize_elevated_injector_status(
    state: &str,
    reason: &str,
    signature_trust: &str,
) -> ElevatedInjectorStatus {
    if state.trim().is_empty() && reason.trim().is_empty() && signature_trust.trim().is_empty() {
        return ElevatedInjectorStatus::default();
    }
    ElevatedInjectorStatus {
        state: normalize_vocabulary_value(state, ELEVATED_INJECTOR_STATES),
        reason: normalize_vocabulary_value(reason, ELEVATED_INJECTOR_REASONS),
        signature_trust: normalize_vocabulary_value(
            signature_trust,
            ELEVATED_INJECTOR_SIGNATURE_TRUST,
        ),
    }
}

fn normalize_vocabulary_value(value: &str, allowed: &[&str]) -> String {
    let normalized = value.trim();
    if allowed.contains(&normalized) {
        normalized.to_string()
    } else {
        "unknown".to_string()
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
        InputEvent::Key {
            scan_code,
            state,
            semantics,
        } => match state {
            KeyState::Down => {
                if !inner
                    .pressed_keys
                    .iter()
                    .any(|(pressed_scan_code, _)| pressed_scan_code == scan_code)
                {
                    inner.pressed_keys.push((*scan_code, *semantics));
                }
            }
            KeyState::Up => inner
                .pressed_keys
                .retain(|(pressed_scan_code, _)| pressed_scan_code != scan_code),
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
    let mut pressed_keys = inner.pressed_keys.clone();
    pressed_keys.sort_unstable_by_key(|(scan_code, _)| *scan_code);
    for (scan_code, semantics) in pressed_keys {
        events.push(InputEvent::Key {
            scan_code,
            state: KeyState::Up,
            semantics,
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

    #[test]
    fn detach_release_keeps_first_down_semantics_across_key_repeat() {
        let relay = InputBrokerRelay::default();
        let first_down = KeySemantics::Windows {
            virtual_key: 0x61,
            num_lock_on: true,
        };
        let repeat_after_toggle = KeySemantics::Windows {
            virtual_key: 0x23,
            num_lock_on: false,
        };
        relay.push_broker_observations(
            vec![
                InputEvent::Key {
                    scan_code: 0x4F,
                    state: KeyState::Down,
                    semantics: first_down,
                },
                InputEvent::Key {
                    scan_code: 0x4F,
                    state: KeyState::Down,
                    semantics: repeat_after_toggle,
                },
            ],
            None,
            None,
            0,
            0,
            0,
            None,
            false,
            0,
        );

        assert_eq!(
            relay.drain_release_events(),
            vec![InputEvent::Key {
                scan_code: 0x4F,
                state: KeyState::Up,
                semantics: first_down,
            }]
        );
    }

    #[test]
    fn capture_stream_reset_preserves_pending_safety_unlock() {
        let relay = InputBrokerRelay::default();
        relay.push_broker_observations(Vec::new(), None, None, 1, 2, 3, None, false, 0);

        relay.reset_capture_stream();

        assert_eq!(
            relay.take_safety_unlock_counts(),
            SafetyUnlockCounts {
                escape: 1,
                lease_expired: 2,
                detector_unavailable: 3,
            },
            "a target transition must not consume broker safety reconciliation"
        );
    }

    #[test]
    fn handoff_probe_never_enters_captured_event_queue() {
        let relay = InputBrokerRelay::default();
        relay.push_broker_observations(Vec::new(), None, None, 0, 0, 0, Some((7, -2)), false, 0);

        assert!(relay.drain_captured_events().is_empty());
        assert_eq!(
            relay.drain_handoff_probe(),
            Some(InputEvent::MouseMove { dx: 7, dy: -2 })
        );
    }

    #[test]
    fn broker_safety_unlock_counts_are_memory_bounded() {
        let relay = InputBrokerRelay::default();
        relay.push_broker_observations(
            Vec::new(),
            None,
            None,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            None,
            false,
            0,
        );

        assert_eq!(
            relay.take_safety_unlock_counts(),
            SafetyUnlockCounts {
                escape: MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE,
                lease_expired: MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE,
                detector_unavailable: MAX_PENDING_SAFETY_UNLOCKS_PER_CAUSE,
            }
        );
    }
}
