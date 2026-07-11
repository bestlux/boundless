//! Conversions between `core_input::InputEvent` and the compact
//! `BrokerInputEvent` proto used by the local user-session input broker
//! exchange. Shared by the daemon-side adapter and the tray broker host so
//! both sides agree on one encoding.

use core_input::{InputEvent, KeySemantics, KeyState, MouseButton};

use crate::boundless::v1::{
    BrokerInputEvent, BrokerKey, BrokerMouseButton, BrokerMouseMove, BrokerMouseMoveAbsolute,
    BrokerMouseWheel, BrokerWindowsKeySemantics, broker_input_event,
};

fn key_state(down: bool) -> KeyState {
    if down { KeyState::Down } else { KeyState::Up }
}

fn mouse_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

fn mouse_button_from_code(code: u32) -> Option<MouseButton> {
    Some(match code {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::X1,
        4 => MouseButton::X2,
        _ => return None,
    })
}

pub fn broker_event_from_input_event(event: &InputEvent) -> BrokerInputEvent {
    let event = match event {
        InputEvent::MouseMove { dx, dy } => {
            broker_input_event::Event::MouseMove(BrokerMouseMove { dx: *dx, dy: *dy })
        }
        InputEvent::MouseMoveAbsolute { x_norm, y_norm } => {
            broker_input_event::Event::MouseMoveAbsolute(BrokerMouseMoveAbsolute {
                x_norm: u32::from(*x_norm),
                y_norm: u32::from(*y_norm),
            })
        }
        InputEvent::MouseButton { button, state } => {
            broker_input_event::Event::MouseButton(BrokerMouseButton {
                button: mouse_button_code(*button),
                down: matches!(state, KeyState::Down),
            })
        }
        InputEvent::MouseWheel { delta_x, delta_y } => {
            broker_input_event::Event::MouseWheel(BrokerMouseWheel {
                delta_x: *delta_x,
                delta_y: *delta_y,
            })
        }
        InputEvent::Key {
            scan_code,
            state,
            semantics,
        } => broker_input_event::Event::Key(BrokerKey {
            scan_code: u32::from(*scan_code),
            down: matches!(state, KeyState::Down),
            windows_semantics: match semantics {
                KeySemantics::Physical => None,
                KeySemantics::Windows {
                    virtual_key,
                    num_lock_on,
                } => Some(BrokerWindowsKeySemantics {
                    virtual_key: u32::from(*virtual_key),
                    num_lock_on: *num_lock_on,
                }),
            },
        }),
    };

    BrokerInputEvent { event: Some(event) }
}

/// Decodes one broker event; returns `None` for empty, out-of-range, or
/// unknown-variant payloads instead of guessing (fail closed).
pub fn input_event_from_broker_event(event: &BrokerInputEvent) -> Option<InputEvent> {
    Some(match event.event.as_ref()? {
        broker_input_event::Event::MouseMove(mouse_move) => InputEvent::MouseMove {
            dx: mouse_move.dx,
            dy: mouse_move.dy,
        },
        broker_input_event::Event::MouseMoveAbsolute(absolute) => InputEvent::MouseMoveAbsolute {
            x_norm: u16::try_from(absolute.x_norm).ok()?,
            y_norm: u16::try_from(absolute.y_norm).ok()?,
        },
        broker_input_event::Event::MouseButton(button) => InputEvent::MouseButton {
            button: mouse_button_from_code(button.button)?,
            state: key_state(button.down),
        },
        broker_input_event::Event::MouseWheel(wheel) => InputEvent::MouseWheel {
            delta_x: wheel.delta_x,
            delta_y: wheel.delta_y,
        },
        broker_input_event::Event::Key(key) => InputEvent::Key {
            scan_code: u16::try_from(key.scan_code).ok()?,
            state: key_state(key.down),
            semantics: match key.windows_semantics.as_ref() {
                Some(semantics) => KeySemantics::Windows {
                    virtual_key: u16::try_from(semantics.virtual_key).ok()?,
                    num_lock_on: semantics.num_lock_on,
                },
                None => KeySemantics::Physical,
            },
        },
    })
}

pub fn broker_events_from_input_events(events: &[InputEvent]) -> Vec<BrokerInputEvent> {
    events.iter().map(broker_event_from_input_event).collect()
}

/// Decodes a broker event batch, dropping undecodable entries and reporting
/// how many were dropped so callers can surface truthful diagnostics.
pub fn input_events_from_broker_events(events: &[BrokerInputEvent]) -> (Vec<InputEvent>, usize) {
    let mut decoded = Vec::with_capacity(events.len());
    let mut dropped = 0usize;
    for event in events {
        match input_event_from_broker_event(event) {
            Some(event) => decoded.push(event),
            None => dropped += 1,
        }
    }
    (decoded, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_event_variant() {
        let events = vec![
            InputEvent::MouseMove { dx: -7, dy: 3 },
            InputEvent::MouseMoveAbsolute {
                x_norm: 0,
                y_norm: u16::MAX,
            },
            InputEvent::MouseButton {
                button: MouseButton::X2,
                state: KeyState::Down,
            },
            InputEvent::MouseWheel {
                delta_x: 120,
                delta_y: -120,
            },
            InputEvent::Key {
                scan_code: 0xE04D,
                state: KeyState::Up,
                semantics: KeySemantics::Windows {
                    virtual_key: 0x27,
                    num_lock_on: true,
                },
            },
        ];

        let encoded = broker_events_from_input_events(&events);
        let (decoded, dropped) = input_events_from_broker_events(&encoded);
        assert_eq!(decoded, events);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn rejects_malformed_events_instead_of_guessing() {
        let malformed = vec![
            BrokerInputEvent { event: None },
            BrokerInputEvent {
                event: Some(broker_input_event::Event::MouseButton(BrokerMouseButton {
                    button: 99,
                    down: true,
                })),
            },
            BrokerInputEvent {
                event: Some(broker_input_event::Event::Key(BrokerKey {
                    scan_code: u32::from(u16::MAX) + 1,
                    down: true,
                    windows_semantics: None,
                })),
            },
            BrokerInputEvent {
                event: Some(broker_input_event::Event::Key(BrokerKey {
                    scan_code: 0x4F,
                    down: true,
                    windows_semantics: Some(BrokerWindowsKeySemantics {
                        virtual_key: u32::from(u16::MAX) + 1,
                        num_lock_on: true,
                    }),
                })),
            },
        ];

        let (decoded, dropped) = input_events_from_broker_events(&malformed);
        assert!(decoded.is_empty());
        assert_eq!(dropped, 4);
    }

    #[test]
    fn preserves_signed_high_resolution_wheel_values() {
        for delta in [1, -1, 40, -40, 120, -120] {
            let event = InputEvent::MouseWheel {
                delta_x: delta,
                delta_y: -delta,
            };
            let encoded = broker_event_from_input_event(&event);
            assert_eq!(input_event_from_broker_event(&encoded), Some(event));
        }
    }
}
