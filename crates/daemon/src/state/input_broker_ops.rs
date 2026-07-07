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

#[derive(Debug, Clone)]
pub struct ClipboardBrokerApplyReport {
    pub source_peer_id: String,
    pub hash: String,
    pub applied: bool,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ClipboardBrokerExchangeOutcome {
    pub accepted: bool,
    pub message: String,
    pub remote_payload: Option<PendingRemoteClipboardPayload>,
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

/// Identity of the caller as verified by the transport layer (named-pipe
/// client process token SID and client session id). `None` means the
/// transport could not verify the caller; broker authorization fails closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBrokerClientIdentity {
    pub user_sid: Option<String>,
    pub session_id: Option<u32>,
}

impl AppState {
    pub(crate) fn input_broker_relay(&self) -> Arc<InputBrokerRelay> {
        self.input_broker.clone()
    }

    /// Configures the only account whose verified pipe clients may act as the
    /// user-session input broker. Called by the service host with the same
    /// allowed-user SID that scopes the control pipe ACL.
    pub fn set_input_broker_allowed_user_sid(&self, allowed_user_sid: &str) {
        self.input_broker
            .set_allowed_user_sid(allowed_user_sid.to_string());
    }

    /// True when incoming inject frames should be left queued for the
    /// user-session broker instead of the local (unsupported) inject backend.
    pub(crate) fn input_broker_route_active(&self) -> bool {
        self.input_broker.service_session_input()
            && self.input_broker.is_attached_fresh(Instant::now())
    }

    pub(crate) fn clipboard_uses_broker(&self) -> bool {
        self.input_broker.service_session_input()
    }

    pub(crate) fn clipboard_backend_mode(&self) -> &'static str {
        if !self.clipboard_uses_broker() {
            return CLIPBOARD_DIRECT_BACKEND_MODE;
        }
        if self.input_broker.is_attached_fresh(Instant::now()) {
            CLIPBOARD_USER_SESSION_BROKER_MODE
        } else {
            CLIPBOARD_BROKER_UNAVAILABLE_MODE
        }
    }

    /// Fail-closed authorization for broker calls, evaluated exclusively
    /// against the transport-verified caller identity. Admin/SYSTEM callers
    /// keep pipe access for diagnostics, but only interactive-session clients
    /// of the configured allowed user pass this gate.
    fn input_broker_client_rejection(
        &self,
        verified_client: &Option<InputBrokerClientIdentity>,
    ) -> Option<&'static str> {
        let Some(client) = verified_client else {
            return Some("unverified_client");
        };
        let Some(session_id) = client.session_id else {
            return Some("unverified_session");
        };
        if session_id == 0 {
            return Some("non_interactive_session");
        }
        let Some(user_sid) = client.user_sid.as_deref() else {
            return Some("unverified_user");
        };
        let Some(allowed_user_sid) = self.input_broker.allowed_user_sid() else {
            return Some("allowed_user_not_configured");
        };
        if user_sid != allowed_user_sid {
            return Some("wrong_user");
        }
        None
    }

    async fn record_input_broker_rejection(&self, surface: &str, reason: &str) {
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: format!("input_broker_{surface}_rejected"),
            peer_id: "none".to_string(),
            detail: format!("reason={reason}"),
            size_bytes: 0,
        });
    }

    pub async fn attach_input_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
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
        if let Some(reason) = self.input_broker_client_rejection(&verified_client) {
            self.record_input_broker_rejection("attach", reason).await;
            return InputBrokerAttachOutcome {
                accepted: false,
                broker_token: String::new(),
                message: format!(
                    "input broker attach denied ({reason}): the pipe client must be a verified interactive-session process of the allowed desktop user"
                ),
            };
        }

        let broker_token = uuid::Uuid::new_v4().to_string();
        let replaced = self.input_broker.attachment().is_some();
        if replaced {
            self.requeue_broker_clipboard_inflight().await;
        }
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
                "client_session_id={} lock_supported={lock_supported} replaced_previous={replaced} broker_version={broker_version}",
                verified_client
                    .as_ref()
                    .and_then(|client| client.session_id)
                    .unwrap_or_default()
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

    pub async fn detach_input_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_token: &str,
    ) -> bool {
        if let Some(reason) = self.input_broker_client_rejection(&verified_client) {
            self.record_input_broker_rejection("detach", reason).await;
            return false;
        }

        let detached = self.input_broker.detach(broker_token);
        if detached {
            self.requeue_broker_clipboard_inflight().await;
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

    pub async fn exchange_clipboard_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_token: &str,
        local_payload: Option<ClipboardPayload>,
        apply_report: Option<ClipboardBrokerApplyReport>,
    ) -> ClipboardBrokerExchangeOutcome {
        if let Some(reason) = self.input_broker_client_rejection(&verified_client) {
            self.record_input_broker_rejection("clipboard_exchange", reason)
                .await;
            return ClipboardBrokerExchangeOutcome {
                accepted: false,
                message: format!(
                    "clipboard broker exchange denied ({reason}): the pipe client must be a verified interactive-session process of the allowed desktop user"
                ),
                ..Default::default()
            };
        }
        if !self
            .input_broker
            .validate_and_touch(broker_token, Instant::now())
        {
            return ClipboardBrokerExchangeOutcome {
                accepted: false,
                message: "input broker token is not attached (stale or replaced); re-attach"
                    .to_string(),
                ..Default::default()
            };
        }

        if let Some(report) = apply_report {
            let matched = self
                .report_broker_remote_clipboard_apply(
                    &report.source_peer_id,
                    &report.hash,
                    report.applied,
                    &report.message,
                )
                .await;
            if !matched {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "clipboard_broker_apply_report_unmatched".to_string(),
                    peer_id: report.source_peer_id,
                    detail: format!("hash={} applied={}", report.hash, report.applied),
                    size_bytes: 0,
                });
            }
        }

        let mut message = String::new();
        if let Some(payload) = local_payload
            && let Err(error) = self
                .queue_local_clipboard_payload_for_connected_peers(payload)
                .await
        {
            message = format!("clipboard broker local payload rejected: {error:#}");
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "clipboard_broker_local_rejected".to_string(),
                peer_id: "none".to_string(),
                detail: message.clone(),
                size_bytes: 0,
            });
        }

        ClipboardBrokerExchangeOutcome {
            accepted: true,
            message,
            remote_payload: self.stage_remote_clipboard_payload_for_broker().await,
        }
    }

    pub async fn exchange_input_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_token: &str,
        observations: InputBrokerExchangeObservations,
    ) -> InputBrokerExchangeOutcome {
        if let Some(reason) = self.input_broker_client_rejection(&verified_client) {
            self.record_input_broker_rejection("exchange", reason).await;
            return InputBrokerExchangeOutcome {
                accepted: false,
                message: format!(
                    "input broker exchange denied ({reason}): the pipe client must be a verified interactive-session process of the allowed desktop user"
                ),
                ..Default::default()
            };
        }
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
