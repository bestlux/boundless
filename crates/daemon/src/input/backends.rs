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
        let counts = self.relay.take_safety_unlock_counts();
        let mut actions = Vec::with_capacity(
            counts
                .escape
                .saturating_add(counts.lease_expired)
                .saturating_add(counts.detector_unavailable) as usize,
        );
        actions.extend((0..counts.escape).map(|_| CaptureControlAction::Escape));
        actions.extend((0..counts.lease_expired).map(|_| CaptureControlAction::LeaseExpired));
        actions.extend(
            (0..counts.detector_unavailable).map(|_| CaptureControlAction::DetectorUnavailable),
        );
        actions
    }

    fn poll_handoff_probe(&mut self) -> Option<InputEvent> {
        self.relay.drain_handoff_probe()
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
        self.input
            .send_events(std::slice::from_ref(event))
            .with_context(|| format!("SendInput failed for {}", input_event_kind(event)))
    }

    fn apply_frame(&mut self, events: &[InputEvent]) -> Result<()> {
        self.input
            .send_events(events)
            .context("SendInput failed for frame batch")
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
            .filter_map(
                |(vk, (down, semantics))| {
                    if *down { Some((*vk, *semantics)) } else { None }
                },
            )
            .collect::<Vec<_>>();
        pressed_keys.sort_unstable_by_key(|(vk, _)| *vk);
        for (vk, semantics) in pressed_keys {
            if let Some(scan_code) = vk_to_scan_code(vk) {
                events.push(InputEvent::Key {
                    scan_code,
                    state: core_input::KeyState::Up,
                    semantics,
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
            let prior = self.last_key_down.get(&vk).copied();
            let Some((was_down, _)) = prior else {
                self.last_key_down.insert(
                    vk,
                    (
                        down,
                        KeySemantics::Windows {
                            virtual_key: vk,
                            num_lock_on: self.num_lock_state.is_on(),
                        },
                    ),
                );
                continue;
            };
            if was_down == down {
                continue;
            }

            let num_lock_on = if vk == VK_NUMLOCK_CODE && down {
                self.num_lock_state.toggle()
            } else {
                self.num_lock_state.is_on()
            };
            let observed_semantics = KeySemantics::Windows {
                virtual_key: vk,
                num_lock_on,
            };
            let event_semantics = if down {
                observed_semantics
            } else {
                prior
                    .filter(|(pressed, _)| *pressed)
                    .map(|(_, semantics)| semantics)
                    .unwrap_or(observed_semantics)
            };
            self.last_key_down.insert(vk, (down, event_semantics));
            if let Some(scan_code) = vk_to_scan_code(vk) {
                events.push(InputEvent::Key {
                    scan_code,
                    state: if down {
                        core_input::KeyState::Down
                    } else {
                        core_input::KeyState::Up
                    },
                    semantics: event_semantics,
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

    fn windows_num_lock_state(&self) -> Option<WindowsNumLockState> {
        Some(self.num_lock_state.clone())
    }
}
