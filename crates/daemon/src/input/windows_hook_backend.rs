use super::*;

impl WindowsHookCaptureBackend {
    pub(super) fn new(state: &AppState) -> Result<Self> {
        let pump = HookInputPump::start({
            let state = state.clone();
            move |source| state.notify_input_capture_wake(source)
        })?;

        Ok(Self { pump })
    }
}

impl InputCaptureBackend for WindowsHookCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        self.pump.drain_release_events()
    }

    fn reset(&mut self) {
        self.pump.reset();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(self.pump.poll_events())
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        self.pump
            .drain_control_actions()
            .into_iter()
            .map(|action| match action {
                HookControlAction::EscapeUnlock => CaptureControlAction::EscapeUnlock,
            })
            .collect()
    }

    fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        self.pump.set_lock_active(active)
    }

    fn lock_supported(&self) -> bool {
        true
    }

    fn backend_mode(&self) -> &'static str {
        self.pump.backend_mode()
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.pump.cursor_position()
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        self.pump.take_dropped_event_count()
    }
}
