use super::*;

#[derive(Debug)]
struct PendingInjectEnqueueReport {
    enqueued: bool,
    depth: usize,
    dropped: Option<(PendingInjectInputFrame, &'static str)>,
    coalesced: Option<(u64, u64, usize)>,
    high_water: Option<usize>,
}

fn move_only_delta(events: &[InputEvent]) -> Option<(i32, i32)> {
    let mut dx = 0i32;
    let mut dy = 0i32;
    let mut saw_event = false;
    for event in events {
        let InputEvent::MouseMove {
            dx: event_dx,
            dy: event_dy,
        } = event
        else {
            return None;
        };
        dx = dx.saturating_add(*event_dx);
        dy = dy.saturating_add(*event_dy);
        saw_event = true;
    }

    saw_event.then_some((dx, dy))
}

fn try_coalesce_pending_inject_back(
    queue: &mut VecDeque<PendingInjectInputFrame>,
    newer: &PendingInjectInputFrame,
) -> Option<(u64, u64, usize)> {
    let last = queue.back_mut()?;
    if last.peer_id != newer.peer_id {
        return None;
    }

    let (older_dx, older_dy) = move_only_delta(&last.events)?;
    let (newer_dx, newer_dy) = move_only_delta(&newer.events)?;
    let merged_dx = older_dx.saturating_add(newer_dx);
    let merged_dy = older_dy.saturating_add(newer_dy);
    let older_sequence = last.sequence;
    let newer_sequence = newer.sequence;

    last.sequence = newer.sequence;
    last.capture_timestamp_unix_ms = newer.capture_timestamp_unix_ms;
    last.received_timestamp_unix_ms = newer.received_timestamp_unix_ms;
    last.queued_timestamp_unix_ms = newer.queued_timestamp_unix_ms;
    last.retry_count = newer.retry_count;
    last.next_retry_at = newer.next_retry_at;
    last.events = if merged_dx == 0 && merged_dy == 0 {
        Vec::new()
    } else {
        vec![InputEvent::MouseMove {
            dx: merged_dx,
            dy: merged_dy,
        }]
    };
    let merged_event_count = last.events.len();
    if merged_event_count == 0 {
        queue.pop_back();
    }
    Some((older_sequence, newer_sequence, merged_event_count))
}

fn remove_oldest_coalescible_pending_inject_frame(
    queue: &mut VecDeque<PendingInjectInputFrame>,
) -> Option<PendingInjectInputFrame> {
    let index = queue
        .iter()
        .position(|frame| move_only_delta(&frame.events).is_some())?;
    queue.remove(index)
}

impl AppState {
    async fn enqueue_pending_inject_input_frame(
        &self,
        frame: PendingInjectInputFrame,
    ) -> PendingInjectEnqueueReport {
        let mut queue = self.input.pending_inject_frames.write().await;
        let incoming_is_move_only = move_only_delta(&frame.events).is_some();
        if let Some((older_sequence, newer_sequence, merged_event_count)) =
            try_coalesce_pending_inject_back(&mut queue, &frame)
        {
            let depth = queue.len();
            let high_water = self.observe_pending_inject_high_water(depth);
            return PendingInjectEnqueueReport {
                enqueued: true,
                depth,
                dropped: None,
                coalesced: Some((older_sequence, newer_sequence, merged_event_count)),
                high_water,
            };
        }

        let mut maybe_frame = Some(frame);
        let mut dropped = None;
        if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
            dropped = remove_oldest_coalescible_pending_inject_frame(&mut queue)
                .map(|frame| (frame, "evict_oldest_move"));

            if dropped.is_none() {
                if incoming_is_move_only {
                    dropped = maybe_frame.take().map(|frame| (frame, "drop_new_move"));
                } else {
                    dropped = queue
                        .pop_front()
                        .map(|frame| (frame, "evict_oldest_fallback"));
                }
            }
        }

        let enqueued = maybe_frame.is_some();
        if let Some(frame) = maybe_frame.take() {
            queue.push_back(frame);
        }

        let depth = queue.len();
        let high_water = self.observe_pending_inject_high_water(depth);
        PendingInjectEnqueueReport {
            enqueued,
            depth,
            dropped,
            coalesced: None,
            high_water,
        }
    }

    pub(crate) async fn clear_pending_inject_input_frames_for_peer(&self, peer_id: &str) {
        let mut queue = self.input.pending_inject_frames.write().await;
        queue.retain(|frame| frame.peer_id != peer_id);
    }

    async fn record_input_inject_queued(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        depth: usize,
        timing: InputFrameTiming,
    ) {
        let capture_to_queue_ms = elapsed_ms(
            timing.capture_timestamp_unix_ms,
            timing.queued_timestamp_unix_ms,
        );
        let receive_to_queue_ms = elapsed_ms(
            timing.received_timestamp_unix_ms,
            timing.queued_timestamp_unix_ms,
        );
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_queued".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_depth={depth} capture_to_queue_ms={capture_to_queue_ms} receive_to_queue_ms={receive_to_queue_ms}"
            ),
            size_bytes: event_count as u64,
        });
    }

    async fn record_input_inject_dropped(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        capture_timestamp_unix_ms: i64,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_dropped".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} dropped_oldest capture_age_ms={}",
                elapsed_ms(capture_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn record_input_inject_skipped(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
        reason: &str,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_skipped".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} reason={reason} queue_wait_ms={} capture_to_skip_ms={} receive_to_skip_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn route_incoming_input_frame(
        &self,
        peer_id: &str,
        frame: InputFrame,
    ) -> Result<RouteDecision> {
        struct RecordingInputSink {
            events: Vec<InputEvent>,
        }
        impl InputSink for RecordingInputSink {
            fn apply(&mut self, event: &InputEvent) -> std::result::Result<(), String> {
                self.events.push(event.clone());
                Ok(())
            }
        }

        let mut sink = RecordingInputSink {
            events: Vec::with_capacity(frame.events.len()),
        };
        let received_timestamp_unix_ms = Utc::now().timestamp_millis();
        let mut decision = {
            let mut router = self.input.router.write().await;
            router
                .route_frame(&frame, &mut sink)
                .map_err(anyhow::Error::from)?
        };
        let mut auto_claimed_owner = false;

        if matches!(
            decision,
            RouteDecision::IgnoredNoOwner | RouteDecision::IgnoredWrongOwner { .. }
        ) {
            let mut retry_sink = RecordingInputSink {
                events: Vec::with_capacity(frame.events.len()),
            };
            let mut blocked_reason: Option<&'static str> = None;
            decision = {
                let mut router = self.input.router.write().await;
                let mut retried_decision = router
                    .route_frame(&frame, &mut retry_sink)
                    .map_err(anyhow::Error::from)?;
                if matches!(
                    retried_decision,
                    RouteDecision::IgnoredNoOwner | RouteDecision::IgnoredWrongOwner { .. }
                ) {
                    let (allow_auto_claim, block_reason) =
                        self.auto_claim_input_owner_allowed_now(&retried_decision);
                    if allow_auto_claim && router.claim_owner(peer_id, true) {
                        auto_claimed_owner = true;
                        retried_decision = router
                            .route_frame(&frame, &mut retry_sink)
                            .map_err(anyhow::Error::from)?;
                    } else if !allow_auto_claim {
                        blocked_reason = Some(block_reason);
                    }
                }
                retried_decision
            };
            sink = retry_sink;
            if auto_claimed_owner {
                self.note_input_owner_transition().await;
            } else if let Some(block_reason) = blocked_reason {
                self.record_input_owner_auto_claim_blocked(peer_id, frame.sequence, block_reason)
                    .await;
            }
        }

        if matches!(decision, RouteDecision::Applied { .. }) {
            let queued_timestamp_unix_ms = Utc::now().timestamp_millis();
            let timing = InputFrameTiming {
                capture_timestamp_unix_ms: frame.timestamp_unix_ms,
                received_timestamp_unix_ms,
                queued_timestamp_unix_ms,
            };
            let pending = PendingInjectInputFrame {
                peer_id: peer_id.to_string(),
                sequence: frame.sequence,
                capture_timestamp_unix_ms: timing.capture_timestamp_unix_ms,
                received_timestamp_unix_ms: timing.received_timestamp_unix_ms,
                queued_timestamp_unix_ms: timing.queued_timestamp_unix_ms,
                retry_count: 0,
                next_retry_at: None,
                events: sink.events,
            };
            let report = self.enqueue_pending_inject_input_frame(pending).await;
            if let Some((older_sequence, newer_sequence, merged_event_count)) = report.coalesced {
                self.record_input_queue_coalesced(
                    "inject",
                    peer_id,
                    older_sequence,
                    newer_sequence,
                    merged_event_count,
                );
            }
            if let Some(depth) = report.high_water {
                self.record_input_queue_high_water("inject", peer_id, depth);
            }
            if let Some((dropped, reason)) = report.dropped {
                self.record_input_queue_overflow_drop(
                    "inject",
                    &dropped.peer_id,
                    dropped.sequence,
                    reason,
                );
                self.record_input_inject_dropped(
                    &dropped.peer_id,
                    dropped.sequence,
                    dropped.events.len(),
                    dropped.capture_timestamp_unix_ms,
                )
                .await;
            }
            if report.enqueued {
                self.notify_input_inject_wake("incoming_frame");
                self.record_input_inject_queued(
                    peer_id,
                    frame.sequence,
                    frame.events.len(),
                    report.depth,
                    timing,
                )
                .await;
            }
        }

        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "input_frame".to_string(),
            peer_id: peer_id.to_string(),
            detail: describe_input_frame_decision(
                &decision,
                frame.sequence,
                frame.timestamp_unix_ms,
                received_timestamp_unix_ms,
                auto_claimed_owner,
            ),
            size_bytes: frame.events.len() as u64,
        });

        Ok(decision)
    }

    #[cfg(test)]
    pub async fn dequeue_pending_inject_input_frame(&self) -> Option<PendingInjectInputFrame> {
        let mut frames = self.dequeue_pending_inject_input_frames_up_to(1).await;
        frames.pop()
    }

    #[cfg(test)]
    pub async fn pending_inject_input_frame_count(&self) -> usize {
        self.input.pending_inject_frames.read().await.len()
    }

    pub async fn dequeue_pending_inject_input_frames_up_to(
        &self,
        max_frames: usize,
    ) -> Vec<PendingInjectInputFrame> {
        if max_frames == 0 {
            return Vec::new();
        }

        let mut queue = self.input.pending_inject_frames.write().await;
        let drain_count = queue.len().min(max_frames);
        queue.drain(..drain_count).collect()
    }

    pub async fn requeue_pending_inject_input_frames_back(
        &self,
        frames: Vec<PendingInjectInputFrame>,
    ) {
        if frames.is_empty() {
            return;
        }

        let mut queue = self.input.pending_inject_frames.write().await;
        for frame in frames {
            if let Some((older_sequence, newer_sequence, merged_event_count)) =
                try_coalesce_pending_inject_back(&mut queue, &frame)
            {
                self.record_input_queue_coalesced(
                    "inject",
                    "requeue",
                    older_sequence,
                    newer_sequence,
                    merged_event_count,
                );
                continue;
            }

            if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
                let dropped = remove_oldest_coalescible_pending_inject_frame(&mut queue)
                    .map(|frame| (frame, "evict_oldest_move"))
                    .or_else(|| {
                        queue
                            .pop_front()
                            .map(|frame| (frame, "evict_oldest_fallback"))
                    });
                if let Some((dropped, reason)) = dropped {
                    self.record_input_queue_overflow_drop(
                        "inject",
                        &dropped.peer_id,
                        dropped.sequence,
                        reason,
                    );
                }
            }
            queue.push_back(frame);
        }
        let depth = queue.len();
        drop(queue);
        if let Some(depth) = self.observe_pending_inject_high_water(depth) {
            self.record_input_queue_high_water("inject", "requeue", depth);
        }
        self.notify_input_inject_wake("retry_requeue");
    }

    pub async fn has_pending_inject_input_frames(&self) -> bool {
        !self.input.pending_inject_frames.read().await.is_empty()
    }

    pub async fn next_pending_inject_retry_at(&self) -> Option<Instant> {
        self.input
            .pending_inject_frames
            .read()
            .await
            .iter()
            .filter_map(|frame| frame.next_retry_at)
            .min()
    }

    fn observe_pending_inject_high_water(&self, depth: usize) -> Option<usize> {
        let mut current = self
            .input
            .pending_inject_high_water
            .load(std::sync::atomic::Ordering::Relaxed);
        while depth > current {
            match self.input.pending_inject_high_water.compare_exchange(
                current,
                depth,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return Some(depth),
                Err(observed) => current = observed,
            }
        }
        None
    }

    pub async fn record_input_inject_applied(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_applied".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_wait_ms={} capture_to_apply_ms={} receive_to_apply_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn record_input_inject_failed(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
        message: &str,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_failed".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_wait_ms={} capture_to_fail_ms={} receive_to_fail_ms={} {message}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn record_input_inject_retry_scheduled(
        &self,
        peer_id: &str,
        sequence: u64,
        retry_count: u8,
        backoff_ms: u64,
        event_count: usize,
        timing: InputFrameTiming,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_retry_scheduled".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} retry_count={retry_count} backoff_ms={backoff_ms} queue_wait_ms={} capture_to_retry_ms={} receive_to_retry_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn record_input_inject_dropped_permanent(
        &self,
        peer_id: &str,
        sequence: u64,
        event_count: usize,
        timing: InputFrameTiming,
        reason: &str,
        message: &str,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_inject_dropped_permanent".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} reason={reason} queue_wait_ms={} capture_to_drop_ms={} receive_to_drop_ms={} {message}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }

    pub async fn claim_input_owner(&self, peer_id: &str, force: bool) -> Result<bool> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        let claimed = self.input.router.write().await.claim_owner(peer_id, force);
        if claimed {
            self.note_input_owner_transition().await;
        }
        Ok(claimed)
    }

    pub async fn input_injection_allowed_for_peer(&self, peer_id: &str) -> bool {
        let router = self.input.router.read().await;
        router.is_enabled() && router.owner() == Some(peer_id)
    }

    pub async fn release_input_owner(&self, peer_id: &str) -> bool {
        let released = self.input.router.write().await.release_owner(peer_id);
        if released {
            self.note_input_owner_transition().await;
        }
        released
    }

    pub async fn note_input_owner_transition(&self) {
        *self.input.owner_last_changed_at.write().await = Some(std::time::Instant::now());
        self.notify_input_inject_wake("input_owner_changed");
    }

    pub async fn input_owner(&self) -> Option<String> {
        self.input
            .router
            .read()
            .await
            .owner()
            .map(|owner| owner.to_string())
    }

    pub async fn set_input_capture_target(&self, peer_id: Option<&str>) -> Result<Option<String>> {
        let next = match peer_id.map(str::trim) {
            Some("") | None => None,
            Some(peer_id) => {
                if self.get_peer(peer_id).await.is_none() {
                    anyhow::bail!("unknown peer {peer_id}");
                }
                Some(peer_id.to_string())
            }
        };

        let mut target = self.input.capture_target_peer_id.write().await;
        *target = next.clone();
        drop(target);
        self.notify_input_capture_wake("capture_target_changed");
        Ok(next)
    }

    pub async fn clear_input_capture_target(&self) {
        *self.input.capture_target_peer_id.write().await = None;
        self.notify_input_capture_wake("capture_target_cleared");
    }

    pub async fn input_capture_target(&self) -> Option<String> {
        self.input.capture_target_peer_id.read().await.clone()
    }

    pub async fn active_input_capture_target(&self) -> Option<String> {
        let target = self.input_capture_target().await?;
        let config = self.config.read().await;
        let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
        if !share_input_enabled {
            return None;
        }
        if config
            .peers
            .iter()
            .any(|peer| peer.peer_id == target && peer.connected)
        {
            Some(target)
        } else {
            None
        }
    }

    pub async fn set_input_lock_runtime(&self, active: bool, supported: bool) {
        *self.input.lock_active.write().await = active;
        *self.input.lock_supported.write().await = supported;
    }

    pub async fn input_lock_runtime(&self) -> (bool, bool) {
        let active = *self.input.lock_active.read().await;
        let supported = *self.input.lock_supported.read().await;
        (active, supported)
    }

    fn auto_claim_input_owner_allowed_now(&self, decision: &RouteDecision) -> (bool, &'static str) {
        match decision {
            RouteDecision::IgnoredNoOwner => (true, "no_owner"),
            RouteDecision::IgnoredWrongOwner { owner_peer_id } => {
                let owner_connected = self
                    .config
                    .try_read()
                    .ok()
                    .map(|config| {
                        config
                            .peers
                            .iter()
                            .any(|peer| peer.peer_id == *owner_peer_id && peer.connected)
                    })
                    .unwrap_or(true);
                if !owner_connected {
                    return (true, "owner_disconnected");
                }

                let cooldown = Duration::from_millis(INPUT_OWNER_AUTO_STEAL_COOLDOWN_MS);
                let cooldown_ready = self
                    .input
                    .owner_last_changed_at
                    .try_read()
                    .ok()
                    .map(|last_changed| {
                        last_changed
                            .as_ref()
                            .map(|last| last.elapsed() >= cooldown)
                            .unwrap_or(true)
                    })
                    .unwrap_or(false);

                if cooldown_ready {
                    (true, "cooldown_elapsed")
                } else {
                    (false, "cooldown_active")
                }
            }
            _ => (false, "not_eligible"),
        }
    }

    async fn record_input_owner_auto_claim_blocked(
        &self,
        peer_id: &str,
        sequence: u64,
        reason: &str,
    ) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_owner_auto_claim_blocked".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!("sequence={sequence} reason={reason}"),
            size_bytes: 0,
        });
    }
}
