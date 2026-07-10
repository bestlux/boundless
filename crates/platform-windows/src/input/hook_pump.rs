use std::collections::HashMap;

use anyhow::Result;
use core_input::{InputEvent, KeyState};

use super::hook_capture::{
    CaptureRuntime, HookCaptureEvent, HookControlAction, mouse_button_from_virtual_key,
    virtual_key_for_mouse_button,
};

/// Translates raw [`CaptureRuntime`] hook events into clean [`InputEvent`]
/// streams with pressed-state tracking, pending-move coalescing, and
/// synthetic release events. Shared by the daemon capture backend and the
/// user-session input broker host.
pub struct HookInputPump {
    capture_runtime: CaptureRuntime,
    last_cursor: Option<(i32, i32)>,
    last_key_down: HashMap<u16, bool>,
    last_button_down: HashMap<u16, bool>,
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
            InputEvent::Key { scan_code, state } => {
                let is_down = matches!(state, KeyState::Down);
                let prior = self.last_key_down.insert(scan_code, is_down);
                if is_down || prior != Some(is_down) {
                    output.push(InputEvent::Key { scan_code, state });
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
            .filter_map(|(scan_code, down)| if *down { Some(*scan_code) } else { None })
            .collect::<Vec<_>>();
        pressed_keys.sort_unstable();
        for scan_code in pressed_keys {
            events.push(InputEvent::Key {
                scan_code,
                state: KeyState::Up,
            });
        }

        events
    }

    pub fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
        let _ = self.capture_runtime.drain_control_actions();
        let _ = self.capture_runtime.drain_events();
    }

    pub fn poll_events(&mut self) -> Vec<InputEvent> {
        self.update_raw_input_runtime_state();

        let mut output = Vec::new();
        let mut pending_move: Option<(i32, i32)> = None;

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
            }
        }

        Self::flush_pending_move(&mut output, &mut pending_move);

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
        if self.capture_runtime.raw_input_enabled() {
            "hook_raw"
        } else {
            "hook"
        }
    }

    pub fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }

    pub fn take_dropped_event_count(&mut self) -> u64 {
        self.capture_runtime.take_dropped_event_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn high_resolution_wheel_delta_survives_hook_pump() {
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(HookCaptureEvent::Input(InputEvent::MouseWheel {
            delta_x: -17,
            delta_y: 30,
        }))
        .expect("queue wheel");
        let runtime = CaptureRuntime::from_test_parts(rx, true);
        let mut pump = HookInputPump::from_capture_runtime(runtime);

        assert_eq!(
            pump.poll_events(),
            vec![InputEvent::MouseWheel {
                delta_x: -17,
                delta_y: 30,
            }]
        );
    }
}
