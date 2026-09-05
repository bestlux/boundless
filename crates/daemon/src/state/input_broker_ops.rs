use super::input_broker::{InputBrokerAttachment, InputBrokerProcessIdentity, InputBrokerRelay};
use super::*;

pub(crate) const INPUT_BROKER_INJECT_MAX_FRAMES_PER_EXCHANGE: usize = 64;

#[derive(Debug, Clone)]
pub struct InputBrokerAttachOutcome {
    pub accepted: bool,
    pub broker_token: String,
    pub message: String,
    pub protocol_revision: u32,
    pub delivery_epoch: String,
}

#[derive(Debug, Default)]
pub struct InputBrokerExchangeOutcome {
    pub accepted: bool,
    pub message: String,
    pub inject_frames: Vec<PendingInjectInputFrame>,
    pub lock_should_be_active: bool,
    pub capture_active: bool,
    pub capture_forwarding_authorized: bool,
    pub inject_batch_id: u64,
    pub inject_batch_cancelled: bool,
    pub inject_authorization_generation: u64,
    pub held_input_authorized: bool,
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
    pub local_payload_disposition: ClipboardBrokerLocalPayloadDisposition,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClipboardBrokerLocalPayloadDisposition {
    #[default]
    NotSubmitted,
    Accepted,
    TransientRejected,
    DeterministicRejected,
}

#[derive(Debug, Clone, Default)]
pub struct InputBrokerExchangeObservations {
    pub captured_events: Vec<InputEvent>,
    pub cursor: Option<(i32, i32)>,
    pub virtual_bounds: Option<(i32, i32, i32, i32)>,
    pub escape_unlock_count: u32,
    pub lease_expired_unlock_count: u32,
    pub detector_unavailable_unlock_count: u32,
    pub handoff_probe: Option<(i32, i32)>,
    pub lock_active: bool,
    pub dropped_event_count: u64,
    pub injected_frame_count: u32,
    pub inject_failure_count: u32,
    pub inject_backpressure: bool,
    pub acked_inject_batch_id: u64,
    pub failed_inject_batch_id: u64,
    pub held_input_authorization_generation: u64,
    pub raw_device_wheel_event_count: u32,
    pub raw_system_wheel_event_count: u32,
    pub hook_wheel_event_count: u32,
    pub elevated_injector_state: String,
    pub elevated_injector_reason: String,
    pub elevated_injector_signature_trust: String,
}

/// Identity of the caller as verified by the transport layer (named-pipe
/// client process token SID and client session id). `None` means the
/// transport could not verify the caller; broker authorization fails closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBrokerClientIdentity {
    pub process_id: Option<u32>,
    pub process_creation_time: Option<u64>,
    pub user_sid: Option<String>,
    pub session_id: Option<u32>,
}

impl InputBrokerClientIdentity {
    fn process_identity(&self) -> Option<InputBrokerProcessIdentity> {
        let process_id = self.process_id.filter(|pid| *pid != 0)?;
        let creation_time = self.process_creation_time.filter(|time| *time != 0)?;
        Some(InputBrokerProcessIdentity {
            process_id,
            creation_time,
            user_sid: self.user_sid.clone()?,
            session_id: self.session_id?,
        })
    }
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
        if client.process_identity().is_none() {
            return Some("unverified_process_incarnation");
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

    pub async fn attach_input_broker_versioned(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_version: String,
        lock_supported: bool,
        protocol_revision: u32,
    ) -> InputBrokerAttachOutcome {
        if protocol_revision != ipc_api::INPUT_BROKER_PROTOCOL_REVISION {
            return InputBrokerAttachOutcome {
                accepted: false,
                broker_token: String::new(),
                message: format!(
                    "input broker protocol mismatch: remote={protocol_revision} expected={}",
                    ipc_api::INPUT_BROKER_PROTOCOL_REVISION
                ),
                protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
                delivery_epoch: String::new(),
            };
        }
        if !self.input_broker.service_session_input() {
            return InputBrokerAttachOutcome {
                accepted: false,
                broker_token: String::new(),
                message: "input broker not required: this daemon owns interactive input in its own session".to_string(),
                protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
                delivery_epoch: String::new(),
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
                protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
                delivery_epoch: String::new(),
            };
        }

        // Serialize replacement with broker exchanges and the capture pass so
        // every pre-replacement Down is ordered before one authoritative Up.
        let _capture_transition = self.input_capture_transition.lock().await;
        let process = verified_client
            .as_ref()
            .and_then(InputBrokerClientIdentity::process_identity)
            .expect("broker authorization verified process incarnation");
        let process_replaced = self.input_broker.process_replaced(&process);
        let broker_token = uuid::Uuid::new_v4().to_string();
        let replaced = self.input_broker.attachment().is_some();
        let capture_target = self.input_capture_target().await;
        // Fail-open state is settled before config access or peer queueing.
        // Network/persistence availability must never retain local capture.
        if replaced || process_replaced {
            let _ = self.input_broker.set_desired_lock_active(false);
            self.set_input_lock_runtime(false, false).await;
            self.clear_input_capture_target().await;
        }
        let recovery_release_count = if process_replaced {
            let mut authorization = self.input.control.authorization.write().await;
            authorization.require_explicit_handoff();
            self.input
                .inject
                .pending_inject_frames
                .write()
                .await
                .clear();
            let release_count = self.input_broker.reset_replaced_process_delivery();
            drop(authorization);
            self.notify_input_owner_transition();
            release_count
        } else {
            0
        };
        let mut release_event_count = 0usize;
        let release = self
            .input_broker
            .prepare_capture_release(capture_target.as_deref());
        if replaced || release.is_some() {
            if let Some((peer_id, release_events)) = release {
                release_event_count = release_events.len();
                for events in release_events.chunks(MAX_EVENTS_PER_FRAME) {
                    if let Err(error) = self.queue_input_events(&peer_id, events.to_vec()).await {
                        self.record_transport_event(TransportEventRecord {
                            timestamp: Utc::now(),
                            direction: "local".to_string(),
                            kind: "input_broker_release_queue_failed".to_string(),
                            peer_id: peer_id.clone(),
                            detail: format!("reason=replacement_attach error={error:#}"),
                            size_bytes: release_event_count as u64,
                        });
                        return InputBrokerAttachOutcome {
                    accepted: false,
                    broker_token: String::new(),
                    message: "input broker replacement deferred: authoritative releases could not be queued; retry attach"
                        .to_string(),
                    protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
                    delivery_epoch: String::new(),
                };
                    }
                }
            }
            self.input_broker.complete_capture_release();
            self.input_broker.clear_pressed_state();
            self.requeue_broker_clipboard_inflight().await;
        }
        // Only a surviving process may retain a delivery receipt/suffix. A
        // replacement has a new epoch and release-only recovery work.
        self.input_broker.attach(InputBrokerAttachment {
            broker_token: broker_token.clone(),
            lock_supported,
            process,
        });
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "local".to_string(),
            kind: "input_broker_attached".to_string(),
            peer_id: "none".to_string(),
            detail: format!(
                "client_session_id={} lock_supported={lock_supported} replaced_previous={replaced} process_replaced={process_replaced} recovery_releases={recovery_release_count} release_events={release_event_count} broker_version={broker_version}",
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
            protocol_revision: ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
            delivery_epoch: self.input_broker.delivery_epoch(),
        }
    }

    #[cfg(test)]
    pub async fn attach_input_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_version: String,
        lock_supported: bool,
    ) -> InputBrokerAttachOutcome {
        self.attach_input_broker_versioned(
            verified_client,
            broker_version,
            lock_supported,
            ipc_api::INPUT_BROKER_PROTOCOL_REVISION,
        )
        .await
    }

    pub async fn detach_input_broker(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_token: &str,
        delivery_epoch: &str,
        acked_inject_batch_id: u64,
    ) -> bool {
        self.detach_input_broker_with_reset(
            verified_client,
            broker_token,
            delivery_epoch,
            acked_inject_batch_id,
            false,
        )
        .await
    }

    pub async fn detach_input_broker_with_reset(
        &self,
        verified_client: Option<InputBrokerClientIdentity>,
        broker_token: &str,
        delivery_epoch: &str,
        acked_inject_batch_id: u64,
        reset_input_session: bool,
    ) -> bool {
        if let Some(reason) = self.input_broker_client_rejection(&verified_client) {
            self.record_input_broker_rejection("detach", reason).await;
            return false;
        }

        // Order an in-flight captured batch before the final release frame,
        // or detach first so the capture pass observes an empty relay.
        let _capture_transition = self.input_capture_transition.lock().await;
        if !verified_client
            .as_ref()
            .and_then(InputBrokerClientIdentity::process_identity)
            .is_some_and(|process| self.input_broker.attached_process_matches(&process))
        {
            self.record_input_broker_rejection("detach", "wrong_process_incarnation")
                .await;
            return false;
        }
        let capture_target = self.input_capture_target().await;
        let detached = match self.input_broker.acknowledge_and_detach(
            broker_token,
            delivery_epoch,
            acked_inject_batch_id,
        ) {
            Ok(detached) => detached,
            Err(reason) => {
                self.record_input_broker_rejection("detach", reason).await;
                return false;
            }
        };
        if detached {
            let uncertain_batch = reset_input_session
                .then(|| self.input_broker.inflight_inject_batch())
                .flatten();
            let mut affected_peers = uncertain_batch
                .as_ref()
                .map(|batch| {
                    batch
                        .frames
                        .iter()
                        .map(|frame| frame.peer_id.clone())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            if reset_input_session && affected_peers.is_empty() {
                affected_peers.extend(
                    self.config
                        .read()
                        .await
                        .peers
                        .iter()
                        .filter(|peer| peer.connected)
                        .map(|peer| peer.peer_id.clone()),
                );
            }
            let unacked_inject_frames = self.input_broker.take_inflight_inject_frames();
            // The daemon owns delivery until the tray acknowledges a batch.
            // A cooperative detach/replacement therefore returns every
            // unacknowledged frame to the queue before a new broker can drain.
            // A hard daemon-process crash remains the explicit durability
            // boundary; these in-memory input frames are not persisted.
            if !reset_input_session {
                self.requeue_pending_inject_input_frames_front(unacked_inject_frames)
                    .await;
            }
            let release_events = self.input_broker.drain_release_events();
            let release_event_count = release_events.len();
            if let Some(peer_id) = capture_target.as_deref()
                && !release_events.is_empty()
            {
                let _ = self.queue_input_events(peer_id, release_events).await;
            }
            self.clear_input_capture_target().await;
            let released_owner = {
                let mut authorization = self.input.control.authorization.write().await;
                let incoming_owner = authorization.owner().map(str::to_string);
                if reset_input_session {
                    if affected_peers.is_empty()
                        && let Some(peer_id) = incoming_owner.as_ref()
                    {
                        affected_peers.insert(peer_id.clone());
                    }
                    authorization.quarantine_auto_claim_peers(affected_peers.iter().cloned());
                }
                let release_current_owner = !reset_input_session
                    || incoming_owner
                        .as_ref()
                        .is_some_and(|peer_id| affected_peers.contains(peer_id));
                release_current_owner
                    && incoming_owner
                        .as_deref()
                        .is_some_and(|peer_id| authorization.release_owner(peer_id))
            };
            if released_owner {
                self.notify_input_owner_transition();
            }
            self.set_input_lock_runtime(false, false).await;
            self.requeue_broker_clipboard_inflight().await;
            if reset_input_session {
                let affected_peer = match affected_peers.len() {
                    0 => "none".to_string(),
                    1 => affected_peers
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| "none".to_string()),
                    _ => "multiple".to_string(),
                };
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "elevated_injector_delivery_uncertain".to_string(),
                    peer_id: affected_peer,
                    detail: format!(
                        "reason=session_reset batch_id={} frame_count={} affected_peer_count={}",
                        uncertain_batch.as_ref().map_or(0, |batch| batch.batch_id),
                        uncertain_batch
                            .as_ref()
                            .map_or(0, |batch| batch.frames.len()),
                        affected_peers.len()
                    ),
                    size_bytes: uncertain_batch
                        .as_ref()
                        .map_or(0, |batch| batch.frames.len() as u64),
                });
            }
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "input_broker_detached".to_string(),
                peer_id: "none".to_string(),
                detail: format!(
                    "reason={} capture_target_cleared={} owner_released={released_owner} release_events={}",
                    if reset_input_session {
                        "delivery_uncertain"
                    } else {
                        "broker_requested"
                    },
                    capture_target.is_some(),
                    release_event_count
                ),
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
        local_sequence: Option<u64>,
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
            .validate_without_touch(broker_token, Instant::now())
            || !verified_client
                .as_ref()
                .and_then(InputBrokerClientIdentity::process_identity)
                .is_some_and(|process| self.input_broker.attached_process_matches(&process))
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
                    detail: format!(
                        "disposition=unmatched_apply_report applied={}",
                        report.applied
                    ),
                    size_bytes: 0,
                });
            }
        }

        let mut message = String::new();
        let (local_payload_disposition, local_update_supersedes_remote) =
            match (local_payload, local_sequence) {
                (None, _) => (ClipboardBrokerLocalPayloadDisposition::NotSubmitted, false),
                (Some(_), None) => {
                    message =
                        "clipboard broker local payload rejected: sequence missing".to_string();
                    (
                        ClipboardBrokerLocalPayloadDisposition::TransientRejected,
                        false,
                    )
                }
                (Some(payload), Some(sequence)) => match self
                    .queue_local_clipboard_payload_for_connected_peers(payload)
                    .await
                {
                    Ok(outcome) => {
                        let first_observation =
                            self.input_broker.accept_local_clipboard_sequence(sequence);
                        (
                            ClipboardBrokerLocalPayloadDisposition::Accepted,
                            first_observation && outcome.supersedes_remote(),
                        )
                    }
                    Err(error) => {
                        let disposition = classify_clipboard_local_payload_error(&error);
                        message = format!("clipboard broker local payload rejected: {error:#}");
                        let reason = match disposition {
                            ClipboardBrokerLocalPayloadDisposition::DeterministicRejected => {
                                "policy_or_validation"
                            }
                            _ => "unknown",
                        };
                        self.record_transport_event(TransportEventRecord {
                            timestamp: Utc::now(),
                            direction: "local".to_string(),
                            kind: "clipboard_broker_local_rejected".to_string(),
                            peer_id: "none".to_string(),
                            detail: format!("disposition=rejected reason={reason}"),
                            size_bytes: 0,
                        });
                        (disposition, false)
                    }
                },
            };

        if local_update_supersedes_remote {
            let discarded = self
                .discard_broker_remote_clipboard_for_local_update()
                .await;
            if discarded > 0 {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "clipboard_broker_remote_superseded".to_string(),
                    peer_id: "none".to_string(),
                    detail: format!("discarded_remote_payloads={discarded}"),
                    size_bytes: discarded as u64,
                });
            }
        }

        let remote_payload = if matches!(
            local_payload_disposition,
            ClipboardBrokerLocalPayloadDisposition::TransientRejected
        ) || local_update_supersedes_remote
        {
            None
        } else {
            self.stage_remote_clipboard_payload_for_broker().await
        };

        ClipboardBrokerExchangeOutcome {
            accepted: true,
            message,
            remote_payload,
            local_payload_disposition,
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
        let _capture_transition = self.input_capture_transition.lock().await;
        if !verified_client
            .as_ref()
            .and_then(InputBrokerClientIdentity::process_identity)
            .is_some_and(|process| self.input_broker.attached_process_matches(&process))
            || !self
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
        if let Some(status) = self.input_broker.observe_elevated_injector_status(
            &observations.elevated_injector_state,
            &observations.elevated_injector_reason,
            &observations.elevated_injector_signature_trust,
        ) {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "elevated_injector_status_changed".to_string(),
                peer_id: "none".to_string(),
                detail: format!(
                    "state={} reason={} signature_trust={}",
                    status.state, status.reason, status.signature_trust
                ),
                size_bytes: 0,
            });
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

        if let Some(mode) = self.input_broker.observe_wheel_source_counts(
            observations.raw_device_wheel_event_count,
            observations.raw_system_wheel_event_count,
            observations.hook_wheel_event_count,
        ) {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "input_broker_wheel_source_changed".to_string(),
                peer_id: "none".to_string(),
                detail: format!(
                    "mode={mode} raw_device_events={} raw_system_events={} hook_events={}",
                    observations.raw_device_wheel_event_count,
                    observations.raw_system_wheel_event_count,
                    observations.hook_wheel_event_count,
                ),
                size_bytes: 0,
            });
        }

        let capture_active = self.active_input_capture_target().await.is_some();
        let lock_should_be_active = self.input_broker.desired_lock_active();
        let lock_report_authorizes_next_exchange = capture_active
            && lock_should_be_active
            && self.input_broker.lock_supported()
            && observations.lock_active;
        let safety_unlock_reported = observations.escape_unlock_count > 0
            || observations.lease_expired_unlock_count > 0
            || observations.detector_unavailable_unlock_count > 0;
        let captured_events = if self.input_broker.capture_forwarding_authorized()
            && lock_report_authorizes_next_exchange
            && !safety_unlock_reported
        {
            observations.captured_events
        } else {
            Vec::new()
        };
        let accepted_handoff_probe = if capture_active {
            None
        } else {
            observations.handoff_probe
        };
        let handoff_probe_reported =
            accepted_handoff_probe.is_some_and(|(dx, dy)| dx != 0 || dy != 0);
        let capture_forwarding_authorized =
            lock_report_authorizes_next_exchange && !safety_unlock_reported;
        let queued = self.input_broker.push_broker_observations(
            captured_events,
            observations.cursor,
            observations.virtual_bounds,
            observations.escape_unlock_count,
            observations.lease_expired_unlock_count,
            observations.detector_unavailable_unlock_count,
            accepted_handoff_probe,
            observations.lock_active,
            observations.dropped_event_count,
        );
        self.input_broker
            .set_capture_forwarding_authorized(capture_forwarding_authorized);
        if queued > 0 || safety_unlock_reported || handoff_probe_reported {
            self.notify_input_capture_wake("input_broker_exchange");
        }

        let inflight_before_ack = self.input_broker.inflight_inject_batch();
        if observations.failed_inject_batch_id != 0 && observations.acked_inject_batch_id != 0 {
            return InputBrokerExchangeOutcome {
                accepted: false,
                message:
                    "input broker cannot acknowledge and fail an inject batch in the same exchange"
                        .to_string(),
                ..Default::default()
            };
        }

        if observations.failed_inject_batch_id != 0 {
            match self
                .input_broker
                .fail_inflight_inject_batch(observations.failed_inject_batch_id)
            {
                Ok((batch, first_report)) => {
                    if first_report {
                        self.record_transport_event(TransportEventRecord {
                            timestamp: Utc::now(),
                            direction: "local".to_string(),
                            kind: "elevated_injector_delivery_uncertain".to_string(),
                            peer_id: "none".to_string(),
                            detail: format!(
                                "reason=delivery_uncertain batch_id={} frame_count={}",
                                batch.batch_id,
                                batch.frames.len()
                            ),
                            size_bytes: batch.frames.len() as u64,
                        });
                    }
                }
                Err(reason) => {
                    return InputBrokerExchangeOutcome {
                        accepted: false,
                        message: format!("input broker inject failure report rejected: {reason}"),
                        ..Default::default()
                    };
                }
            }
        }

        if observations.inject_backpressure && inflight_before_ack.is_none() {
            return InputBrokerExchangeOutcome {
                accepted: false,
                message: "input broker backpressure reported without an in-flight inject batch"
                    .to_string(),
                ..Default::default()
            };
        }

        if observations.inject_backpressure
            && observations.acked_inject_batch_id != 0
            && inflight_before_ack
                .as_ref()
                .is_some_and(|batch| batch.batch_id == observations.acked_inject_batch_id)
        {
            return InputBrokerExchangeOutcome {
                accepted: false,
                message: "input broker cannot acknowledge an inject batch while reporting backpressure for it"
                    .to_string(),
                ..Default::default()
            };
        }

        if let Err(reason) = self
            .input_broker
            .acknowledge_inject_batch(observations.acked_inject_batch_id)
        {
            return InputBrokerExchangeOutcome {
                accepted: false,
                message: format!("input broker inject acknowledgement rejected: {reason}"),
                ..Default::default()
            };
        }

        let existing_batch = self.input_broker.inflight_inject_batch().or_else(|| {
            // Generation is validated below for normal work; synthesized Ups
            // deliberately do not require the failed process's remote owner.
            self.input_broker.stage_recovery_cleanup(1)
        });
        let dequeued = if existing_batch.is_none() && !observations.inject_backpressure {
            self.dequeue_pending_inject_input_frames_up_to(
                INPUT_BROKER_INJECT_MAX_FRAMES_PER_EXCHANGE,
            )
            .await
        } else {
            Vec::new()
        };

        // Owner, sharing policy, and generation are one linearizable state.
        // Hold its read guard through retained-batch validation or new-batch
        // staging so an owner/policy writer cannot relabel frames between the
        // authorization decision and the generation committed to the batch.
        let authorization = self.input.control.authorization.read().await;
        let held_input_authorized = authorization
            .authorizes_held_generation(observations.held_input_authorization_generation);
        let retained_authorization_changed = existing_batch.as_ref().is_some_and(|batch| {
            !batch.cancelled
                && !batch.recovery_cleanup
                && (batch.authorization_generation != authorization.generation()
                    || batch.frames.iter().any(|frame| {
                        !authorization.authorizes_peer_generation(
                            &frame.peer_id,
                            frame.authorization_generation,
                        )
                    }))
        });
        let mut accepted = Vec::with_capacity(dequeued.len());
        let mut rejected = Vec::new();
        for frame in dequeued {
            if authorization
                .authorizes_peer_generation(&frame.peer_id, frame.authorization_generation)
            {
                accepted.push(frame);
            } else {
                rejected.push(frame);
            }
        }
        let staged_batch = (!accepted.is_empty()).then(|| {
            self.input_broker
                .stage_inject_batch(accepted, authorization.generation())
        });
        drop(authorization);

        let batch = if let Some(mut batch) = existing_batch {
            if retained_authorization_changed {
                for frame in &batch.frames {
                    self.record_input_inject_skipped(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                        "retained_batch_authorization_changed",
                    )
                    .await;
                }
                let Some(cancelled_batch) = self
                    .input_broker
                    .cancel_inflight_inject_batch(batch.batch_id)
                else {
                    return InputBrokerExchangeOutcome {
                        accepted: false,
                        message:
                            "input broker inject batch changed during authorization revalidation"
                                .to_string(),
                        ..Default::default()
                    };
                };
                batch = cancelled_batch;
            }
            Some(batch)
        } else {
            for frame in rejected {
                self.record_input_inject_skipped(
                    &frame.peer_id,
                    frame.sequence,
                    frame.events.len(),
                    frame.timing(),
                    "owner_or_feature_changed",
                )
                .await;
            }
            if let Some(batch) = staged_batch.as_ref() {
                for frame in &batch.frames {
                    self.record_input_broker_inject_dispatched(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        frame.timing(),
                    )
                    .await;
                }
            }
            staged_batch
        };
        let inject_batch_id = batch.as_ref().map_or(0, |batch| batch.batch_id);
        let inject_batch_cancelled = batch.as_ref().is_some_and(|batch| batch.cancelled);
        let inject_authorization_generation = batch
            .as_ref()
            .filter(|batch| !batch.cancelled)
            .map_or(0, |batch| batch.authorization_generation);
        let inject_frames = if observations.inject_backpressure || inject_batch_cancelled {
            Vec::new()
        } else {
            batch.map_or_else(Vec::new, |batch| batch.frames)
        };

        InputBrokerExchangeOutcome {
            accepted: true,
            message: String::new(),
            inject_frames,
            lock_should_be_active,
            capture_active,
            capture_forwarding_authorized,
            inject_batch_id,
            inject_batch_cancelled,
            inject_authorization_generation,
            held_input_authorized,
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

fn classify_clipboard_local_payload_error(
    error: &anyhow::Error,
) -> ClipboardBrokerLocalPayloadDisposition {
    if error.downcast_ref::<ClipboardPolicyError>().is_some()
        || error.downcast_ref::<BmpValidationError>().is_some()
    {
        ClipboardBrokerLocalPayloadDisposition::DeterministicRejected
    } else {
        ClipboardBrokerLocalPayloadDisposition::TransientRejected
    }
}

#[cfg(test)]
mod local_payload_disposition_tests {
    use super::*;

    #[test]
    fn classifies_policy_failures_as_deterministic_and_unknown_failures_as_transient() {
        let deterministic =
            anyhow::Error::new(ClipboardPolicyError::TextTooLarge { size: 2, limit: 1 });
        assert_eq!(
            classify_clipboard_local_payload_error(&deterministic),
            ClipboardBrokerLocalPayloadDisposition::DeterministicRejected
        );

        let transient = anyhow::anyhow!("temporary queue failure");
        assert_eq!(
            classify_clipboard_local_payload_error(&transient),
            ClipboardBrokerLocalPayloadDisposition::TransientRejected
        );
    }
}
