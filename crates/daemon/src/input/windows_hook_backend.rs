use super::*;

impl WindowsHookCaptureBackend {
    pub(super) fn new(state: &AppState) -> Result<Self> {
        let mut pump = HookInputPump::start({
            let state = state.clone();
            move |source| state.notify_input_capture_wake(source)
        })?;
        enable_direct_input_lock_lease(&mut pump, DIRECT_INPUT_LOCK_LEASE)?;

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
                HookControlAction::EscapeUnlock
                | HookControlAction::LeaseExpiredUnlock
                | HookControlAction::DetectorUnavailableUnlock => {
                    CaptureControlAction::EscapeUnlock
                }
            })
            .collect()
    }

    fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        self.pump.set_lock_active(active)
    }

    fn safety_unlock_generation(&self) -> u64 {
        self.pump.safety_unlock_generation()
    }

    fn set_lock_active_if_safety_generation(
        &mut self,
        active: bool,
        expected_generation: u64,
    ) -> Result<bool> {
        self.pump
            .set_lock_active_if_safety_generation(active, expected_generation)
    }

    fn renew_lock_lease(&self) -> bool {
        self.pump.renew_lock_lease()
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

    fn windows_num_lock_state(&self) -> Option<WindowsNumLockState> {
        Some(self.pump.num_lock_state())
    }
}

fn enable_direct_input_lock_lease(pump: &mut HookInputPump, timeout: Duration) -> Result<()> {
    pump.enable_lock_lease(timeout)
}
