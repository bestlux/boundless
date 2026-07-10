use super::*;

pub(super) fn input_events_to_wire(events: &[InputEvent]) -> Vec<WireInputEvent> {
    events.iter().map(input_event_to_wire).collect()
}

fn input_event_to_wire(event: &InputEvent) -> WireInputEvent {
    match event {
        InputEvent::MouseMove { dx, dy } => WireInputEvent::MouseMove { dx: *dx, dy: *dy },
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => WireInputEvent::MouseMoveAbsolute {
            x_norm: *x_norm,
            y_norm: *y_norm,
        },
        InputEvent::MouseButton { button, state } => WireInputEvent::MouseButton {
            button: match button {
                MouseButton::Left => WireMouseButton::Left,
                MouseButton::Right => WireMouseButton::Right,
                MouseButton::Middle => WireMouseButton::Middle,
                MouseButton::X1 => WireMouseButton::X1,
                MouseButton::X2 => WireMouseButton::X2,
            },
            state: match state {
                KeyState::Down => WireKeyState::Down,
                KeyState::Up => WireKeyState::Up,
            },
        },
        InputEvent::MouseWheel { delta_x, delta_y } => WireInputEvent::MouseWheel {
            delta_x: *delta_x,
            delta_y: *delta_y,
        },
        InputEvent::Key {
            scan_code,
            state,
            semantics,
        } => WireInputEvent::Key {
            scan_code: *scan_code,
            state: match state {
                KeyState::Down => WireKeyState::Down,
                KeyState::Up => WireKeyState::Up,
            },
            semantics: match semantics {
                KeySemantics::Physical => WireKeySemantics::Physical,
                KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } => WireKeySemantics::Windows {
                    virtual_key: *virtual_key,
                    num_lock_on: *num_lock_on,
                },
            },
        },
    }
}

pub(super) fn input_event_from_wire(event: WireInputEvent) -> InputEvent {
    match event {
        WireInputEvent::MouseMove { dx, dy } => InputEvent::MouseMove { dx, dy },
        WireInputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            InputEvent::MouseMoveAbsolute { x_norm, y_norm }
        }
        WireInputEvent::MouseButton { button, state } => InputEvent::MouseButton {
            button: match button {
                WireMouseButton::Left => MouseButton::Left,
                WireMouseButton::Right => MouseButton::Right,
                WireMouseButton::Middle => MouseButton::Middle,
                WireMouseButton::X1 => MouseButton::X1,
                WireMouseButton::X2 => MouseButton::X2,
            },
            state: match state {
                WireKeyState::Down => KeyState::Down,
                WireKeyState::Up => KeyState::Up,
            },
        },
        WireInputEvent::MouseWheel { delta_x, delta_y } => {
            InputEvent::MouseWheel { delta_x, delta_y }
        }
        WireInputEvent::Key {
            scan_code,
            state,
            semantics,
        } => InputEvent::Key {
            scan_code,
            state: match state {
                WireKeyState::Down => KeyState::Down,
                WireKeyState::Up => KeyState::Up,
            },
            semantics: match semantics {
                WireKeySemantics::Physical => KeySemantics::Physical,
                WireKeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } => KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                },
            },
        },
    }
}

pub(super) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_key_semantics_survive_wire_conversion() {
        let original = InputEvent::Key {
            scan_code: 0x4F,
            state: KeyState::Down,
            semantics: KeySemantics::Windows {
                virtual_key: 0x61,
                num_lock_on: true,
            },
        };

        let wire = input_event_to_wire(&original);
        assert_eq!(input_event_from_wire(wire), original);
    }
}
