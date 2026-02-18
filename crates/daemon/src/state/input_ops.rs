use super::*;

impl AppState {
    async fn enqueue_pending_inject_input_frame(
        &self,
        frame: PendingInjectInputFrame,
    ) -> (usize, Option<PendingInjectInputFrame>) {
        let mut queue = self.pending_inject_input_frames.write().await;
        let dropped = if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
            queue.pop_front()
        } else {
            None
        };
        queue.push_back(frame);
        (queue.len(), dropped)
    }

    pub(crate) async fn clear_pending_inject_input_frames_for_peer(&self, peer_id: &str) {
        let mut queue = self.pending_inject_input_frames.write().await;
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
        })
        .await;
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
        })
        .await;
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
        })
        .await;
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
        let (decision, auto_claimed_owner) = {
            let mut router = self.input_router.write().await;
            let mut decision = router
                .route_frame(&frame, &mut sink)
                .map_err(anyhow::Error::from)?;
            let mut auto_claimed_owner = false;

            if matches!(
                decision,
                RouteDecision::IgnoredNoOwner | RouteDecision::IgnoredWrongOwner { .. }
            ) && router.claim_owner(peer_id, true)
            {
                auto_claimed_owner = true;
                decision = router
                    .route_frame(&frame, &mut sink)
                    .map_err(anyhow::Error::from)?;
            }

            (decision, auto_claimed_owner)
        };

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
                events: sink.events,
            };
            let (depth, dropped) = self.enqueue_pending_inject_input_frame(pending).await;
            if let Some(dropped) = dropped {
                self.record_input_inject_dropped(
                    &dropped.peer_id,
                    dropped.sequence,
                    dropped.events.len(),
                    dropped.capture_timestamp_unix_ms,
                )
                .await;
            }
            self.record_input_inject_queued(
                peer_id,
                frame.sequence,
                frame.events.len(),
                depth,
                timing,
            )
            .await;
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
        })
        .await;

        Ok(decision)
    }

    pub async fn dequeue_pending_inject_input_frame(&self) -> Option<PendingInjectInputFrame> {
        self.pending_inject_input_frames.write().await.pop_front()
    }

    pub async fn requeue_pending_inject_input_frame_front(&self, frame: PendingInjectInputFrame) {
        let mut queue = self.pending_inject_input_frames.write().await;
        if queue.len() >= MAX_PENDING_INJECT_INPUT_FRAMES {
            queue.pop_back();
        }
        queue.push_front(frame);
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
        })
        .await;
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
        })
        .await;
    }

    pub async fn claim_input_owner(&self, peer_id: &str, force: bool) -> Result<bool> {
        if self.get_peer(peer_id).await.is_none() {
            anyhow::bail!("unknown peer {peer_id}");
        }

        Ok(self.input_router.write().await.claim_owner(peer_id, force))
    }

    pub async fn input_injection_allowed_for_peer(&self, peer_id: &str) -> bool {
        let router = self.input_router.read().await;
        router.is_enabled() && router.owner() == Some(peer_id)
    }

    pub async fn release_input_owner(&self, peer_id: &str) -> bool {
        self.input_router.write().await.release_owner(peer_id)
    }

    pub async fn input_owner(&self) -> Option<String> {
        self.input_router
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

        let mut target = self.input_capture_target_peer_id.write().await;
        *target = next.clone();
        Ok(next)
    }

    pub async fn clear_input_capture_target(&self) {
        *self.input_capture_target_peer_id.write().await = None;
    }

    pub async fn input_capture_target(&self) -> Option<String> {
        self.input_capture_target_peer_id.read().await.clone()
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
        *self.input_lock_active.write().await = active;
        *self.input_lock_supported.write().await = supported;
    }

    pub async fn input_lock_runtime(&self) -> (bool, bool) {
        let active = *self.input_lock_active.read().await;
        let supported = *self.input_lock_supported.read().await;
        (active, supported)
    }

}

