use super::*;

impl WindowsHookCaptureBackend {
    pub(super) fn new(state: &AppState) -> Result<Self> {
        let capture_runtime = CaptureRuntime::start({
            let state = state.clone();
            move |source| state.notify_input_capture_wake(source)
        })?;

        Ok(Self {
            capture_runtime,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
        })
    }

    fn update_pressed_state_and_filter(&mut self, event: InputEvent, output: &mut Vec<InputEvent>) {
        match event {
            InputEvent::MouseButton { button, state } => {
                let vk = virtual_key_for_mouse_button(button);
                let is_down = matches!(state, core_input::KeyState::Down);
                let prior = self.last_button_down.insert(vk, is_down);
                if prior != Some(is_down) {
                    output.push(InputEvent::MouseButton { button, state });
                }
            }
            InputEvent::Key { scan_code, state } => {
                let is_down = matches!(state, core_input::KeyState::Down);
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
}

impl InputCaptureBackend for WindowsHookCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
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
                    state: core_input::KeyState::Up,
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
                state: core_input::KeyState::Up,
            });
        }

        events
    }

    fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
        self.control_actions.clear();
        let _ = self.capture_runtime.drain_events();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
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
                HookCaptureEvent::Control(action) => {
                    let action = match action {
                        HookControlAction::EscapeUnlock => CaptureControlAction::EscapeUnlock,
                    };
                    self.control_actions.push_back(action);
                }
            }
        }

        Self::flush_pending_move(&mut output, &mut pending_move);

        Ok(output)
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        self.control_actions.drain(..).collect()
    }

    fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        self.capture_runtime.set_lock_active(active)
    }

    fn lock_supported(&self) -> bool {
        true
    }

    fn backend_mode(&self) -> &'static str {
        if self.capture_runtime.raw_input_enabled() {
            "hook_raw"
        } else {
            "hook"
        }
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        self.capture_runtime.take_dropped_event_count()
    }
}
