use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use anyhow::Result;
use core_input::{InputEvent, KeySemantics, KeyState};

use super::hook_capture::{
    CaptureRuntime, CapturedWheelEvent, HookCaptureEvent, HookControlAction, WheelCaptureSource,
    mouse_button_from_virtual_key, virtual_key_for_mouse_button,
};

const WHEEL_DEDUPE_HOLD: Duration = Duration::from_millis(20);
const WHEEL_TOMBSTONE_TTL: Duration = Duration::from_millis(250);
const WHEEL_TOMBSTONE_CAP: usize = 512;

fn backend_mode_for(raw_input_enabled: bool, keyboard_hook_degraded: bool) -> &'static str {
    match (raw_input_enabled, keyboard_hook_degraded) {
        (true, true) => "hook_raw_keyboard_hook_degraded",
        (true, false) => "hook_raw",
        (false, true) => "hook_escape_detector_unavailable",
        (false, false) => "hook",
    }
}

#[derive(Debug, Clone)]
struct EmittedWheelTombstone {
    delta_x: i32,
    delta_y: i32,
    source: WheelCaptureSource,
    message_time_ms: u32,
    expires_at: Instant,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WheelSourceCounts {
    pub raw_device: u32,
    pub raw_system: u32,
    pub hook: u32,
}

/// Translates raw [`CaptureRuntime`] hook events into clean [`InputEvent`]
/// streams with pressed-state tracking, pending-move coalescing, and
/// synthetic release events. Shared by the daemon capture backend and the
/// user-session input broker host.
pub struct HookInputPump {
    capture_runtime: CaptureRuntime,
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, (bool, KeySemantics)>,
    last_button_down: HashMap<u16, bool>,
    pending_wheels: VecDeque<CapturedWheelEvent>,
    emitted_wheel_tombstones: VecDeque<EmittedWheelTombstone>,
    wheel_source_counts: WheelSourceCounts,
}

impl HookInputPump {
    pub fn start<F>(wake_notifier: F) -> Result<Self>
    where
        F: Fn(&'static str) + Send + Sync + 'static,
    {
        Ok(Self::from_capture_runtime(CaptureRuntime::start(
            wake_notifier,
        )?))
    }

    pub fn from_capture_runtime(capture_runtime: CaptureRuntime) -> Self {
        Self {
            capture_runtime,
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
            pending_wheels: VecDeque::new(),
            emitted_wheel_tombstones: VecDeque::new(),
            wheel_source_counts: WheelSourceCounts::default(),
        }
    }

    fn update_pressed_state_and_filter(&mut self, event: InputEvent, output: &mut Vec<InputEvent>) {
        match event {
            InputEvent::MouseButton { button, state } => {
                let vk = virtual_key_for_mouse_button(button);
                let is_down = matches!(state, KeyState::Down);
                let prior = self.last_button_down.insert(vk, is_down);
                if prior != Some(is_down) {
                    output.push(InputEvent::MouseButton { button, state });
                }
            }
            InputEvent::Key {
                scan_code,
                state,
                semantics,
            } => {
                let is_down = matches!(state, KeyState::Down);
                let prior = self.last_key_down.insert(scan_code, (is_down, semantics));
                if is_down || prior.map(|(down, _)| down) != Some(is_down) {
                    output.push(InputEvent::Key {
                        scan_code,
                        state,
                        semantics: if is_down {
                            semantics
                        } else {
                            prior
                                .filter(|(was_down, _)| *was_down)
                                .map(|(_, pressed_semantics)| pressed_semantics)
                                .unwrap_or(semantics)
                        },
                    });
                }
            }
            InputEvent::MouseMove { .. }
            | InputEvent::MouseMoveAbsolute { .. }
            | InputEvent::MouseWheel { .. } => output.push(event),
        }
    }

    fn accumulate_pending_move(pending_move: &mut Option<(i32, i32)>, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        match pending_move {
            Some((pending_dx, pending_dy)) => {
                *pending_dx = pending_dx.saturating_add(dx);
                *pending_dy = pending_dy.saturating_add(dy);
            }
            None => *pending_move = Some((dx, dy)),
        }
    }

    fn flush_pending_move(output: &mut Vec<InputEvent>, pending_move: &mut Option<(i32, i32)>) {
        let Some((dx, dy)) = pending_move.take() else {
            return;
        };
        if dx == 0 && dy == 0 {
            return;
        }
        output.push(InputEvent::MouseMove { dx, dy });
    }

    fn update_raw_input_runtime_state(&mut self) {
        let was_enabled = self.capture_runtime.raw_input_enabled();
        if !self.capture_runtime.refresh() && was_enabled {
            self.last_cursor = None;
        }
    }

    fn matching_wheel_copy(pending: &CapturedWheelEvent, incoming: &CapturedWheelEvent) -> bool {
        pending.source.is_raw() != incoming.source.is_raw()
            && pending.message_time_ms == incoming.message_time_ms
            && pending.delta_x == incoming.delta_x
            && pending.delta_y == incoming.delta_y
    }

    fn matching_emitted_wheel_copy(
        tombstone: &EmittedWheelTombstone,
        incoming: &CapturedWheelEvent,
    ) -> bool {
        tombstone.source.is_raw() != incoming.source.is_raw()
            && tombstone.message_time_ms == incoming.message_time_ms
            && tombstone.delta_x == incoming.delta_x
            && tombstone.delta_y == incoming.delta_y
    }

    fn prune_wheel_tombstones(&mut self, now: Instant) {
        while self
            .emitted_wheel_tombstones
            .front()
            .is_some_and(|tombstone| tombstone.expires_at <= now)
        {
            self.emitted_wheel_tombstones.pop_front();
        }
    }

    fn consume_emitted_wheel_copy(&mut self, incoming: &CapturedWheelEvent, now: Instant) -> bool {
        self.prune_wheel_tombstones(now);
        let Some(index) = self
            .emitted_wheel_tombstones
            .iter()
            .position(|tombstone| Self::matching_emitted_wheel_copy(tombstone, incoming))
        else {
            return false;
        };
        self.emitted_wheel_tombstones.remove(index);
        true
    }

    fn record_emitted_wheel_tombstone(&mut self, wheel: &CapturedWheelEvent, now: Instant) {
        self.prune_wheel_tombstones(now);
        if self.emitted_wheel_tombstones.len() >= WHEEL_TOMBSTONE_CAP {
            self.emitted_wheel_tombstones.pop_front();
        }
        self.emitted_wheel_tombstones
            .push_back(EmittedWheelTombstone {
                delta_x: wheel.delta_x,
                delta_y: wheel.delta_y,
                source: wheel.source,
                message_time_ms: wheel.message_time_ms,
                expires_at: now + WHEEL_TOMBSTONE_TTL,
            });
    }

    fn emit_wheel(&mut self, wheel: CapturedWheelEvent, output: &mut Vec<InputEvent>) {
        match wheel.source {
            WheelCaptureSource::RawDevice => {
                self.wheel_source_counts.raw_device =
                    self.wheel_source_counts.raw_device.saturating_add(1);
            }
            WheelCaptureSource::RawSystem => {
                self.wheel_source_counts.raw_system =
                    self.wheel_source_counts.raw_system.saturating_add(1);
            }
            WheelCaptureSource::Hook => {
                self.wheel_source_counts.hook = self.wheel_source_counts.hook.saturating_add(1);
            }
        }
        output.push(InputEvent::MouseWheel {
            delta_x: wheel.delta_x,
            delta_y: wheel.delta_y,
        });
    }

    fn observe_wheel(&mut self, incoming: CapturedWheelEvent, output: &mut Vec<InputEvent>) {
        if self.consume_emitted_wheel_copy(&incoming, Instant::now()) {
            return;
        }
        if let Some(index) = self
            .pending_wheels
            .iter()
            .position(|pending| Self::matching_wheel_copy(pending, &incoming))
        {
            let counterpart = self
                .pending_wheels
                .remove(index)
                .expect("matching pending wheel must remain present");
            let canonical = if incoming.source.is_raw() {
                incoming
            } else {
                counterpart
            };
            self.emit_wheel(canonical, output);
        } else {
            self.pending_wheels.push_back(incoming);
        }
    }

    fn flush_expired_wheels(&mut self, now: Instant, output: &mut Vec<InputEvent>) {
        let mut retained = VecDeque::with_capacity(self.pending_wheels.len());
        while let Some(wheel) = self.pending_wheels.pop_front() {
            if now.saturating_duration_since(wheel.observed_at) >= WHEEL_DEDUPE_HOLD {
                self.record_emitted_wheel_tombstone(&wheel, now);
                self.emit_wheel(wheel, output);
            } else {
                retained.push_back(wheel);
            }
        }
        self.pending_wheels = retained;
    }

    pub fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();

        let mut pressed_buttons = self
            .last_button_down
            .iter()
            .filter_map(|(vk, down)| if *down { Some(*vk) } else { None })
            .collect::<Vec<_>>();
        pressed_buttons.sort_unstable();
        for vk in pressed_buttons {
            if let Some(button) = mouse_button_from_virtual_key(vk) {
                events.push(InputEvent::MouseButton {
                    button,
                    state: KeyState::Up,
                });
            }
        }

        let mut pressed_keys = self
            .last_key_down
            .iter()
            .filter_map(|(scan_code, (down, semantics))| {
                if *down {
                    Some((*scan_code, *semantics))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
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

    pub fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
        self.pending_wheels.clear();
        self.emitted_wheel_tombstones.clear();
        let _ = self.capture_runtime.drain_control_actions();
        let _ = self.capture_runtime.drain_events();
    }

    pub fn poll_events(&mut self) -> Vec<InputEvent> {
        self.update_raw_input_runtime_state();

        let mut output = Vec::new();
        let mut pending_move: Option<(i32, i32)> = None;
        self.flush_expired_wheels(Instant::now(), &mut output);

        for event in self.capture_runtime.drain_events() {
            match event {
                HookCaptureEvent::MouseDelta { dx, dy } => {
                    if self.capture_runtime.raw_input_enabled() {
                        Self::accumulate_pending_move(&mut pending_move, dx, dy);
                    }
                }
                HookCaptureEvent::MousePosition { x, y } => {
                    if let Some((last_x, last_y)) = self.last_cursor {
                        let dx = x - last_x;
                        let dy = y - last_y;
                        if !self.capture_runtime.raw_input_enabled()
                            || !self.capture_runtime.lock_active()
                        {
                            Self::accumulate_pending_move(&mut pending_move, dx, dy);
                        }
                    }
                    self.last_cursor = Some((x, y));
                }
                HookCaptureEvent::Input(input_event) => {
                    Self::flush_pending_move(&mut output, &mut pending_move);
                    self.update_pressed_state_and_filter(input_event, &mut output);
                }
                HookCaptureEvent::Wheel(wheel) => {
                    Self::flush_pending_move(&mut output, &mut pending_move);
                    self.observe_wheel(wheel, &mut output);
                }
            }
        }

        Self::flush_pending_move(&mut output, &mut pending_move);
        self.flush_expired_wheels(Instant::now(), &mut output);

        output
    }

    pub fn drain_control_actions(&mut self) -> Vec<HookControlAction> {
        self.capture_runtime.drain_control_actions()
    }

    pub fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        self.capture_runtime.set_lock_active(active)
    }

    pub fn safety_unlock_generation(&self) -> u64 {
        self.capture_runtime.safety_unlock_generation()
    }

    pub fn set_lock_active_if_safety_generation(
        &mut self,
        active: bool,
        expected_generation: u64,
    ) -> Result<bool> {
        self.capture_runtime
            .set_lock_active_if_safety_generation(active, expected_generation)
    }

    pub fn lock_active(&self) -> bool {
        self.capture_runtime.lock_active()
    }

    pub fn enable_lock_lease(&mut self, timeout: std::time::Duration) -> Result<()> {
        self.capture_runtime.enable_lock_lease(timeout)
    }

    pub fn renew_lock_lease(&self) -> bool {
        self.capture_runtime.renew_lock_lease()
    }

    pub fn backend_mode(&self) -> &'static str {
        backend_mode_for(
            self.capture_runtime.raw_input_enabled(),
            self.capture_runtime.keyboard_hook_degraded(),
        )
    }

    pub fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }

    pub fn take_dropped_event_count(&mut self) -> u64 {
        self.capture_runtime.take_dropped_event_count()
    }

    pub fn take_wheel_source_counts(&mut self) -> WheelSourceCounts {
        std::mem::take(&mut self.wheel_source_counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn backend_mode_reports_escape_detector_availability() {
        assert_eq!(backend_mode_for(true, false), "hook_raw");
        assert_eq!(
            backend_mode_for(true, true),
            "hook_raw_keyboard_hook_degraded"
        );
        assert_eq!(
            backend_mode_for(false, true),
            "hook_escape_detector_unavailable"
        );
        assert_eq!(backend_mode_for(false, false), "hook");
    }

    fn wheel(
        delta_x: i32,
        delta_y: i32,
        source: WheelCaptureSource,
        message_time_ms: u32,
        observed_at: Instant,
    ) -> HookCaptureEvent {
        HookCaptureEvent::Wheel(CapturedWheelEvent {
            delta_x,
            delta_y,
            source,
            message_time_ms,
            observed_at,
        })
    }

    #[test]
    fn high_resolution_wheel_delta_survives_hook_pump() {
        let (tx, rx) = mpsc::sync_channel(4);
        let observed_at = Instant::now();
        tx.send(wheel(-17, 30, WheelCaptureSource::Hook, 41, observed_at))
            .expect("queue hook wheel");
        tx.send(wheel(
            -17,
            30,
            WheelCaptureSource::RawDevice,
            41,
            observed_at,
        ))
        .expect("queue raw wheel");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: -17,
                delta_y: 30,
            }]
        );
        assert_eq!(
            pump.take_wheel_source_counts(),
            WheelSourceCounts {
                raw_device: 1,
                raw_system: 0,
                hook: 0,
            }
        );
    }

    #[test]
    fn signed_wheel_deltas_dedupe_one_to_one_across_raw_and_hook_sources() {
        let cases = [1, -1, 40, -40, 120, -120];
        let (tx, rx) = mpsc::sync_channel(32);
        let observed_at = Instant::now();
        for (index, delta) in cases.into_iter().enumerate() {
            let timestamp = 100 + index as u32;
            tx.send(wheel(
                0,
                delta,
                WheelCaptureSource::Hook,
                timestamp,
                observed_at,
            ))
            .expect("queue hook vertical wheel");
            tx.send(wheel(
                0,
                delta,
                WheelCaptureSource::RawDevice,
                timestamp,
                observed_at,
            ))
            .expect("queue raw vertical wheel");
            tx.send(wheel(
                delta,
                0,
                WheelCaptureSource::RawSystem,
                timestamp + 50,
                observed_at,
            ))
            .expect("queue raw horizontal wheel");
            tx.send(wheel(
                delta,
                0,
                WheelCaptureSource::Hook,
                timestamp + 50,
                observed_at,
            ))
            .expect("queue hook horizontal wheel");
        }
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        let expected = cases
            .into_iter()
            .flat_map(|delta| {
                [
                    InputEvent::MouseWheel {
                        delta_x: 0,
                        delta_y: delta,
                    },
                    InputEvent::MouseWheel {
                        delta_x: delta,
                        delta_y: 0,
                    },
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(pump.poll_events(), expected);
        assert_eq!(
            pump.take_wheel_source_counts(),
            WheelSourceCounts {
                raw_device: 6,
                raw_system: 6,
                hook: 0,
            }
        );
    }

    #[test]
    fn repeated_identical_wheels_with_one_timestamp_remain_distinct() {
        let (tx, rx) = mpsc::sync_channel(8);
        let observed_at = Instant::now();
        for source in [
            WheelCaptureSource::Hook,
            WheelCaptureSource::Hook,
            WheelCaptureSource::RawDevice,
            WheelCaptureSource::RawDevice,
        ] {
            tx.send(wheel(0, 1, source, 55, observed_at))
                .expect("queue repeated wheel");
        }
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        assert_eq!(
            pump.poll_events(),
            vec![
                InputEvent::MouseWheel {
                    delta_x: 0,
                    delta_y: 1,
                },
                InputEvent::MouseWheel {
                    delta_x: 0,
                    delta_y: 1,
                }
            ]
        );
    }

    #[test]
    fn hook_only_wheels_fall_back_after_bounded_dedupe_hold() {
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(wheel(
            0,
            -40,
            WheelCaptureSource::Hook,
            90,
            Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1),
        ))
        .expect("queue fallback wheel");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: -40,
            }]
        );
        assert_eq!(
            pump.take_wheel_source_counts(),
            WheelSourceCounts {
                raw_device: 0,
                raw_system: 0,
                hook: 1,
            }
        );
    }

    #[test]
    fn late_raw_counterpart_is_suppressed_after_hook_fallback_emits() {
        let (tx, rx) = mpsc::sync_channel(4);
        let old = Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1);
        tx.send(wheel(0, -40, WheelCaptureSource::Hook, 91, old))
            .expect("queue hook fallback");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);
        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: -40,
            }]
        );

        tx.send(wheel(0, -40, WheelCaptureSource::RawDevice, 91, old))
            .expect("queue late raw counterpart");
        assert!(
            pump.poll_events().is_empty(),
            "late raw delivery must not duplicate the emitted hook fallback"
        );
    }

    #[test]
    fn late_counterparts_consume_tombstones_one_to_one_for_repeated_events() {
        let (tx, rx) = mpsc::sync_channel(8);
        let old = Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1);
        for _ in 0..2 {
            tx.send(wheel(0, 1, WheelCaptureSource::Hook, 92, old))
                .expect("queue repeated hook fallback");
        }
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);
        assert_eq!(pump.poll_events().len(), 2);

        for _ in 0..3 {
            tx.send(wheel(0, 1, WheelCaptureSource::RawDevice, 92, old))
                .expect("queue repeated late raw");
        }
        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: 1,
            }],
            "two late counterparts consume two tombstones; the extra physical event survives"
        );
    }

    #[test]
    fn emitted_wheel_tombstones_are_memory_bounded() {
        let event_count = WHEEL_TOMBSTONE_CAP + 20;
        let (tx, rx) = mpsc::sync_channel(event_count);
        let old = Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1);
        for index in 0..event_count {
            tx.send(wheel(0, 1, WheelCaptureSource::Hook, index as u32, old))
                .expect("queue hook fallback");
        }
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);
        assert_eq!(pump.poll_events().len(), event_count);
        assert_eq!(pump.emitted_wheel_tombstones.len(), WHEEL_TOMBSTONE_CAP);
    }

    #[test]
    fn expired_wheel_tombstone_is_pruned_without_collapsing_later_event() {
        let (tx, rx) = mpsc::sync_channel(4);
        let old = Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1);
        tx.send(wheel(0, 40, WheelCaptureSource::Hook, 93, old))
            .expect("queue hook fallback");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);
        assert_eq!(pump.poll_events().len(), 1);
        pump.emitted_wheel_tombstones
            .front_mut()
            .expect("hook tombstone")
            .expires_at = Instant::now() - Duration::from_millis(1);

        tx.send(wheel(0, 40, WheelCaptureSource::RawDevice, 93, old))
            .expect("queue raw event after tombstone expiry");
        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: 0,
                delta_y: 40,
            }],
            "an expired signature must not collapse a later physical event"
        );
    }

    #[test]
    fn identical_wheels_with_different_timestamps_are_not_deduped() {
        let (tx, rx) = mpsc::sync_channel(4);
        let observed_at = Instant::now() - WHEEL_DEDUPE_HOLD - Duration::from_millis(1);
        tx.send(wheel(0, 1, WheelCaptureSource::Hook, 100, observed_at))
            .expect("queue first wheel");
        tx.send(wheel(0, 1, WheelCaptureSource::RawDevice, 101, observed_at))
            .expect("queue second wheel");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        assert_eq!(pump.poll_events().len(), 2);
    }

    #[test]
    fn key_repeat_survives_and_release_keeps_pressed_semantics() {
        let (tx, rx) = mpsc::sync_channel(4);
        let pressed_semantics = KeySemantics::Windows {
            virtual_key: 0x61,
            num_lock_on: true,
        };
        let release_observation = KeySemantics::Windows {
            virtual_key: 0x23,
            num_lock_on: false,
        };
        for state in [KeyState::Down, KeyState::Down] {
            tx.send(HookCaptureEvent::Input(InputEvent::Key {
                scan_code: 0x4F,
                state,
                semantics: pressed_semantics,
            }))
            .expect("queue keypad down/repeat");
        }
        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 0x4F,
            state: KeyState::Up,
            semantics: release_observation,
        }))
        .expect("queue keypad up");
        tx.send(HookCaptureEvent::Input(InputEvent::Key {
            scan_code: 0x4F,
            state: KeyState::Up,
            semantics: release_observation,
        }))
        .expect("queue duplicate keypad up");

        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);
        assert_eq!(
            pump.poll_events(),
            vec![
                InputEvent::Key {
                    scan_code: 0x4F,
                    state: KeyState::Down,
                    semantics: pressed_semantics,
                },
                InputEvent::Key {
                    scan_code: 0x4F,
                    state: KeyState::Down,
                    semantics: pressed_semantics,
                },
                InputEvent::Key {
                    scan_code: 0x4F,
                    state: KeyState::Up,
                    semantics: pressed_semantics,
                },
            ]
        );
    }
}
