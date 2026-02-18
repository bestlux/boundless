use super::*;

pub(super) async fn run(state: AppState) -> Result<()> {
    let mut inject_backend = input_backend();
    let mut capture_backend = input_capture_backend();
    state
        .set_input_lock_runtime(false, capture_backend.lock_supported())
        .await;
    let mut capture_backend_mode = capture_backend.backend_mode();
    record_local_input_runtime_event(
        &state,
        "input_capture_backend_mode",
        capture_backend_mode,
        "none",
    )
    .await;
    let mut inject_ticker = time::interval(INPUT_TICK);
    let mut capture_ticker = time::interval(INPUT_CAPTURE_TICK);
    let mut last_capture_target: Option<String> = None;
    let mut edge_switch_state = EdgeSwitchState::default();

    loop {
        tokio::select! {
            _ = inject_ticker.tick() => {
                drain_pending_inject_frames(&state, inject_backend.as_mut()).await;
            }
            _ = capture_ticker.tick() => {
                capture_and_queue_outgoing_frames(
                    &state,
                    capture_backend.as_mut(),
                    &mut last_capture_target,
                    &mut edge_switch_state,
                )
                .await;
                let next_mode = capture_backend.backend_mode();
                if next_mode != capture_backend_mode {
                    capture_backend_mode = next_mode;
                    record_local_input_runtime_event(
                        &state,
                        "input_capture_backend_mode",
                        capture_backend_mode,
                        "none",
                    )
                    .await;
                }
            }
        }
    }
}

pub(super) async fn drain_pending_inject_frames(state: &AppState, backend: &mut dyn InputBackend) {
    while let Some(frame) = state.dequeue_pending_inject_input_frame().await {
        if !state.input_injection_allowed_for_peer(&frame.peer_id).await {
            state
                .record_input_inject_skipped(
                    &frame.peer_id,
                    frame.sequence,
                    frame.events.len(),
                    frame.timing(),
                    "owner_or_feature_changed",
                )
                .await;
            continue;
        }

        match apply_frame(backend, &frame) {
            Ok(()) => {
                state
                    .record_input_inject_applied(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                    )
                    .await;
            }
            Err(error) => {
                let message = format!("{error:#}");
                state
                    .record_input_inject_failed(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                        &message,
                    )
                    .await;
                state.requeue_pending_inject_input_frame_front(frame).await;
                break;
            }
        }
    }
}

pub(super) fn apply_frame(
    backend: &mut dyn InputBackend,
    frame: &PendingInjectInputFrame,
) -> Result<()> {
    for event in &frame.events {
        backend.apply(event)?;
    }
    Ok(())
}

pub(super) async fn capture_and_queue_outgoing_frames(
    state: &AppState,
    backend: &mut dyn InputCaptureBackend,
    last_capture_target: &mut Option<String>,
    edge_switch_state: &mut EdgeSwitchState,
) {
    let mut capture_target = state.active_input_capture_target().await;
    sync_local_input_lock(state, backend, capture_target.is_some()).await;

    if &capture_target != last_capture_target {
        if let Some(previous_target) = last_capture_target.as_deref() {
            let release_events = backend.drain_release_events();
            if !release_events.is_empty() {
                for chunk in release_events.chunks(MAX_EVENTS_PER_FRAME) {
                    if let Err(error) = state
                        .queue_input_events(previous_target, chunk.to_vec())
                        .await
                    {
                        warn!(
                            peer_id = %previous_target,
                            error = ?error,
                            "failed to queue synthetic release events for previous capture target"
                        );
                        break;
                    }
                }
            }
        }

        backend.reset();
        *last_capture_target = capture_target.clone();
    }

    let events = match backend.poll_events() {
        Ok(events) => events,
        Err(error) => {
            warn!(error = ?error, "input capture poll failed");
            Vec::new()
        }
    };
    let cursor_position = backend.cursor_position();
    let screen_bounds = local_virtual_screen_bounds();

    let mut escape_triggered = false;
    for action in backend.drain_control_actions() {
        if !matches!(action, CaptureControlAction::EscapeUnlock) {
            continue;
        }

        if capture_target.is_some() {
            state.clear_input_capture_target().await;
            record_local_input_runtime_event(
                state,
                "input_escape_triggered",
                "double_ctrl",
                "none",
            )
            .await;
            edge_switch_state.last_direction = None;
            edge_switch_state.x_pressure = 0;
            edge_switch_state.y_pressure = 0;
            edge_switch_state.suppress_until_unix_ms =
                Some(unix_now_ms().saturating_add(ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS));
            capture_target = None;
            sync_local_input_lock(state, backend, false).await;
            escape_triggered = true;
        }
    }

    let pre_handoff_target = capture_target;
    if let Some(peer_id) = pre_handoff_target.as_deref()
        && !events.is_empty()
    {
        for chunk in events.chunks(MAX_EVENTS_PER_FRAME) {
            if let Err(error) = state.queue_input_events(peer_id, chunk.to_vec()).await {
                warn!(
                    peer_id = %peer_id,
                    error = ?error,
                    "failed to queue captured local input frame"
                );
                break;
            }
        }
    }

    if !escape_triggered {
        maybe_handoff_capture_target_from_motion(
            state,
            &events,
            pre_handoff_target.as_deref(),
            edge_switch_state,
            cursor_position,
            screen_bounds,
        )
        .await;
    }

    let post_handoff_target = state.active_input_capture_target().await;
    sync_local_input_lock(state, backend, post_handoff_target.is_some()).await;

    let (Some(peer_id), None) = (
        post_handoff_target.as_deref(),
        pre_handoff_target.as_deref(),
    ) else {
        return;
    };

    let replay_events = if edge_switch_state.suppress_until_unix_ms.is_some() {
        filter_edge_start_replay_events(&events)
    } else {
        events
    };

    if !replay_events.is_empty() {
        for chunk in replay_events.chunks(MAX_EVENTS_PER_FRAME) {
            if let Err(error) = state.queue_input_events(peer_id, chunk.to_vec()).await {
                warn!(
                    peer_id = %peer_id,
                    error = ?error,
                    "failed to queue captured local input frame after local edge-start handoff"
                );
                break;
            }
        }
    }
}

async fn sync_local_input_lock(
    state: &AppState,
    backend: &mut dyn InputCaptureBackend,
    should_lock: bool,
) {
    let supported = backend.lock_supported();
    let active = match backend.set_lock_active(should_lock) {
        Ok(active) => active,
        Err(error) => {
            warn!(error = ?error, should_lock, "failed to update local input lock state");
            false
        }
    };

    let (last_active, last_supported) = state.input_lock_runtime().await;
    if last_active != active {
        let kind = if active {
            "input_lock_engaged"
        } else {
            "input_lock_released"
        };
        let detail = if should_lock && !active {
            "requested=true applied=false".to_string()
        } else {
            format!("requested={should_lock} applied={active}")
        };
        record_local_input_runtime_event(state, kind, &detail, "none").await;
    }
    if last_active != active || last_supported != supported {
        state.set_input_lock_runtime(active, supported).await;
    }
}

pub(super) async fn record_local_input_runtime_event(
    state: &AppState,
    kind: &str,
    detail: &str,
    peer_id: &str,
) {
    state
        .record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: kind.to_string(),
            peer_id: peer_id.to_string(),
            detail: detail.to_string(),
            size_bytes: 0,
        })
        .await;
}
