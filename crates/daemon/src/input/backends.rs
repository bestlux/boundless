use super::*;

#[cfg(any(test, windows))]
impl InputBackend for UnsupportedInteractiveInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        Err(anyhow::anyhow!(
            "interactive input unsupported from Windows service session 0"
        ))
    }
}

#[cfg(any(test, windows))]
impl InputCaptureBackend for UnsupportedInteractiveCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        Vec::new()
    }

    fn reset(&mut self) {}

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        Vec::new()
    }

    fn set_lock_active(&mut self, _active: bool) -> Result<bool> {
        Ok(false)
    }

    fn lock_supported(&self) -> bool {
        false
    }

    fn backend_mode(&self) -> &'static str {
        "service_session_unsupported"
    }
}

impl InputCaptureBackend for BrokerRelayCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        self.relay.drain_release_events()
    }

    fn reset(&mut self) {
        self.relay.reset_capture_stream();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(self.relay.drain_captured_events())
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        (0..self.relay.take_escape_unlock_count())
            .map(|_| CaptureControlAction::EscapeUnlock)
            .collect()
    }

    fn set_lock_active(&mut self, active: bool) -> Result<bool> {
        Ok(self.relay.set_desired_lock_active(active))
    }

    fn lock_supported(&self) -> bool {
        self.relay.lock_supported()
    }

    fn backend_mode(&self) -> &'static str {
        if self.relay.is_attached_fresh(Instant::now()) {
            crate::state::INPUT_BROKER_BACKEND_MODE
        } else {
            crate::state::SERVICE_SESSION_UNSUPPORTED_BACKEND_MODE
        }
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.relay.cursor_position()
    }

    fn virtual_screen_bounds(&self) -> Option<VirtualScreenBounds> {
        self.relay
            .virtual_bounds()
            .map(|(left, top, right, bottom)| VirtualScreenBounds {
                left,
                top,
                right,
                bottom,
            })
    }

    fn take_dropped_event_count(&mut self) -> u64 {
        self.relay.take_dropped_event_count()
    }
}

#[cfg(not(windows))]
impl InputBackend for NoopInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(windows))]
impl InputCaptureBackend for NoopCaptureBackend {
    fn drain_release_events(&mut self) -> Vec<InputEvent> {
        Vec::new()
    }

    fn reset(&mut self) {}

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        Vec::new()
    }

    fn set_lock_active(&mut self, _active: bool) -> Result<bool> {
        Ok(false)
    }

    fn lock_supported(&self) -> bool {
        false
    }

    fn backend_mode(&self) -> &'static str {
        "noop"
    }
}

#[cfg(windows)]
impl InputBackend for WindowsInputBackend {
    fn apply(&mut self, event: &InputEvent) -> Result<()> {
        let records = input_records_for_event(event);
        send_input_records(&records)
            .with_context(|| format!("SendInput failed for {}", input_event_kind(event)))
    }

    fn apply_frame(&mut self, events: &[InputEvent]) -> Result<()> {
        let mut records = Vec::new();
        for event in events {
            records.extend(input_records_for_event(event));
        }
        send_input_records(&records).context("SendInput failed for frame batch")
    }
}

#[cfg(windows)]
impl InputCaptureBackend for WindowsPollingCaptureBackend {
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
            .filter_map(|(vk, down)| if *down { Some(*vk) } else { None })
            .collect::<Vec<_>>();
        pressed_keys.sort_unstable();
        for vk in pressed_keys {
            if let Some(scan_code) = vk_to_scan_code(vk) {
                events.push(InputEvent::Key {
                    scan_code,
                    state: core_input::KeyState::Up,
                });
            }
        }

        events
    }

    fn reset(&mut self) {
        self.last_cursor = None;
        self.last_key_down.clear();
        self.last_button_down.clear();
    }

    fn poll_events(&mut self) -> Result<Vec<InputEvent>> {
        let mut events = Vec::new();

        if let Some((x, y)) = cursor_position()? {
            if let Some((last_x, last_y)) = self.last_cursor {
                let dx = x - last_x;
                let dy = y - last_y;
                if dx != 0 || dy != 0 {
                    events.push(InputEvent::MouseMove { dx, dy });
                }
            }
            self.last_cursor = Some((x, y));
        }

        for (vk, button) in mouse_button_virtual_keys() {
            let down = is_virtual_key_down(vk);
            if let Some(last) = self.last_button_down.insert(vk, down)
                && last != down
            {
                events.push(InputEvent::MouseButton {
                    button,
                    state: if down {
                        core_input::KeyState::Down
                    } else {
                        core_input::KeyState::Up
                    },
                });
            }
        }

        for &vk in captured_key_virtual_keys() {
            let down = is_virtual_key_down(vk);
            if let Some(last) = self.last_key_down.insert(vk, down)
                && last != down
                && let Some(scan_code) = vk_to_scan_code(vk)
            {
                events.push(InputEvent::Key {
                    scan_code,
                    state: if down {
                        core_input::KeyState::Down
                    } else {
                        core_input::KeyState::Up
                    },
                });
            }
        }

        Ok(events)
    }

    fn drain_control_actions(&mut self) -> Vec<CaptureControlAction> {
        Vec::new()
    }

    fn set_lock_active(&mut self, _active: bool) -> Result<bool> {
        Ok(false)
    }

    fn lock_supported(&self) -> bool {
        false
    }

    fn backend_mode(&self) -> &'static str {
        "polling"
    }

    fn cursor_position(&self) -> Option<(i32, i32)> {
        self.last_cursor
    }
}
