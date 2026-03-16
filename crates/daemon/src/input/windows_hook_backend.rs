use super::*;

impl WindowsHookCaptureBackend {
    pub(super) fn new(state: &AppState) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::sync_channel::<HookCaptureEvent>(HOOK_EVENT_QUEUE_CAP);
        let (startup_tx, startup_rx) = mpsc::channel::<Result<u32>>();

        let hook_thread = thread::spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            if let Err(error) = set_hook_event_sender(Some(event_tx)) {
                let _ = startup_tx.send(Err(error));
                return;
            }

            let _guard = HookSenderGuard;
            let keyboard_hook = unsafe { install_keyboard_hook() };
            let mouse_hook = unsafe { install_mouse_hook() };
            match (keyboard_hook, mouse_hook) {
                (Ok(keyboard_hook), Ok(mouse_hook)) => {
                    let _ = startup_tx.send(Ok(thread_id));
                    if let Err(error) = unsafe { run_hook_message_loop() } {
                        warn!(error = ?error, "hook message loop exited with error");
                    }
                    unsafe {
                        let _ = UnhookWindowsHookEx(keyboard_hook);
                        let _ = UnhookWindowsHookEx(mouse_hook);
                    }
                }
                (keyboard, mouse) => {
                    if let Ok(hook) = keyboard.as_ref() {
                        unsafe {
                            let _ = UnhookWindowsHookEx(*hook);
                        }
                    }
                    if let Ok(hook) = mouse.as_ref() {
                        unsafe {
                            let _ = UnhookWindowsHookEx(*hook);
                        }
                    }
                    let error = keyboard
                        .err()
                        .or_else(|| mouse.err())
                        .unwrap_or_else(|| anyhow::anyhow!("failed to install capture hooks"));
                    let _ = startup_tx.send(Err(error));
                }
            }
        });

        let hook_thread_id = startup_rx.recv().context("hook startup channel closed")??;
        set_hook_wake_state(Some(state.clone())).context("set hook wake state")?;
        let (raw_input_thread_id, raw_input_thread, raw_input_enabled) =
            match spawn_raw_input_thread() {
                Ok((thread_id, thread)) => (Some(thread_id), Some(thread), true),
                Err(error) => {
                    warn!(
                        error = ?error,
                        "raw input mouse capture unavailable; falling back to mouse hook position deltas"
                    );
                    (None, None, false)
                }
            };

        Ok(Self {
            event_rx,
            hook_thread_id,
            hook_thread: Some(hook_thread),
            raw_input_thread_id,
            raw_input_thread,
            raw_input_enabled,
            lock_active: false,
            control_actions: VecDeque::new(),
            last_cursor: None,
            last_key_down: HashMap::new(),
            last_button_down: HashMap::new(),
        })
    }

    fn drain_pending_events(&mut self) -> Vec<HookCaptureEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
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
        if !self.raw_input_enabled {
            return;
        }

        let finished = self
            .raw_input_thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished());
        if !finished {
            return;
        }

        if let Some(thread) = self.raw_input_thread.take() {
            let _ = thread.join();
        }
        self.raw_input_thread_id = None;
        self.raw_input_enabled = false;
        self.last_cursor = None;
        warn!("raw input capture thread exited; using mouse hook position delta fallback");
    }
}

impl Drop for WindowsHookCaptureBackend {
    fn drop(&mut self) {
        if self.lock_active {
            let _ = set_hook_lock_active(false);
            self.lock_active = false;
        }
        if let Some(thread_id) = self.raw_input_thread_id {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(thread) = self.raw_input_thread.take() {
            let _ = thread.join();
        }
        self.raw_input_thread_id = None;
        self.raw_input_enabled = false;
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.hook_thread.take() {
            let _ = thread.join();
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
        let _ = self.drain_pending_events();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        self.update_raw_input_runtime_state();

        let mut output = Vec::new();
        let mut pending_move: Option<(i32, i32)> = None;

        for event in self.drain_pending_events() {
            match event {
                HookCaptureEvent::MouseDelta { dx, dy } => {
                    if self.raw_input_enabled {
                        Self::accumulate_pending_move(&mut pending_move, dx, dy);
                    }
                }
                HookCaptureEvent::MousePosition { x, y } => {
                    if let Some((last_x, last_y)) = self.last_cursor {
                        let dx = x - last_x;
                        let dy = y - last_y;
                        if !self.raw_input_enabled || !self.lock_active {
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
        if self.lock_active != active {
            set_hook_lock_active(active)?;
            self.lock_active = active;
        }
        Ok(self.lock_active)
    }

    fn lock_supported(&self) -> bool {
        true
    }

    fn backend_mode(&self) -> &'static str {
        if self.raw_input_enabled {
            "hook_raw"
        } else {
            "hook"
        }
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        take_hook_dropped_event_count()
    }
}
