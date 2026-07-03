use super::input_broker::{InputBrokerAttachment, InputBrokerRelay};
use super::*;

pub(crate) const INPUT_BROKER_INJECT_MAX_FRAMES_PER_EXCHANGE: usize = 64;

#[derive(Debug, Clone)]
pub struct InputBrokerAttachOutcome {
    pub accepted: bool,
    pub broker_token: String,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct InputBrokerExchangeOutcome {
    pub accepted: bool,
    pub message: String,
    pub inject_frames: Vec<PendingInjectInputFrame>,
    pub lock_should_be_active: bool,
    pub capture_active: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InputBrokerExchangeObservations {
    pub captured_events: Vec<InputEvent>,
    pub cursor: Option<(i32, i32)>,
    pub virtual_bounds: Option<(i32, i32, i32, i32)>,
    pub escape_unlock_count: u32,
    pub lock_active: bool,
    pub dropped_event_count: u64,
    pub injected_frame_count: u32,
    pub inject_failure_count: u32,
}

impl AppState {
    pub(crate) fn input_broker_relay(&self) -> Arc<InputBrokerRelay> {
        self.input_broker.clone()
    }

    /// True when incoming inject frames should be left queued for the
    /// user-session broker instead of the local (unsupported) inject backend.
    pub(crate) fn input_broker_route_active(&self) -> bool {
        self.input_broker.service_session_input()
            && self.input_broker.is_attached_fresh(Instant::now())
    }

    pub async fn attach_input_broker(
        &self,
        process_session_id: u32,
        broker_version: String,
        lock_supported: bool,
    ) -> InputBrokerAttachOutcome {
        if !self.input_broker.service_session_input() {
            return InputBrokerAttachOutcome {
                accepted: false,
                broker_token: String::new(),
                message: "input broker not required: this daemon owns interactive input in its own session".to_string(),
            };
        }
        if process_session_id == 0 {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "input_broker_attach_rejected".to_string(),
                peer_id: "none".to_string(),
                detail: "reason=non_interactive_session process_session_id=0".to_string(),
                size_bytes: 0,
            });
            return InputBrokerAttachOutcome {
                accepted: false,
                broker_token: String::new(),
                message:
                    "input broker must run in an interactive user session (session 0 rejected)"
                        .to_string(),
            };
        }

        let broker_token = uuid::Uuid::new_v4().to_string();
        let replaced = self.input_broker.attachment().is_some();
        self.input_broker.attach(InputBrokerAttachment {
            broker_token: broker_token.clone(),
            lock_supported,
        });
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_broker_attached".to_string(),
            peer_id: "none".to_string(),
            detail: format!(
                "process_session_id={process_session_id} lock_supported={lock_supported} replaced_previous={replaced} broker_version={broker_version}"
            ),
            size_bytes: 0,
        });
        self.notify_input_capture_wake("input_broker_attached");

        InputBrokerAttachOutcome {
            accepted: true,
            broker_token,
            message: "input broker attached for the normal unlocked desktop of the allowed user"
                .to_string(),
        }
    }

    pub async fn detach_input_broker(&self, broker_token: &str) -> bool {
        let detached = self.input_broker.detach(broker_token);
        if detached {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "input_broker_detached".to_string(),
                peer_id: "none".to_string(),
                detail: "reason=broker_requested".to_string(),
                size_bytes: 0,
            });
            self.notify_input_capture_wake("input_broker_detached");
        }
        detached
    }

    pub async fn exchange_input_broker(
        &self,
        broker_token: &str,
        observations: InputBrokerExchangeObservations,
    ) -> InputBrokerExchangeOutcome {
        if !self
            .input_broker
            .validate_and_touch(broker_token, Instant::now())
        {
            return InputBrokerExchangeOutcome {
                accepted: false,
                message: "input broker token is not attached (stale or replaced); re-attach"
                    .to_string(),
                ..Default::default()
            };
        }

        if observations.inject_failure_count > 0 {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "input_broker_inject_report".to_string(),
                peer_id: "none".to_string(),
                detail: format!(
                    "injected_frames={} failed_frames={}",
                    observations.injected_frame_count, observations.inject_failure_count
                ),
                size_bytes: 0,
            });
        }

        let queued = self.input_broker.push_broker_observations(
            observations.captured_events,
            observations.cursor,
            observations.virtual_bounds,
            observations.escape_unlock_count,
            observations.lock_active,
            observations.dropped_event_count,
        );
        if queued > 0 || observations.escape_unlock_count > 0 {
            self.notify_input_capture_wake("input_broker_exchange");
        }

        let mut inject_frames = Vec::new();
        let dequeued = self
            .dequeue_pending_inject_input_frames_up_to(INPUT_BROKER_INJECT_MAX_FRAMES_PER_EXCHANGE)
            .await;
        for frame in dequeued {
            if !self.input_injection_allowed_for_peer(&frame.peer_id).await {
                self.record_input_inject_skipped(
                    &frame.peer_id,
                    frame.sequence,
                    frame.events.len(),
                    frame.timing(),
                    "owner_or_feature_changed",
                )
                .await;
                continue;
            }
            self.record_input_broker_inject_dispatched(
                &frame.peer_id,
                frame.sequence,
                frame.events.len(),
                frame.timing(),
            )
            .await;
            inject_frames.push(frame);
        }

        let capture_active = self.active_input_capture_target().await.is_some();

        InputBrokerExchangeOutcome {
            accepted: true,
            message: String::new(),
            inject_frames,
            lock_should_be_active: self.input_broker.desired_lock_active(),
            capture_active,
        }
    }

    async fn record_input_broker_inject_dispatched(
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
            kind: "input_broker_inject_dispatched".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "sequence={sequence} queue_wait_ms={} capture_to_dispatch_ms={} receive_to_dispatch_ms={}",
                elapsed_ms(timing.queued_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.capture_timestamp_unix_ms, now_ms),
                elapsed_ms(timing.received_timestamp_unix_ms, now_ms)
            ),
            size_bytes: event_count as u64,
        });
    }
}
