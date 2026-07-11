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
    if last.peer_id != newer.peer_id
        || last.authorization_generation != newer.authorization_generation
    {
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

fn remove_newest_coalescible_pending_inject_frame(
    queue: &mut VecDeque<PendingInjectInputFrame>,
) -> Option<PendingInjectInputFrame> {
    let index = queue
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, frame)| move_only_delta(&frame.events).is_some().then_some(index))?;
    queue.remove(index)
}

impl AppState {
    pub async fn queue_input_move(&self, peer_id: &str, dx: i32, dy: i32) -> Result<()> {
        self.queue_input_events(peer_id, vec![InputEvent::MouseMove { dx, dy }])
            .await
    }

    pub async fn queue_input_key(
        &self,
        peer_id: &str,
        scan_code: u16,
        key_state: KeyState,
    ) -> Result<()> {
        self.queue_input_events(
            peer_id,
            vec![InputEvent::Key {
                scan_code,
                state: key_state,
                semantics: core_input::KeySemantics::Physical,
            }],
        )
        .await
    }

    pub async fn queue_input_events(&self, peer_id: &str, events: Vec<InputEvent>) -> Result<()> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }
        if events.is_empty() {
            anyhow::bail!("input frame must include at least one event");
        }
        if events.len() > MAX_EVENTS_PER_FRAME {
            anyhow::bail!(
                "input frame event count exceeds limit: {} > {}",
                events.len(),
                MAX_EVENTS_PER_FRAME
            );
        }

        let sequence = {
            let mut sequences = self.input.control.sequence_by_peer.write().await;
            let entry = sequences.entry(peer_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };

        self.queue_outgoing_input_payload(
            peer_id,
            OutboundPayload::InputFrame {
                sequence,
                timestamp_unix_ms: Utc::now().timestamp_millis(),
                events,
            },
        )
        .await;
        Ok(())
    }

    async fn enqueue_pending_inject_input_frame(
        &self,
        frame: PendingInjectInputFrame,
    ) -> PendingInjectEnqueueReport {
        let mut queue = self.input.inject.pending_inject_frames.write().await;
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
        let mut queue = self.input.inject.pending_inject_frames.write().await;
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
        let (mut decision, mut accepted_authorization_generation) = {
            let mut authorization = self.input.control.authorization.write().await;
            let decision = authorization
                .route_frame(&frame, &mut sink)
                .map_err(anyhow::Error::from)?;
            let generation = matches!(decision, RouteDecision::Applied { .. })
                .then(|| authorization.generation());
            (decision, generation)
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
            let (retried_decision, retried_authorization_generation) = {
                let mut authorization = self.input.control.authorization.write().await;
                let mut retried_decision = authorization
                    .route_frame(&frame, &mut retry_sink)
                    .map_err(anyhow::Error::from)?;
                if matches!(
                    retried_decision,
                    RouteDecision::IgnoredNoOwner | RouteDecision::IgnoredWrongOwner { .. }
                ) {
                    let (allow_auto_claim, block_reason) = self.auto_claim_input_owner_allowed_now(
                        &retried_decision,
                        authorization.owner_last_changed_at(),
                    );
                    let (claimed, owner_changed) = if allow_auto_claim {
                        authorization.claim_owner(peer_id, true)
                    } else {
                        (false, false)
                    };
                    if claimed {
                        auto_claimed_owner = owner_changed;
                        retried_decision = authorization
                            .route_frame(&frame, &mut retry_sink)
                            .map_err(anyhow::Error::from)?;
                    } else if !allow_auto_claim {
                        blocked_reason = Some(block_reason);
                    }
                }
                let generation = matches!(retried_decision, RouteDecision::Applied { .. })
                    .then(|| authorization.generation());
                (retried_decision, generation)
            };
            decision = retried_decision;
            accepted_authorization_generation = retried_authorization_generation;
            sink = retry_sink;
            if auto_claimed_owner {
                self.notify_input_owner_transition();
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
                authorization_generation: accepted_authorization_generation
                    .expect("applied route captures its authorization generation"),
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
        self.input.inject.pending_inject_frames.read().await.len()
    }

    pub async fn dequeue_pending_inject_input_frames_up_to(
        &self,
        max_frames: usize,
    ) -> Vec<PendingInjectInputFrame> {
        if max_frames == 0 {
            return Vec::new();
        }

        let mut queue = self.input.inject.pending_inject_frames.write().await;
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

        let mut queue = self.input.inject.pending_inject_frames.write().await;
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

    pub async fn requeue_pending_inject_input_frames_front(
        &self,
        frames: Vec<PendingInjectInputFrame>,
    ) {
        if frames.is_empty() {
            return;
        }

        let mut dropped = Vec::new();
        let mut queue = self.input.inject.pending_inject_frames.write().await;
        // Every caller returns a bounded drain (currently at most 64 frames)
        // into a larger 128-frame queue. Reserve the retained slice's full
        // capacity before inserting any of it so overflow eviction can inspect
        // only pre-existing, newer work. Otherwise a retained move inserted
        // first by the reverse/prepend loop can evict itself on the next frame.
        assert!(
            frames.len() <= MAX_PENDING_INJECT_INPUT_FRAMES,
            "retained inject slice must fit the bounded pending queue"
        );
        let retained_capacity = frames.len();
        let newer_capacity = MAX_PENDING_INJECT_INPUT_FRAMES.saturating_sub(retained_capacity);
        while queue.len() > newer_capacity {
            let dropped_frame = remove_newest_coalescible_pending_inject_frame(&mut queue)
                .map(|frame| (frame, "evict_newest_move"))
                .or_else(|| {
                    queue
                        .pop_back()
                        .map(|frame| (frame, "evict_newest_fallback"))
                });
            if let Some((dropped_frame, reason)) = dropped_frame {
                dropped.push((dropped_frame.peer_id, dropped_frame.sequence, reason));
            }
        }
        for frame in frames.into_iter().rev() {
            queue.push_front(frame);
        }
        let depth = queue.len();
        drop(queue);

        for (peer_id, sequence, reason) in dropped {
            self.record_input_queue_overflow_drop("inject", &peer_id, sequence, reason);
        }
        if let Some(depth) = self.observe_pending_inject_high_water(depth) {
            self.record_input_queue_high_water("inject", "requeue", depth);
        }
        self.notify_input_inject_wake("retry_requeue");
    }

    pub async fn has_pending_inject_input_frames(&self) -> bool {
        !self
            .input
            .inject
            .pending_inject_frames
            .read()
            .await
            .is_empty()
    }

    pub async fn next_pending_inject_retry_at(&self) -> Option<Instant> {
        self.input
            .inject
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
            .inject
            .pending_inject_high_water
            .load(std::sync::atomic::Ordering::Relaxed);
        while depth > current {
            match self
                .input
                .inject
                .pending_inject_high_water
                .compare_exchange(
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

        let (claimed, owner_changed) = {
            let mut authorization = self.input.control.authorization.write().await;
            authorization.claim_owner(peer_id, force)
        };
        if owner_changed {
            self.notify_input_owner_transition();
        }
        Ok(claimed)
    }

    pub async fn with_input_injection_authorization<T>(
        &self,
        peer_id: &str,
        authorization_generation: u64,
        apply: impl FnOnce() -> T,
    ) -> Option<T> {
        let authorization = self.input.control.authorization.read().await;
        authorization
            .authorizes_peer_generation(peer_id, authorization_generation)
            .then(apply)
    }

    pub async fn input_injection_authorized(
        &self,
        peer_id: &str,
        authorization_generation: u64,
    ) -> bool {
        self.input
            .control
            .authorization
            .read()
            .await
            .authorizes_peer_generation(peer_id, authorization_generation)
    }

    pub async fn held_input_authorization_is_current(&self, generation: u64) -> bool {
        self.input
            .control
            .authorization
            .read()
            .await
            .authorizes_held_generation(generation)
    }

    pub async fn release_input_owner(&self, peer_id: &str) -> bool {
        let released = {
            let mut authorization = self.input.control.authorization.write().await;
            authorization.release_owner(peer_id)
        };
        if released {
            self.notify_input_owner_transition();
        }
        released
    }

    pub fn notify_input_owner_transition(&self) {
        self.notify_input_inject_wake("input_owner_changed");
    }

    #[cfg(test)]
    pub(crate) async fn input_authorization_generation(&self) -> u64 {
        self.input.control.authorization.read().await.generation()
    }

    pub async fn input_owner(&self) -> Option<String> {
        self.input
            .control
            .authorization
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

        let mut target = self.input.control.capture_target_peer_id.write().await;
        *target = next.clone();
        drop(target);
        self.notify_input_capture_wake("capture_target_changed");
        Ok(next)
    }

    pub async fn clear_input_capture_target(&self) {
        *self.input.control.capture_target_peer_id.write().await = None;
        self.notify_input_capture_wake("capture_target_cleared");
    }

    pub async fn input_capture_target(&self) -> Option<String> {
        self.input
            .control
            .capture_target_peer_id
            .read()
            .await
            .clone()
    }

    pub async fn active_input_capture_target(&self) -> Option<String> {
        if self.input_capture_backend_mode().await == "service_session_unsupported" {
            return None;
        }
        let target = self.input_capture_target().await?;
        let config = self.config.read().await;
        active_input_capture_target_from_config(&config, &target)
    }

    pub async fn set_input_lock_runtime(&self, active: bool, supported: bool) {
        *self.input.control.lock_active.write().await = active;
        *self.input.control.lock_supported.write().await = supported;
    }

    pub async fn input_lock_runtime(&self) -> (bool, bool) {
        let active = *self.input.control.lock_active.read().await;
        let supported = *self.input.control.lock_supported.read().await;
        (active, supported)
    }

    pub async fn set_input_capture_backend_mode(&self, mode: &str) {
        *self.input.control.capture_backend_mode.write().await = mode.to_string();
    }

    pub async fn input_capture_backend_mode(&self) -> String {
        self.input.control.capture_backend_mode.read().await.clone()
    }

    pub async fn pending_inject_frame_stats(&self) -> (usize, usize) {
        let pending = self.input.inject.pending_inject_frames.read().await.len();
        let high_water = self
            .input
            .inject
            .pending_inject_high_water
            .load(std::sync::atomic::Ordering::Acquire);
        (pending, high_water)
    }

    fn auto_claim_input_owner_allowed_now(
        &self,
        decision: &RouteDecision,
        owner_last_changed_at: Option<Instant>,
    ) -> (bool, &'static str) {
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
                let cooldown_ready = owner_last_changed_at
                    .map(|last| last.elapsed() >= cooldown)
                    .unwrap_or(true);

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

pub(crate) fn active_input_capture_target_from_config(
    config: &RuntimeConfig,
    target: &str,
) -> Option<String> {
    let share_input_enabled = config.features.get("share_input").copied().unwrap_or(true);
    if !share_input_enabled {
        return None;
    }

    config
        .peers
        .iter()
        .any(|peer| peer.peer_id == target && peer.connected)
        .then(|| target.to_string())
}
