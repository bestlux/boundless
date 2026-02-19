use super::*;

pub(super) fn protocol_supports_clipboard_image(protocol: ProtocolVersion) -> bool {
    protocol.as_tuple() >= PROTOCOL_CLIPBOARD_IMAGE_MIN.as_tuple()
}

pub(super) fn protocol_supports_input_anchor(protocol: ProtocolVersion) -> bool {
    protocol.as_tuple() >= PROTOCOL_INPUT_ANCHOR_MIN.as_tuple()
}

pub(super) fn protocol_supports_file_chunk_credit(protocol: ProtocolVersion) -> bool {
    protocol.as_tuple() >= PROTOCOL_FILE_CHUNK_CREDIT_MIN.as_tuple()
}

pub(super) fn input_events_to_wire_for_protocol(
    events: &[InputEvent],
    remote_protocol: ProtocolVersion,
) -> Vec<WireInputEvent> {
    events
        .iter()
        .filter_map(|event| {
            if matches!(event, InputEvent::MouseMoveAbsolute { .. })
                && !protocol_supports_input_anchor(remote_protocol)
            {
                return None;
            }
            Some(input_event_to_wire(event))
        })
        .collect()
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
        InputEvent::Key { scan_code, state } => WireInputEvent::Key {
            scan_code: *scan_code,
            state: match state {
                KeyState::Down => WireKeyState::Down,
                KeyState::Up => WireKeyState::Up,
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
        WireInputEvent::Key { scan_code, state } => InputEvent::Key {
            scan_code,
            state: match state {
                WireKeyState::Down => KeyState::Down,
                WireKeyState::Up => KeyState::Up,
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
