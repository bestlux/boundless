//! Volatile diagnostics authority and a single, cancellation-safe bounded request slot.
use std::{io::Read, sync::Mutex as StdMutex};

use app_services::paired_testing::*;
use core_protocol::{PROTOCOL_CURRENT, WireMessage};
use sha2::{Digest, Sha256};
use tokio::sync::{OnceCell, Semaphore, oneshot};

use super::*;

pub(super) struct PairedTestingState {
    inner: StdMutex<Inner>,
    run: Semaphore,
    identity: OnceCell<PairedTestIdentity>,
    instance_id: String,
    #[cfg(test)]
    reply_pause: StdMutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

impl Default for PairedTestingState {
    fn default() -> Self {
        Self {
            inner: StdMutex::new(Inner::default()),
            run: Semaphore::new(1),
            identity: OnceCell::new(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            #[cfg(test)]
            reply_pause: StdMutex::new(None),
        }
    }
}

#[derive(Default)]
struct Inner {
    consent_revision: u64,
    lease: Option<Lease>,
    pending: Option<Pending>,
    last_denial: Option<Instant>,
}

struct Lease {
    peer_id: String,
    expires: Instant,
    remaining_bytes: u64,
    remaining_requests: u32,
}

struct Pending {
    peer_id: String,
    request_id: String,
    payload: Option<Vec<u8>>,
    deadline: Instant,
    sent_on: Option<(u64, EvidenceCategory)>,
    response: oneshot::Sender<ProbeResponse>,
}

struct ProbeResponse {
    status: String,
    payload: Vec<u8>,
    remote: Option<PairedTestIdentity>,
    local_session_id: u64,
    remote_session_id: u64,
    category: EvidenceCategory,
}

/// Timing out or cancelling the run future drops its unsent payload and reply slot.
struct PendingGuard<'a>(&'a PairedTestingState);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0
            .inner
            .lock()
            .expect("diagnostic state poisoned")
            .pending = None;
    }
}

impl Inner {
    fn consent(&mut self, now: Instant) -> PairedTestConsent {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires <= now)
        {
            self.lease = None;
        }
        match &self.lease {
            Some(lease) => PairedTestConsent {
                schema_version: 1,
                peer_id: Some(lease.peer_id.clone()),
                enabled: lease.remaining_requests > 0 && lease.remaining_bytes > 0,
                remaining_seconds: lease
                    .expires
                    .saturating_duration_since(now)
                    .as_secs()
                    .saturating_add(1)
                    .min(u64::from(MAX_LEASE_SECONDS)) as u32,
                remaining_requests: lease.remaining_requests,
                remaining_bytes: lease.remaining_bytes,
            },
            None => PairedTestConsent {
                schema_version: 1,
                ..Default::default()
            },
        }
    }

    fn authorize_probe(
        &mut self,
        peer: &str,
        bytes: usize,
        now: Instant,
    ) -> Result<(), &'static str> {
        self.consent(now);
        let Some(lease) = self.lease.as_mut().filter(|lease| lease.peer_id == peer) else {
            return Err("consent_required");
        };
        if bytes > MAX_PROBE_BYTES {
            return Err("payload_limit");
        }
        if lease.remaining_requests == 0 || bytes as u64 > lease.remaining_bytes {
            return Err("lease_budget_exhausted");
        }
        lease.remaining_requests -= 1;
        lease.remaining_bytes -= bytes as u64;
        Ok(())
    }
}

impl AppState {
    #[cfg(test)]
    pub(crate) fn pause_next_diagnostic_reply_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, observed) = oneshot::channel();
        let (release, resumed) = oneshot::channel();
        *self.paired_testing.reply_pause.lock().unwrap() = Some((entered, resumed));
        (observed, release)
    }

    async fn ensure_diagnostic_peer(&self, peer_id: &str) -> Result<()> {
        self.ensure_trust_rotation_not_pending()?;
        anyhow::ensure!(
            self.list_peers()
                .await
                .iter()
                .any(|peer| peer.peer_id == peer_id),
            "peer is not paired"
        );
        anyhow::ensure!(
            self.trusted_records()
                .await?
                .iter()
                .any(|peer| peer.machine_id == peer_id),
            "peer is not trusted"
        );
        Ok(())
    }

    pub async fn set_paired_test_consent(
        &self,
        peer_id: String,
        duration_seconds: u32,
    ) -> Result<PairedTestConsent> {
        anyhow::ensure!(
            duration_seconds <= MAX_LEASE_SECONDS,
            "lease cannot exceed {MAX_LEASE_SECONDS} seconds"
        );
        let revision = {
            let mut inner = self
                .paired_testing
                .inner
                .lock()
                .expect("diagnostic state poisoned");
            inner.consent_revision = inner.consent_revision.wrapping_add(1);
            if duration_seconds == 0 {
                inner.lease = None;
                inner.last_denial = None;
                return Ok(inner.consent(Instant::now()));
            }
            inner.consent_revision
        };
        if duration_seconds > 0 {
            self.ensure_diagnostic_peer(&peer_id).await?;
            // Prepare build metadata before consent begins, outside the input/transport reactor.
            let _ = self.paired_test_identity().await;
        }
        let now = Instant::now();
        let mut inner = self
            .paired_testing
            .inner
            .lock()
            .expect("diagnostic state poisoned");
        anyhow::ensure!(
            inner.consent_revision == revision,
            "consent request superseded"
        );
        inner.last_denial = None;
        inner.lease = (duration_seconds > 0).then(|| Lease {
            peer_id,
            expires: now + Duration::from_secs(u64::from(duration_seconds)),
            remaining_bytes: MAX_LEASE_BYTES,
            remaining_requests: MAX_LEASE_REQUESTS,
        });
        Ok(inner.consent(now))
    }

    pub fn paired_test_consent(&self) -> PairedTestConsent {
        self.paired_testing
            .inner
            .lock()
            .expect("diagnostic state poisoned")
            .consent(Instant::now())
    }

    async fn paired_test_identity(&self) -> PairedTestIdentity {
        self.paired_testing
            .identity
            .get_or_init(|| async {
                let machine_id = self.snapshot().await.machine_id;
                let binary_sha256 = tokio::task::spawn_blocking(|| -> Option<String> {
                    let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
                    let mut hasher = Sha256::new();
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        let len = file.read(&mut buffer).ok()?;
                        if len == 0 {
                            break;
                        }
                        hasher.update(&buffer[..len]);
                    }
                    Some(
                        hasher
                            .finalize()
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                    )
                })
                .await
                .ok()
                .flatten();
                PairedTestIdentity {
                    machine_id,
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    protocol_version: PROTOCOL_CURRENT.to_string(),
                    platform: std::env::consts::OS.into(),
                    architecture: std::env::consts::ARCH.into(),
                    debug_assertions: cfg!(debug_assertions),
                    daemon_instance_id: self.paired_testing.instance_id.clone(),
                    binary_sha256,
                    source_revision: option_env!("BOUNDLESS_SOURCE_REVISION").map(str::to_owned),
                }
            })
            .await
            .clone()
    }

    pub(crate) fn take_diagnostic_probe(
        &self,
        peer_id: &str,
        session_id: u64,
        category: EvidenceCategory,
    ) -> Option<WireMessage> {
        let mut inner = self
            .paired_testing
            .inner
            .lock()
            .expect("diagnostic state poisoned");
        let pending = inner.pending.as_mut()?;
        if pending.peer_id != peer_id || pending.deadline <= Instant::now() {
            return None;
        }
        let payload = pending.payload.take()?;
        pending.sent_on = Some((session_id, category));
        Some(WireMessage::DiagnosticProbe {
            request_id: pending.request_id.clone(),
            payload,
        })
    }

    pub(crate) async fn diagnostic_probe_reply(
        &self,
        peer_id: &str,
        session_id: u64,
        request_id: String,
        payload: Vec<u8>,
    ) -> Option<WireMessage> {
        // TLS authentication occurs before dispatch; reject stale/superseded sessions as well.
        if request_id.len() != 36
            || uuid::Uuid::parse_str(&request_id).is_err()
            || !self
                .transport
                .is_active_transport_session(peer_id, session_id)
        {
            return None;
        }
        let status = {
            let mut inner = self
                .paired_testing
                .inner
                .lock()
                .expect("diagnostic state poisoned");
            match inner.authorize_probe(peer_id, payload.len(), Instant::now()) {
                Ok(()) => "ok",
                Err(reason) => {
                    // A non-consenting peer cannot induce an unbounded response/log flood.
                    if inner
                        .last_denial
                        .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
                    {
                        return None;
                    }
                    inner.last_denial = Some(Instant::now());
                    reason
                }
            }
        };
        let (payload, metadata_json) = if status == "ok" {
            #[cfg(test)]
            {
                // Per-daemon fault seam: pause an actual accepted TLS request before egress.
                // No global hook and no fabricated response or timing samples.
                let pause = self.paired_testing.reply_pause.lock().unwrap().take();
                if let Some((entered, resumed)) = pause {
                    let _ = entered.send(());
                    let _ = resumed.await;
                }
            }
            let identity = self.paired_test_identity().await;
            (payload, serde_json::to_string(&identity).ok()?)
        } else {
            (Vec::new(), String::new())
        };
        Some(WireMessage::DiagnosticReply {
            request_id,
            status: status.into(),
            payload,
            metadata_json,
            session_id,
        })
    }

    pub(crate) fn complete_diagnostic_probe(
        &self,
        peer_id: &str,
        session_id: u64,
        message: WireMessage,
    ) {
        let WireMessage::DiagnosticReply {
            request_id,
            status,
            payload,
            metadata_json,
            session_id: remote_session_id,
        } = message
        else {
            return;
        };
        if payload.len() > MAX_PROBE_BYTES || metadata_json.len() > 2048 || status.len() > 64 {
            return;
        }
        if !self
            .transport
            .is_active_transport_session(peer_id, session_id)
        {
            return;
        }
        let mut inner = self
            .paired_testing
            .inner
            .lock()
            .expect("diagnostic state poisoned");
        let Some(pending) = inner.pending.as_ref() else {
            return;
        };
        if pending.peer_id != peer_id
            || pending.request_id != request_id
            || pending.deadline <= Instant::now()
            || pending
                .sent_on
                .as_ref()
                .is_none_or(|(sent_id, _)| *sent_id != session_id)
        {
            return;
        }
        let pending = inner.pending.take().expect("matched diagnostic request");
        let (_, category) = pending.sent_on.expect("sent request has a session");
        let _ = pending.response.send(ProbeResponse {
            status,
            payload,
            remote: serde_json::from_str(&metadata_json).ok(),
            local_session_id: session_id,
            remote_session_id,
            category,
        });
    }

    async fn diagnostic_exchange(
        &self,
        peer_id: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<ProbeResponse> {
        let (send, receive) = oneshot::channel();
        {
            let mut inner = self
                .paired_testing
                .inner
                .lock()
                .expect("diagnostic state poisoned");
            anyhow::ensure!(
                inner.pending.is_none(),
                "diagnostic request already pending"
            );
            inner.pending = Some(Pending {
                peer_id: peer_id.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                payload: Some(payload),
                deadline: Instant::now() + timeout,
                sent_on: None,
                response: send,
            });
        }
        let _cleanup = PendingGuard(&self.paired_testing);
        self.transport.notify_outgoing_flush_signal();
        tokio::time::timeout(timeout, receive)
            .await
            .map_err(|_| anyhow::anyhow!("response_timeout"))?
            .map_err(|_| anyhow::anyhow!("request_cancelled"))
    }

    pub async fn run_paired_test(&self, options: PairedTestOptions) -> Result<PairedTestReport> {
        options.validate()?;
        self.ensure_diagnostic_peer(&options.peer_id).await?;
        anyhow::ensure!(
            self.has_active_transport_session(&options.peer_id),
            "peer is offline; connect both PCs running protocol {PROTOCOL_CURRENT}"
        );
        let _permit = self
            .paired_testing
            .run
            .try_acquire()
            .map_err(|_| anyhow::anyhow!("another paired test is running"))?;
        let local = self.paired_test_identity().await;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(MAX_RUN_SECONDS);
        let mut report = PairedTestReport {
            schema_version: 1,
            run_id: uuid::Uuid::new_v4().to_string(),
            started_at: Utc::now().to_rfc3339(),
            duration_ms: 0,
            local,
            remote: None,
            evidence_category: None,
            local_transport_session_id: None,
            remote_transport_session_id: None,
            passed: true,
            tests: Vec::new(),
            not_tested: [
                "physical_keyboard_mouse_injection",
                "emergency_unlock",
                "clipboard",
                "file_workflows",
                "reconnect_recovery",
                "cpu_memory_disk_budgets",
                "physical_two_pc_attestation",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        };
        // Two separate workloads use the live authenticated connection. No OS input, clipboard,
        // user data, files, shell commands, or network disconnections are produced.
        for (name, bytes) in [
            ("transport_rtt", 64),
            ("bulk_echo_integrity", options.payload_bytes),
        ] {
            let mut summary = ProbeSummary {
                name: name.into(),
                requested_samples: options.samples,
                completed_samples: 0,
                payload_bytes_per_sample: bytes,
                verified_round_trip_bytes: 0,
                latency_us: Vec::new(),
                p50_us: None,
                p95_us: None,
                errors: Vec::new(),
            };
            for sequence in 0..options.samples {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    summary.errors.push("run_deadline_exceeded".into());
                    break;
                };
                let payload = synthetic_payload(bytes as usize, sequence);
                let probe_started = Instant::now();
                let result = self
                    .diagnostic_exchange(
                        &options.peer_id,
                        payload.clone(),
                        remaining.min(Duration::from_millis(u64::from(options.timeout_ms))),
                    )
                    .await;
                match result {
                    Ok(response) => {
                        if response.status != "ok" {
                            summary
                                .errors
                                .push(format!("peer_rejected:{}", response.status));
                            break;
                        }
                        if response.payload != payload {
                            summary.errors.push("payload_integrity_mismatch".into());
                            break;
                        }
                        let Some(remote) = response.remote.filter(|remote| {
                            remote.machine_id == options.peer_id
                                && remote.protocol_version == PROTOCOL_CURRENT.to_string()
                        }) else {
                            summary.errors.push("invalid_peer_identity_metadata".into());
                            break;
                        };
                        if report
                            .remote
                            .as_ref()
                            .is_some_and(|previous| *previous != remote)
                            || report
                                .local_transport_session_id
                                .is_some_and(|id| id != response.local_session_id)
                            || report
                                .remote_transport_session_id
                                .is_some_and(|id| id != response.remote_session_id)
                        {
                            summary.errors.push("session_changed_during_run".into());
                            break;
                        }
                        report.remote = Some(remote);
                        report.evidence_category = Some(response.category);
                        report.local_transport_session_id = Some(response.local_session_id);
                        report.remote_transport_session_id = Some(response.remote_session_id);
                        summary.latency_us.push(
                            probe_started
                                .elapsed()
                                .as_micros()
                                .min(u128::from(u64::MAX)) as u64,
                        );
                    }
                    Err(error) => {
                        summary.errors.push(error.to_string());
                        break;
                    }
                }
            }
            summary.finish();
            let failed = !summary.errors.is_empty()
                || summary.completed_samples != summary.requested_samples;
            report.tests.push(summary);
            if failed {
                report.passed = false;
                break;
            }
        }
        report.duration_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }
}

fn synthetic_payload(bytes: usize, sequence: u32) -> Vec<u8> {
    (0..bytes)
        .map(|index| {
            ((index as u32)
                .wrapping_mul(31)
                .wrapping_add(sequence.wrapping_mul(17))
                % 251) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests;
