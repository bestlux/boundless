use core_input::{EasyMouseMode, InputEvent, SwitchDirection};
use tracing::{info, warn};

use crate::state::{AppState, CaptureHandoffTarget};

use super::{
    EDGE_POSITION_TOLERANCE_PX, EDGE_PRESSURE_THRESHOLD,
    EDGE_REMOTE_PRESSURE_THRESHOLD_DENOMINATOR, EDGE_REMOTE_PRESSURE_THRESHOLD_MAX,
    EDGE_REMOTE_PRESSURE_THRESHOLD_NUMERATOR, EDGE_SWITCH_POST_HANDOFF_SUPPRESS_MS,
    EdgeSwitchState, VirtualScreenBounds, record_local_input_runtime_event,
};

#[cfg(all(windows, not(test)))]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub(super) fn filter_edge_start_replay_events(events: &[InputEvent]) -> Vec<InputEvent> {
    events
        .iter()
        .filter(|event| !matches!(event, InputEvent::MouseMove { .. }))
        .cloned()
        .collect()
}

pub(super) fn edge_switch_direction_from_motion(
    events: &[InputEvent],
    state: &mut EdgeSwitchState,
    wrap_mouse: bool,
    current_target: Option<&str>,
    cursor_position: Option<(i32, i32)>,
    screen_bounds: Option<VirtualScreenBounds>,
) -> Option<SwitchDirection> {
    let pressure_threshold = edge_switch_pressure_threshold(current_target, screen_bounds);
    let pressure_limit = pressure_threshold.saturating_mul(2);

    let now_ms = unix_now_ms();
    if state
        .suppress_until_unix_ms
        .is_some_and(|until| now_ms < until)
    {
        state.last_direction = None;
        state.x_pressure = 0;
        state.y_pressure = 0;
        return None;
    }
    state.suppress_until_unix_ms = None;

    let mut batch_dx: i32 = 0;
    let mut batch_dy: i32 = 0;
    for event in events {
        let InputEvent::MouseMove { dx, dy } = event else {
            continue;
        };

        batch_dx = batch_dx.saturating_add(*dx);
        batch_dy = batch_dy.saturating_add(*dy);

        if *dx != 0 {
            if state.x_pressure.signum() != dx.signum() {
                state.x_pressure = 0;
            }
            state.x_pressure = state
                .x_pressure
                .saturating_add(*dx)
                .clamp(-pressure_limit, pressure_limit);
        }

        if *dy != 0 {
            if state.y_pressure.signum() != dy.signum() {
                state.y_pressure = 0;
            }
            state.y_pressure = state
                .y_pressure
                .saturating_add(*dy)
                .clamp(-pressure_limit, pressure_limit);
        }
    }

    if current_target.is_none() {
        let direction = batch_direction(batch_dx, batch_dy, wrap_mouse)?;
        if !is_local_handoff_edge(direction, cursor_position, screen_bounds) {
            return None;
        }
        return Some(direction);
    }

    let x_abs = state.x_pressure.abs();
    let y_abs = state.y_pressure.abs();

    let direction = if x_abs >= pressure_threshold && x_abs >= y_abs {
        Some(if state.x_pressure < 0 {
            SwitchDirection::Left
        } else {
            SwitchDirection::Right
        })
    } else if wrap_mouse && y_abs >= pressure_threshold && y_abs >= x_abs {
        Some(if state.y_pressure < 0 {
            SwitchDirection::Up
        } else {
            SwitchDirection::Down
        })
    } else {
        None
    };

    let direction = direction?;

    if current_target.is_none() && !is_local_handoff_edge(direction, cursor_position, screen_bounds)
    {
        return None;
    }

    Some(direction)
}

fn edge_switch_pressure_threshold(
    current_target: Option<&str>,
    screen_bounds: Option<VirtualScreenBounds>,
) -> i32 {
    if current_target.is_none() {
        return EDGE_PRESSURE_THRESHOLD;
    }

    let Some(bounds) = screen_bounds else {
        return EDGE_PRESSURE_THRESHOLD;
    };

    let width = bounds
        .right
        .saturating_sub(bounds.left)
        .saturating_add(1)
        .max(0);
    let scaled = width.saturating_mul(EDGE_REMOTE_PRESSURE_THRESHOLD_NUMERATOR)
        / EDGE_REMOTE_PRESSURE_THRESHOLD_DENOMINATOR;
    scaled.clamp(EDGE_PRESSURE_THRESHOLD, EDGE_REMOTE_PRESSURE_THRESHOLD_MAX)
}

fn batch_direction(dx: i32, dy: i32, wrap_mouse: bool) -> Option<SwitchDirection> {
    let x_abs = dx.abs();
    let y_abs = dy.abs();
    if x_abs == 0 && y_abs == 0 {
        return None;
    }

    if x_abs >= y_abs {
        return Some(if dx < 0 {
            SwitchDirection::Left
        } else {
            SwitchDirection::Right
        });
    }

    if wrap_mouse {
        return Some(if dy < 0 {
            SwitchDirection::Up
        } else {
            SwitchDirection::Down
        });
    }

    None
}

pub(super) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) async fn maybe_handoff_capture_target_from_motion(
    state: &AppState,
    events: &[InputEvent],
    current_target: Option<&str>,
    edge_switch_state: &mut EdgeSwitchState,
    cursor_position: Option<(i32, i32)>,
    screen_bounds: Option<VirtualScreenBounds>,
) {
    let (mode, wrap_mouse) = state.edge_switch_policy().await;
    if !matches!(mode, EasyMouseMode::Enable) {
        edge_switch_state.last_direction = None;
        edge_switch_state.x_pressure = 0;
        edge_switch_state.y_pressure = 0;
        return;
    }

    let direction = edge_switch_direction_from_motion(
        events,
        edge_switch_state,
        wrap_mouse,
        current_target,
        cursor_position,
        screen_bounds,
    );
    let Some(direction) = direction else {
        edge_switch_state.last_direction = None;
        return;
    };

    if edge_switch_state.last_direction == Some(direction) {
        return;
    }
    edge_switch_state.last_direction = Some(direction);

    let Some(next_target) = state
        .capture_handoff_target_for_direction(current_target, direction)
        .await
    else {
        return;
    };

    match next_target {
        CaptureHandoffTarget::Peer(next_peer_id) => {
            if current_target == Some(next_peer_id.as_str()) {
                return;
            }
            match state.set_input_capture_target(Some(&next_peer_id)).await {
                Ok(Some(peer_id)) => {
                    let previous_target = current_target.unwrap_or("local");
                    let anchor_event =
                        handoff_anchor_event(direction, cursor_position, screen_bounds);
                    if let Err(error) = state.queue_input_events(&peer_id, vec![anchor_event]).await
                    {
                        warn!(
                            peer_id = %peer_id,
                            error = ?error,
                            "failed to queue handoff cursor anchor event"
                        );
                    }
                    info!(
                        direction = ?direction,
                        previous_target = %previous_target,
                        next_target = %peer_id,
                        "edge switch capture handoff applied"
                    );
                    record_local_input_runtime_event(
                        state,
                        "input_handoff",
                        &format!("direction={direction:?} from={previous_target} to={peer_id}"),
                        &peer_id,
                    )
                    .await;
                    edge_switch_state.x_pressure = 0;
                    edge_switch_state.y_pressure = 0;
                    edge_switch_state.suppress_until_unix_ms =
                        Some(unix_now_ms().saturating_add(EDGE_SWITCH_POST_HANDOFF_SUPPRESS_MS));
                }
                Ok(None) => {}
                Err(error) => {
                    let previous_target = current_target.unwrap_or("local");
                    warn!(
                        direction = ?direction,
                        previous_target = %previous_target,
                        next_target = %next_peer_id,
                        error = ?error,
                        "failed to apply edge switch capture handoff"
                    );
                }
            }
        }
        CaptureHandoffTarget::Local => {
            if current_target.is_none() {
                return;
            }
            let previous_target = current_target.unwrap_or("local");
            state.clear_input_capture_target().await;
            info!(
                direction = ?direction,
                previous_target = %previous_target,
                "edge switch capture handoff returned to local"
            );
            record_local_input_runtime_event(
                state,
                "input_handoff",
                &format!("direction={direction:?} from={previous_target} to=local"),
                "none",
            )
            .await;
            edge_switch_state.x_pressure = 0;
            edge_switch_state.y_pressure = 0;
            edge_switch_state.suppress_until_unix_ms =
                Some(unix_now_ms().saturating_add(EDGE_SWITCH_POST_HANDOFF_SUPPRESS_MS));
        }
    }
}

fn is_local_handoff_edge(
    direction: SwitchDirection,
    cursor_position: Option<(i32, i32)>,
    screen_bounds: Option<VirtualScreenBounds>,
) -> bool {
    let Some(bounds) = screen_bounds else {
        return true;
    };
    let Some((x, y)) = cursor_position else {
        return false;
    };

    match direction {
        SwitchDirection::Left => x <= bounds.left.saturating_add(EDGE_POSITION_TOLERANCE_PX),
        SwitchDirection::Right => x >= bounds.right.saturating_sub(EDGE_POSITION_TOLERANCE_PX),
        SwitchDirection::Up => y <= bounds.top.saturating_add(EDGE_POSITION_TOLERANCE_PX),
        SwitchDirection::Down => y >= bounds.bottom.saturating_sub(EDGE_POSITION_TOLERANCE_PX),
    }
}

pub(super) fn handoff_anchor_event(
    direction: SwitchDirection,
    cursor_position: Option<(i32, i32)>,
    screen_bounds: Option<VirtualScreenBounds>,
) -> InputEvent {
    let center = u16::MAX / 2;
    let x_axis = normalize_cursor_axis(
        cursor_position.map(|(x, _)| x),
        screen_bounds.map(|b| (b.left, b.right)),
    )
    .unwrap_or(center);
    let y_axis = normalize_cursor_axis(
        cursor_position.map(|(_, y)| y),
        screen_bounds.map(|b| (b.top, b.bottom)),
    )
    .unwrap_or(center);

    let (x_norm, y_norm) = match direction {
        SwitchDirection::Left => (u16::MAX, y_axis),
        SwitchDirection::Right => (0, y_axis),
        SwitchDirection::Up => (x_axis, u16::MAX),
        SwitchDirection::Down => (x_axis, 0),
    };

    InputEvent::MouseMoveAbsolute { x_norm, y_norm }
}

fn normalize_cursor_axis(value: Option<i32>, bounds: Option<(i32, i32)>) -> Option<u16> {
    let value = value?;
    let (start, end) = bounds?;
    let span = end.saturating_sub(start);
    if span <= 0 {
        return None;
    }

    let clamped = value.clamp(start, end).saturating_sub(start) as u64;
    let norm = (clamped.saturating_mul(u16::MAX as u64) / span as u64) as u16;
    Some(norm)
}

#[cfg(all(windows, not(test)))]
pub(super) fn local_virtual_screen_bounds() -> Option<VirtualScreenBounds> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return None;
    }

    Some(VirtualScreenBounds {
        left,
        top,
        right: left.saturating_add(width.saturating_sub(1)),
        bottom: top.saturating_add(height.saturating_sub(1)),
    })
}

#[cfg(any(not(windows), test))]
pub(super) fn local_virtual_screen_bounds() -> Option<VirtualScreenBounds> {
    None
}
