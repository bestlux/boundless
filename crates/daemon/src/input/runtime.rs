use super::*;

#[derive(Debug, Default)]
pub(super) struct InjectDrainOutcome {
    pub(super) continue_immediately: bool,
}

pub(super) async fn run(state: AppState, mode: InputRuntimeMode) -> Result<()> {
    let mut inject_backend = input_backend(mode);
    let mut capture_backend = input_capture_backend(&state, mode);
    state
        .set_input_lock_runtime(false, capture_backend.lock_supported())
        .await;
    let mut capture_backend_mode = capture_backend.backend_mode();
    state
        .set_input_capture_backend_mode(capture_backend_mode)
        .await;
    record_local_input_runtime_event(
        &state,
        "input_capture_backend_mode",
        capture_backend_mode,
        "none",
    )
    .await;
    let capture_wake = state.input_capture_wake_signal();
    let inject_wake = state.input_inject_wake_signal();
    let mut safety_ticker = time::interval(INPUT_RUNTIME_SAFETY_TICK);
    safety_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_capture_target: Option<String> = None;
    let mut edge_switch_state = EdgeSwitchState::default();

    capture_and_queue_outgoing_frames(
        &state,
        capture_backend.as_mut(),
        &mut last_capture_target,
        &mut edge_switch_state,
    )
    .await;

    loop {
        let mut run_capture = capture_wake.take_pending();
        let mut run_inject = inject_wake.take_pending();
        let next_retry_at = if state.input_broker_route_active() {
            // Retry pacing belongs to the broker exchange while it owns the
            // inject queue; keeping the deadline here would spin the loop.
            None
        } else {
            state.next_pending_inject_retry_at().await
        };

        if !run_capture && !run_inject {
            let capture_notified = capture_wake.notified();
            let inject_notified = inject_wake.notified();
            tokio::pin!(capture_notified);
            tokio::pin!(inject_notified);

            if let Some(deadline) = next_retry_at {
                let retry_sleep = time::sleep_until(deadline.into());
                tokio::pin!(retry_sleep);
                tokio::select! {
                    _ = &mut capture_notified => {
                        run_capture = capture_wake.take_pending();
                    }
                    _ = &mut inject_notified => {
                        run_inject = inject_wake.take_pending();
                    }
                    _ = &mut retry_sleep => {
                        run_inject = true;
                        record_local_input_runtime_event(
                            &state,
                            "input_runtime_wake",
                            "channel=input_inject source=retry_deadline",
                            "none",
                        )
                        .await;
                    }
                    _ = safety_ticker.tick() => {
                        run_capture = true;
                        run_inject = true;
                        record_local_input_runtime_event(
                            &state,
                            "input_runtime_wake",
                            "channel=all source=safety_tick",
                            "none",
                        )
                        .await;
                    }
                }
            } else {
                tokio::select! {
                    _ = &mut capture_notified => {
                        run_capture = capture_wake.take_pending();
                    }
                    _ = &mut inject_notified => {
                        run_inject = inject_wake.take_pending();
                    }
                    _ = safety_ticker.tick() => {
                        run_capture = true;
                        run_inject = true;
                        record_local_input_runtime_event(
                            &state,
                            "input_runtime_wake",
                            "channel=all source=safety_tick",
                            "none",
                        )
                        .await;
                    }
                }
            }
        }

        if run_capture {
            // Refresh the reported backend mode before draining events so a
            // broker attach/detach transition takes effect for the same
            // capture pass instead of dropping its first event batch.
            let next_mode = capture_backend.backend_mode();
            if next_mode != capture_backend_mode {
                capture_backend_mode = next_mode;
                state
                    .set_input_capture_backend_mode(capture_backend_mode)
                    .await;
                record_local_input_runtime_event(
                    &state,
                    "input_capture_backend_mode",
                    capture_backend_mode,
                    "none",
                )
                .await;
            }
            capture_and_queue_outgoing_frames(
                &state,
                capture_backend.as_mut(),
                &mut last_capture_target,
                &mut edge_switch_state,
            )
            .await;
        }

        if run_inject {
            if state.input_broker_route_active() {
                // Incoming frames stay queued for the attached user-session
                // broker, which drains them through the control-plane
                // exchange and injects in the interactive session.
                continue;
            }
            let outcome = drain_pending_inject_frames(&state, inject_backend.as_mut()).await;
            if outcome.continue_immediately {
                continue;
            }
        }
    }
}

pub(super) async fn drain_pending_inject_frames(
    state: &AppState,
    backend: &mut dyn InputBackend,
) -> InjectDrainOutcome {
    let frames = state
        .dequeue_pending_inject_input_frames_up_to(INPUT_INJECT_MAX_FRAMES_PER_WAKE)
        .await;
    if frames.is_empty() {
        return InjectDrainOutcome::default();
    }

    let started = std::time::Instant::now();
    let mut deferred_frames = Vec::new();
    let mut preserve_deferred_front = false;
    let mut processed = 0usize;
    let mut remaining = frames.into_iter();
    while let Some(mut frame) = remaining.next() {
        if processed >= INPUT_INJECT_MAX_FRAMES_PER_WAKE
            || started.elapsed() >= INPUT_INJECT_WORK_QUANTUM
        {
            deferred_frames.push(frame);
            deferred_frames.extend(remaining);
            break;
        }

        if frame
            .next_retry_at
            .is_some_and(|next| std::time::Instant::now() < next)
        {
            deferred_frames.push(frame);
            deferred_frames.extend(remaining);
            preserve_deferred_front = true;
            break;
        }

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

                let now_ms = Utc::now().timestamp_millis();
                let frame_age_ms = now_ms.saturating_sub(frame.capture_timestamp_unix_ms);
                if frame.retry_count >= INPUT_INJECT_MAX_RETRIES
                    || frame_age_ms >= INPUT_INJECT_MAX_AGE_MS
                {
                    state
                        .record_input_inject_dropped_permanent(
                            &frame.peer_id,
                            frame.sequence,
                            frame.events.len(),
                            frame.timing(),
                            if frame.retry_count >= INPUT_INJECT_MAX_RETRIES {
                                "retry_limit"
                            } else {
                                "age_limit"
                            },
                            &message,
                        )
                        .await;
                    continue;
                }

                frame.retry_count = frame.retry_count.saturating_add(1);
                let exponent = u32::from(frame.retry_count.saturating_sub(1)).min(8);
                let backoff_ms = (INPUT_INJECT_RETRY_BASE_BACKOFF_MS
                    .saturating_mul(1u64 << exponent))
                .min(INPUT_INJECT_RETRY_MAX_BACKOFF_MS);
                frame.next_retry_at =
                    Some(std::time::Instant::now() + Duration::from_millis(backoff_ms));
                state
                    .record_input_inject_retry_scheduled(
                        &frame.peer_id,
                        frame.sequence,
                        frame.retry_count,
                        backoff_ms,
                        frame.events.len(),
                        frame.timing(),
                    )
                    .await;
                deferred_frames.push(frame);
                deferred_frames.extend(remaining);
                preserve_deferred_front = true;
                break;
            }
        }
        processed += 1;
    }

    if !deferred_frames.is_empty() {
        if preserve_deferred_front {
            state
                .requeue_pending_inject_input_frames_front(deferred_frames)
                .await;
        } else {
            state
                .requeue_pending_inject_input_frames_back(deferred_frames)
                .await;
        }
    }

    let now = std::time::Instant::now();
    let continue_immediately = state.has_pending_inject_input_frames().await
        && state
            .next_pending_inject_retry_at()
            .await
            .is_none_or(|next| next <= now);
    InjectDrainOutcome {
        continue_immediately,
    }
}

pub(super) fn apply_frame(
    backend: &mut dyn InputBackend,
    frame: &PendingInjectInputFrame,
) -> Result<()> {
    backend.apply_frame(&frame.events)
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
    let dropped_event_count = backend.take_dropped_event_count();
    if dropped_event_count > 0 {
        record_local_input_runtime_event(
            state,
            "input_hook_queue_dropped",
            &format!("dropped_events={dropped_event_count}"),
            "none",
        )
        .await;
    }
    if !events.is_empty() {
        state.note_real_local_input_activity().await;
    }
    let cursor_position = backend.cursor_position();
    let screen_bounds = backend
        .virtual_screen_bounds()
        .or_else(local_virtual_screen_bounds);

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
            edge_switch_state.suppress_until_instant = Some(
                std::time::Instant::now()
                    + Duration::from_millis(ESCAPE_EDGE_RECAPTURE_SUPPRESS_MS),
            );
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

    let replay_events = if edge_switch_state.suppress_until_instant.is_some() {
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
    if !should_record_local_input_runtime_event(kind, detail) {
        return;
    }

    state.record_transport_event(TransportEventRecord {
        timestamp: Utc::now(),
        direction: "local".to_string(),
        kind: kind.to_string(),
        peer_id: peer_id.to_string(),
        detail: detail.to_string(),
        size_bytes: 0,
    });
}

pub(super) fn should_record_local_input_runtime_event(kind: &str, detail: &str) -> bool {
    !(kind == "input_runtime_wake"
        && detail
            .split_whitespace()
            .any(|part| part == "source=safety_tick"))
}
