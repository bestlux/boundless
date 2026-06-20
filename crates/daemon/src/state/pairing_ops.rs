use super::*;

const PAIRING_RECONNECT_STATUS_CONNECTIVITY_PENDING: &str = "connectivity_pending";
const PAIRING_RECONNECT_STATUS_FAILED: &str = "reconnect_failed";

#[derive(Debug, Clone, Copy)]
enum VerificationMismatchKind {
    Code,
    Nonce,
    CodeAndNonce,
}

impl VerificationMismatchKind {
    fn invalid_message(self) -> &'static str {
        match self {
            VerificationMismatchKind::Code => "verification code is invalid",
            VerificationMismatchKind::Nonce => "verification nonce is invalid",
            VerificationMismatchKind::CodeAndNonce => "verification code and nonce are invalid",
        }
    }

    fn rejection_message(self) -> &'static str {
        match self {
            VerificationMismatchKind::Code => "verification code rejected",
            VerificationMismatchKind::Nonce => "verification nonce rejected",
            VerificationMismatchKind::CodeAndNonce => "verification code and nonce rejected",
        }
    }
}

impl AppState {
    pub async fn create_pairing_code(&self, ttl_secs: u64) -> (String, chrono::DateTime<Utc>) {
        let code = generate_pairing_code(Duration::from_secs(ttl_secs));
        self.pairing
            .pairing_codes
            .write()
            .await
            .insert(code.value.clone(), code.expires_at);
        (code.value, code.expires_at)
    }

    pub async fn consume_pairing_code(&self, code: &str) -> Result<()> {
        let now = Utc::now();
        let mut pairing_codes = self.pairing.pairing_codes.write().await;
        validate_and_consume_pairing_code(&mut pairing_codes, code, now)?;
        pairing_codes.retain(|_, expires_at| *expires_at >= now);
        Ok(())
    }

    pub async fn queue_nearby_pairing_request(
        &self,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
        source_ip: IpAddr,
    ) -> Result<PendingNearbyPairingRequest> {
        self.ensure_trust_rotation_not_pending()?;
        self.expire_nearby_pairing_requests().await;
        let mut pending_requests = self.pairing.pending_requests.write().await;
        self.ensure_trust_rotation_not_pending()?;
        let requester_machine_id = requester_bundle.machine_id.clone();
        if let Some(existing) = find_pending_nearby_pairing_request(
            &pending_requests,
            &requester_machine_id,
            source_ip,
            PendingNearbyPairingAdmissionMode::ManualApproval,
        ) {
            return Ok(existing.summary.clone());
        }
        ensure_nearby_pairing_admission_capacity(
            &pending_requests,
            &requester_machine_id,
            source_ip,
            PendingNearbyPairingAdmissionMode::ManualApproval,
        )?;

        let summary = PendingNearbyPairingRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            requester_machine_id,
            requester_display_name: requester_bundle.display_name.clone(),
            created_at: Utc::now(),
            verification_code: None,
            verification_nonce: None,
            verification_expires_at: None,
        };
        let request_id = summary.request_id.clone();

        pending_requests.insert(
            request_id,
            PendingNearbyPairingRequestRecord {
                summary: summary.clone(),
                requester_bundle,
                requester_alias: requester_alias.and_then(normalize_optional_alias),
                source_ip,
                mode: PendingNearbyPairingMode::ManualApproval,
            },
        );
        Ok(summary)
    }

    pub async fn queue_nearby_pairing_code_challenge(
        &self,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
        source_ip: IpAddr,
        ttl_secs: u64,
    ) -> Result<PendingNearbyPairingRequest> {
        self.ensure_trust_rotation_not_pending()?;
        self.expire_nearby_pairing_requests().await;
        let mut pending_requests = self.pairing.pending_requests.write().await;
        self.ensure_trust_rotation_not_pending()?;
        let requester_machine_id = requester_bundle.machine_id.clone();
        if let Some(existing) = find_pending_nearby_pairing_request(
            &pending_requests,
            &requester_machine_id,
            source_ip,
            PendingNearbyPairingAdmissionMode::CodeChallenge,
        ) {
            return Ok(existing.summary.clone());
        }
        ensure_nearby_pairing_admission_capacity(
            &pending_requests,
            &requester_machine_id,
            source_ip,
            PendingNearbyPairingAdmissionMode::CodeChallenge,
        )?;

        let code = generate_pairing_code(Duration::from_secs(ttl_secs.max(30)));
        let verification_nonce = uuid::Uuid::new_v4().simple().to_string();
        let summary = PendingNearbyPairingRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            requester_machine_id: requester_bundle.machine_id.clone(),
            requester_display_name: requester_bundle.display_name.clone(),
            created_at: Utc::now(),
            verification_code: Some(code.value.clone()),
            verification_nonce: Some(verification_nonce.clone()),
            verification_expires_at: Some(code.expires_at),
        };
        let request_id = summary.request_id.clone();

        pending_requests.insert(
            request_id,
            PendingNearbyPairingRequestRecord {
                summary: summary.clone(),
                requester_bundle,
                requester_alias: requester_alias.and_then(normalize_optional_alias),
                source_ip,
                mode: PendingNearbyPairingMode::CodeChallenge {
                    code: code.value,
                    nonce: verification_nonce,
                    expires_at: code.expires_at,
                    attempts_left: NEARBY_PAIRING_CHALLENGE_MAX_ATTEMPTS,
                },
            },
        );
        Ok(summary)
    }

    pub async fn validate_nearby_code_request_rate_limit(&self, remote_ip: IpAddr) -> Result<()> {
        let now = Utc::now();
        let mut last_seen = self.pairing.code_request_last_seen_by_ip.write().await;
        last_seen.retain(|_, seen_at| *seen_at + chrono::TimeDelta::seconds(60) >= now);

        if let Some(previous) = last_seen.get(&remote_ip)
            && *previous + chrono::TimeDelta::seconds(NEARBY_PAIRING_CODE_REQUEST_COOLDOWN_SECONDS)
                > now
        {
            anyhow::bail!("nearby code request rate limited; retry in a few seconds");
        }

        last_seen.insert(remote_ip, now);
        Ok(())
    }

    pub async fn validate_nearby_code_submission_allowed(&self, remote_ip: IpAddr) -> Result<()> {
        let now = Utc::now();
        let mut lockouts = self.pairing.code_submission_lockout_by_ip.write().await;
        lockouts.retain(|_, lockout_until| *lockout_until >= now);

        if let Some(lockout_until) = lockouts.get(&remote_ip)
            && *lockout_until >= now
        {
            anyhow::bail!(
                "verification temporarily locked after repeated invalid attempts; retry later"
            );
        }
        Ok(())
    }

    pub async fn record_nearby_code_submission_result(&self, remote_ip: IpAddr, success: bool) {
        if success {
            self.pairing
                .code_submission_failures_by_ip
                .write()
                .await
                .remove(&remote_ip);
            self.pairing
                .code_submission_lockout_by_ip
                .write()
                .await
                .remove(&remote_ip);
            return;
        }

        let now = Utc::now();
        let mut failures_map = self.pairing.code_submission_failures_by_ip.write().await;
        let window =
            chrono::TimeDelta::seconds(NEARBY_PAIRING_CODE_SUBMISSION_FAILURE_WINDOW_SECONDS);
        let failures = failures_map.entry(remote_ip).or_default();
        failures.retain(|timestamp| *timestamp + window >= now);
        failures.push(now);

        if failures.len() >= NEARBY_PAIRING_CODE_SUBMISSION_MAX_FAILURES {
            failures.clear();
            self.pairing
                .code_submission_lockout_by_ip
                .write()
                .await
                .insert(
                    remote_ip,
                    now + chrono::TimeDelta::seconds(
                        NEARBY_PAIRING_CODE_SUBMISSION_LOCKOUT_SECONDS,
                    ),
                );
        }
    }

    pub async fn list_pending_nearby_pairing_requests(&self) -> Vec<PendingNearbyPairingRequest> {
        self.expire_nearby_pairing_requests().await;

        let mut requests = self
            .pairing
            .pending_requests
            .read()
            .await
            .values()
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    pub async fn nearby_pairing_status(&self, request_id: &str) -> NearbyPairingStatus {
        self.expire_nearby_pairing_requests().await;

        if self
            .pairing
            .pending_requests
            .read()
            .await
            .contains_key(request_id)
        {
            return NearbyPairingStatus::Pending;
        }

        let now = Utc::now();
        let mut decisions = self.pairing.decisions.write().await;
        decisions.retain(|_, record| {
            record.decided_at
                + chrono::TimeDelta::minutes(NEARBY_PAIRING_DECISION_RETENTION_MINUTES)
                >= now
        });
        if let Some(record) = decisions.get(request_id) {
            return match &record.decision {
                NearbyPairingDecision::Approved {
                    responder_bundle,
                    peer_machine_id,
                    reconnect_status,
                    message,
                } => NearbyPairingStatus::Approved {
                    responder_bundle: responder_bundle.clone(),
                    peer_machine_id: peer_machine_id.clone(),
                    reconnect_status: reconnect_status.clone(),
                    message: message.clone(),
                },
                NearbyPairingDecision::Rejected { message } => NearbyPairingStatus::Rejected {
                    message: message.clone(),
                },
            };
        }

        NearbyPairingStatus::Missing
    }

    pub async fn approve_nearby_pairing_request(
        &self,
        request_id: &str,
        alias_override: Option<String>,
    ) -> Result<NearbyPairingCommitResult> {
        self.ensure_trust_rotation_not_pending()?;
        self.expire_nearby_pairing_requests().await;

        let pending = match self
            .pairing
            .pending_requests
            .write()
            .await
            .remove(request_id)
        {
            Some(pending) => pending,
            None => {
                if let Some(result) = self.approved_pairing_commit_result(request_id, true).await {
                    return Ok(result);
                }
                anyhow::bail!("nearby pairing request not found");
            }
        };
        if matches!(pending.mode, PendingNearbyPairingMode::CodeChallenge { .. }) {
            self.pairing
                .pending_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            anyhow::bail!("pairing request requires verification code confirmation");
        }
        let peer_id = pending.summary.requester_machine_id.clone();
        let effective_alias = alias_override
            .and_then(normalize_optional_alias)
            .or(pending.requester_alias.clone());
        let responder_bundle = self.export_trust_bundle().await?;

        if let Err(error) = self
            .import_trust_bundle(pending.requester_bundle.clone(), effective_alias)
            .await
        {
            self.pairing
                .pending_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            return Err(error);
        }

        Ok(self
            .finish_pairing_trust_commit(request_id, peer_id, responder_bundle, false)
            .await)
    }

    pub async fn submit_nearby_pairing_code(
        &self,
        request_id: &str,
        code: &str,
        verification_nonce: &str,
        alias_override: Option<String>,
    ) -> Result<NearbyPairingCommitResult> {
        self.ensure_trust_rotation_not_pending()?;
        self.expire_nearby_pairing_requests().await;

        let normalized_code = code.trim();
        if normalized_code.is_empty() {
            anyhow::bail!("verification code must not be empty");
        }
        let normalized_nonce = verification_nonce.trim();
        if normalized_nonce.is_empty() {
            anyhow::bail!("verification nonce must not be empty");
        }

        let mut pending = match self
            .pairing
            .pending_requests
            .write()
            .await
            .remove(request_id)
        {
            Some(pending) => pending,
            None => {
                if let Some(result) = self.approved_pairing_commit_result(request_id, true).await {
                    return Ok(result);
                }
                anyhow::bail!("nearby pairing request not found");
            }
        };

        let now = Utc::now();
        let mut invalid_attempt: Option<(u8, VerificationMismatchKind)> = None;
        match &mut pending.mode {
            PendingNearbyPairingMode::ManualApproval => {
                self.pairing
                    .pending_requests
                    .write()
                    .await
                    .insert(request_id.to_string(), pending);
                anyhow::bail!("pairing request requires manual approval");
            }
            PendingNearbyPairingMode::CodeChallenge {
                code,
                nonce,
                expires_at,
                attempts_left,
            } => {
                if *expires_at < now {
                    self.pairing.decisions.write().await.insert(
                        request_id.to_string(),
                        NearbyPairingDecisionRecord {
                            decision: NearbyPairingDecision::Rejected {
                                message: "verification code expired".to_string(),
                            },
                            decided_at: now,
                        },
                    );
                    anyhow::bail!("verification code expired");
                }
                let mismatch = match (
                    code.eq_ignore_ascii_case(normalized_code),
                    nonce == normalized_nonce,
                ) {
                    (true, true) => None,
                    (false, true) => Some(VerificationMismatchKind::Code),
                    (true, false) => Some(VerificationMismatchKind::Nonce),
                    (false, false) => Some(VerificationMismatchKind::CodeAndNonce),
                };
                if let Some(kind) = mismatch {
                    if *attempts_left > 1 {
                        *attempts_left -= 1;
                        invalid_attempt = Some((*attempts_left, kind));
                    } else {
                        invalid_attempt = Some((0, kind));
                    }
                }
            }
        };

        if let Some((attempts_remaining, mismatch_kind)) = invalid_attempt {
            if attempts_remaining > 0 {
                self.pairing
                    .pending_requests
                    .write()
                    .await
                    .insert(request_id.to_string(), pending);
                anyhow::bail!(
                    "{}; attempts_remaining={attempts_remaining}",
                    mismatch_kind.invalid_message()
                );
            }

            self.pairing.decisions.write().await.insert(
                request_id.to_string(),
                NearbyPairingDecisionRecord {
                    decision: NearbyPairingDecision::Rejected {
                        message: format!(
                            "{}: too many attempts",
                            mismatch_kind.rejection_message()
                        ),
                    },
                    decided_at: now,
                },
            );
            anyhow::bail!(
                "{}; pairing request rejected",
                mismatch_kind.invalid_message()
            );
        }

        let peer_id = pending.summary.requester_machine_id.clone();
        let effective_alias = alias_override
            .and_then(normalize_optional_alias)
            .or(pending.requester_alias.clone());
        let responder_bundle = self.export_trust_bundle().await?;

        if let Err(error) = self
            .import_trust_bundle(pending.requester_bundle.clone(), effective_alias)
            .await
        {
            self.pairing
                .pending_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            return Err(error);
        }

        Ok(self
            .finish_pairing_trust_commit(request_id, peer_id, responder_bundle, false)
            .await)
    }

    async fn approved_pairing_commit_result(
        &self,
        request_id: &str,
        already_committed: bool,
    ) -> Option<NearbyPairingCommitResult> {
        let decisions = self.pairing.decisions.read().await;
        let record = decisions.get(request_id)?;
        let NearbyPairingDecision::Approved {
            responder_bundle,
            peer_machine_id,
            reconnect_status,
            ..
        } = &record.decision
        else {
            return None;
        };
        Some(NearbyPairingCommitResult {
            responder_bundle: responder_bundle.clone(),
            peer_machine_id: peer_machine_id.clone(),
            trust_committed: true,
            already_committed,
            reconnect_status: reconnect_status.clone(),
            message: pairing_commit_message(already_committed, reconnect_status),
        })
    }

    async fn finish_pairing_trust_commit(
        &self,
        request_id: &str,
        peer_id: String,
        responder_bundle: TrustBundle,
        already_committed: bool,
    ) -> NearbyPairingCommitResult {
        let provisional_status = PAIRING_RECONNECT_STATUS_CONNECTIVITY_PENDING.to_string();
        let provisional_message = pairing_commit_message(already_committed, &provisional_status);
        self.record_approved_pairing_decision(
            request_id,
            responder_bundle.clone(),
            peer_id.clone(),
            provisional_status,
            provisional_message,
        )
        .await;

        let reconnect_status = self.request_pairing_reconnect_after_commit(&peer_id).await;
        let message = pairing_commit_message(already_committed, &reconnect_status);
        self.record_approved_pairing_decision(
            request_id,
            responder_bundle.clone(),
            peer_id.clone(),
            reconnect_status.clone(),
            message.clone(),
        )
        .await;

        NearbyPairingCommitResult {
            responder_bundle,
            peer_machine_id: peer_id,
            trust_committed: true,
            already_committed,
            reconnect_status,
            message,
        }
    }

    async fn record_approved_pairing_decision(
        &self,
        request_id: &str,
        responder_bundle: TrustBundle,
        peer_machine_id: String,
        reconnect_status: String,
        message: String,
    ) {
        self.pairing.decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Approved {
                    responder_bundle,
                    peer_machine_id,
                    reconnect_status,
                    message,
                },
                decided_at: Utc::now(),
            },
        );
    }

    async fn request_pairing_reconnect_after_commit(&self, peer_id: &str) -> String {
        match self.request_peer_reconnect_and_reset(peer_id).await {
            Ok((generation, aborted_sessions)) => {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "pairing_connectivity_pending".to_string(),
                    peer_id: peer_id.to_string(),
                    detail: format!(
                        "trust_committed=true generation={generation} aborted_sessions={aborted_sessions}"
                    ),
                    size_bytes: 0,
                });
                PAIRING_RECONNECT_STATUS_CONNECTIVITY_PENDING.to_string()
            }
            Err(error) => {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "pairing_reconnect_failed".to_string(),
                    peer_id: peer_id.to_string(),
                    detail: format!("trust_committed=true error={error}"),
                    size_bytes: 0,
                });
                PAIRING_RECONNECT_STATUS_FAILED.to_string()
            }
        }
    }

    pub async fn reject_nearby_pairing_request(&self, request_id: &str) -> bool {
        self.expire_nearby_pairing_requests().await;

        let removed = self
            .pairing
            .pending_requests
            .write()
            .await
            .remove(request_id);
        if removed.is_none() {
            return false;
        }

        self.pairing.decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Rejected {
                    message: "nearby pairing request rejected".to_string(),
                },
                decided_at: Utc::now(),
            },
        );
        true
    }

    async fn expire_nearby_pairing_requests(&self) {
        let now = Utc::now();

        let expired = {
            let mut pending = self.pairing.pending_requests.write().await;
            prune_expired_nearby_pairing_requests(&mut pending, now)
        };

        if expired.is_empty() {
            return;
        }

        let mut decisions = self.pairing.decisions.write().await;
        for (request_id, message) in expired {
            decisions.insert(
                request_id,
                NearbyPairingDecisionRecord {
                    decision: NearbyPairingDecision::Rejected { message },
                    decided_at: now,
                },
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingNearbyPairingAdmissionMode {
    ManualApproval,
    CodeChallenge,
}

fn find_pending_nearby_pairing_request<'a>(
    pending_requests: &'a HashMap<String, PendingNearbyPairingRequestRecord>,
    requester_machine_id: &str,
    source_ip: IpAddr,
    mode: PendingNearbyPairingAdmissionMode,
) -> Option<&'a PendingNearbyPairingRequestRecord> {
    pending_requests.values().find(|record| {
        record.summary.requester_machine_id == requester_machine_id
            && record.source_ip == source_ip
            && pairing_mode_matches(&record.mode, mode)
    })
}

fn ensure_nearby_pairing_admission_capacity(
    pending_requests: &HashMap<String, PendingNearbyPairingRequestRecord>,
    requester_machine_id: &str,
    source_ip: IpAddr,
    mode: PendingNearbyPairingAdmissionMode,
) -> Result<()> {
    let pending_for_peer = pending_requests
        .values()
        .filter(|record| record.summary.requester_machine_id == requester_machine_id)
        .count();
    if pending_for_peer >= MAX_PENDING_NEARBY_PAIRING_REQUESTS_PER_PEER {
        anyhow::bail!("too many pending pairing requests for this peer; try again later");
    }

    let pending_for_source = pending_requests
        .values()
        .filter(|record| record.source_ip == source_ip)
        .count();
    if pending_for_source >= MAX_PENDING_NEARBY_PAIRING_REQUESTS_PER_SOURCE {
        anyhow::bail!("too many pending pairing requests from this source; try again later");
    }

    if matches!(mode, PendingNearbyPairingAdmissionMode::CodeChallenge) {
        let pending_code_challenge_count = pending_requests
            .values()
            .filter(|record| matches!(record.mode, PendingNearbyPairingMode::CodeChallenge { .. }))
            .count();
        if pending_code_challenge_count >= MAX_PENDING_NEARBY_CODE_CHALLENGES {
            anyhow::bail!("too many pending code confirmation requests; try again later");
        }
    }

    if pending_requests.len() >= MAX_PENDING_NEARBY_PAIRING_REQUESTS {
        anyhow::bail!("too many pending pairing requests; try again later");
    }

    Ok(())
}

fn pairing_mode_matches(
    record_mode: &PendingNearbyPairingMode,
    admission_mode: PendingNearbyPairingAdmissionMode,
) -> bool {
    matches!(
        (record_mode, admission_mode),
        (
            PendingNearbyPairingMode::ManualApproval,
            PendingNearbyPairingAdmissionMode::ManualApproval
        ) | (
            PendingNearbyPairingMode::CodeChallenge { .. },
            PendingNearbyPairingAdmissionMode::CodeChallenge
        )
    )
}

fn prune_expired_nearby_pairing_requests(
    pending_requests: &mut HashMap<String, PendingNearbyPairingRequestRecord>,
    now: chrono::DateTime<Utc>,
) -> Vec<(String, String)> {
    let mut expired = Vec::new();
    pending_requests.retain(|request_id, record| {
        let rejection_message = match &record.mode {
            PendingNearbyPairingMode::ManualApproval => (record.summary.created_at
                + chrono::TimeDelta::seconds(NEARBY_PAIRING_PENDING_REQUEST_TTL_SECONDS)
                < now)
                .then_some("pairing request expired"),
            PendingNearbyPairingMode::CodeChallenge { expires_at, .. } => {
                (*expires_at < now).then_some("verification code expired")
            }
        };
        if let Some(message) = rejection_message {
            expired.push((request_id.clone(), message.to_string()));
            false
        } else {
            true
        }
    });
    expired
}

fn pairing_commit_message(already_committed: bool, reconnect_status: &str) -> String {
    let trust = if already_committed {
        "nearby pairing already trusted"
    } else {
        "nearby pairing trust established"
    };
    match reconnect_status {
        PAIRING_RECONNECT_STATUS_FAILED => {
            format!("{trust}; reconnect request failed; use reconnect or remove peer to re-pair")
        }
        PAIRING_RECONNECT_STATUS_CONNECTIVITY_PENDING => {
            format!("{trust}; connectivity pending")
        }
        _ => trust.to_string(),
    }
}
