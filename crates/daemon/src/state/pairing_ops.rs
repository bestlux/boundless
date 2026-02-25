use super::*;

impl AppState {
    pub async fn create_pairing_code(&self, ttl_secs: u64) -> (String, DateTime<Utc>) {
        let code = generate_pairing_code(Duration::from_secs(ttl_secs));
        self.pairing_codes
            .write()
            .await
            .insert(code.value.clone(), code.expires_at);
        (code.value, code.expires_at)
    }

    pub async fn consume_pairing_code(&self, code: &str) -> Result<()> {
        let now = Utc::now();
        let mut pairing_codes = self.pairing_codes.write().await;
        validate_and_consume_pairing_code(&mut pairing_codes, code, now)?;
        pairing_codes.retain(|_, expires_at| *expires_at >= now);
        Ok(())
    }

    pub async fn queue_nearby_pairing_request(
        &self,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
    ) -> Result<PendingNearbyPairingRequest> {
        let mut pending_requests = self.pending_nearby_pairing_requests.write().await;
        if pending_requests.len() >= MAX_PENDING_NEARBY_PAIRING_REQUESTS {
            anyhow::bail!("too many pending pairing requests; try again later");
        }

        let summary = PendingNearbyPairingRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            requester_machine_id: requester_bundle.machine_id.clone(),
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
                mode: PendingNearbyPairingMode::ManualApproval,
            },
        );
        Ok(summary)
    }

    pub async fn queue_nearby_pairing_code_challenge(
        &self,
        requester_bundle: TrustBundle,
        requester_alias: Option<String>,
        ttl_secs: u64,
    ) -> Result<PendingNearbyPairingRequest> {
        let mut pending_requests = self.pending_nearby_pairing_requests.write().await;
        let requester_machine_id = requester_bundle.machine_id.clone();
        pending_requests.retain(|_, record| {
            !(record.summary.requester_machine_id == requester_machine_id
                && matches!(record.mode, PendingNearbyPairingMode::CodeChallenge { .. }))
        });

        let pending_code_challenge_count = pending_requests
            .values()
            .filter(|record| matches!(record.mode, PendingNearbyPairingMode::CodeChallenge { .. }))
            .count();
        if pending_code_challenge_count >= MAX_PENDING_NEARBY_CODE_CHALLENGES {
            anyhow::bail!("too many pending code confirmation requests; try again later");
        }
        if pending_requests.len() >= MAX_PENDING_NEARBY_PAIRING_REQUESTS {
            anyhow::bail!("too many pending pairing requests; try again later");
        }

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
        let mut last_seen = self.nearby_code_request_last_seen_by_ip.write().await;
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
        let mut lockouts = self.nearby_code_submission_lockout_by_ip.write().await;
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
            self.nearby_code_submission_failures_by_ip
                .write()
                .await
                .remove(&remote_ip);
            self.nearby_code_submission_lockout_by_ip
                .write()
                .await
                .remove(&remote_ip);
            return;
        }

        let now = Utc::now();
        let mut failures_map = self.nearby_code_submission_failures_by_ip.write().await;
        let window =
            chrono::TimeDelta::seconds(NEARBY_PAIRING_CODE_SUBMISSION_FAILURE_WINDOW_SECONDS);
        let failures = failures_map.entry(remote_ip).or_default();
        failures.retain(|timestamp| *timestamp + window >= now);
        failures.push(now);

        if failures.len() >= NEARBY_PAIRING_CODE_SUBMISSION_MAX_FAILURES {
            failures.clear();
            self.nearby_code_submission_lockout_by_ip
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
        self.expire_nearby_pairing_challenges().await;

        let mut requests = self
            .pending_nearby_pairing_requests
            .read()
            .await
            .values()
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    pub async fn nearby_pairing_status(&self, request_id: &str) -> NearbyPairingStatus {
        self.expire_nearby_pairing_challenges().await;

        if self
            .pending_nearby_pairing_requests
            .read()
            .await
            .contains_key(request_id)
        {
            return NearbyPairingStatus::Pending;
        }

        let now = Utc::now();
        let mut decisions = self.nearby_pairing_decisions.write().await;
        decisions.retain(|_, record| {
            record.decided_at
                + chrono::TimeDelta::minutes(NEARBY_PAIRING_DECISION_RETENTION_MINUTES)
                >= now
        });
        if let Some(record) = decisions.get(request_id) {
            return match &record.decision {
                NearbyPairingDecision::Approved { responder_bundle } => {
                    NearbyPairingStatus::Approved {
                        responder_bundle: responder_bundle.clone(),
                    }
                }
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
    ) -> Result<TrustBundle> {
        self.expire_nearby_pairing_challenges().await;

        let pending = {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .remove(request_id)
        }
        .ok_or_else(|| anyhow::anyhow!("nearby pairing request not found"))?;
        if matches!(pending.mode, PendingNearbyPairingMode::CodeChallenge { .. }) {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            anyhow::bail!("pairing request requires verification code confirmation");
        }
        let peer_id = pending.summary.requester_machine_id.clone();
        let effective_alias = alias_override
            .and_then(normalize_optional_alias)
            .or(pending.requester_alias.clone());

        if let Err(error) = self
            .import_trust_bundle(pending.requester_bundle.clone(), effective_alias)
            .await
        {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            return Err(error);
        }

        let responder_bundle = self.export_trust_bundle().await?;
        self.nearby_pairing_decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Approved {
                    responder_bundle: responder_bundle.clone(),
                },
                decided_at: Utc::now(),
            },
        );
        let _ = self
            .request_peer_reconnect_and_reset(&peer_id)
            .await
            .context("request reconnect after nearby pairing approval")?;
        Ok(responder_bundle)
    }

    pub async fn submit_nearby_pairing_code(
        &self,
        request_id: &str,
        code: &str,
        verification_nonce: &str,
        alias_override: Option<String>,
    ) -> Result<TrustBundle> {
        self.expire_nearby_pairing_challenges().await;

        let normalized_code = code.trim();
        if normalized_code.is_empty() {
            anyhow::bail!("verification code must not be empty");
        }
        let normalized_nonce = verification_nonce.trim();
        if normalized_nonce.is_empty() {
            anyhow::bail!("verification nonce must not be empty");
        }

        let mut pending = {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .remove(request_id)
        }
        .ok_or_else(|| anyhow::anyhow!("nearby pairing request not found"))?;

        let now = Utc::now();
        let mut invalid_attempts_remaining: Option<u8> = None;
        match &mut pending.mode {
            PendingNearbyPairingMode::ManualApproval => {
                self.pending_nearby_pairing_requests
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
                    self.nearby_pairing_decisions.write().await.insert(
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
                if !code.eq_ignore_ascii_case(normalized_code) || nonce != normalized_nonce {
                    if *attempts_left > 1 {
                        *attempts_left -= 1;
                        invalid_attempts_remaining = Some(*attempts_left);
                    } else {
                        invalid_attempts_remaining = Some(0);
                    }
                }
            }
        };

        if let Some(attempts_remaining) = invalid_attempts_remaining {
            if attempts_remaining > 0 {
                self.pending_nearby_pairing_requests
                    .write()
                    .await
                    .insert(request_id.to_string(), pending);
                anyhow::bail!(
                    "verification code is invalid; attempts_remaining={attempts_remaining}"
                );
            }

            self.nearby_pairing_decisions.write().await.insert(
                request_id.to_string(),
                NearbyPairingDecisionRecord {
                    decision: NearbyPairingDecision::Rejected {
                        message: "verification code rejected: too many attempts".to_string(),
                    },
                    decided_at: now,
                },
            );
            anyhow::bail!("verification code is invalid; pairing request rejected");
        }

        let peer_id = pending.summary.requester_machine_id.clone();
        let effective_alias = alias_override
            .and_then(normalize_optional_alias)
            .or(pending.requester_alias.clone());

        if let Err(error) = self
            .import_trust_bundle(pending.requester_bundle.clone(), effective_alias)
            .await
        {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .insert(request_id.to_string(), pending);
            return Err(error);
        }

        let responder_bundle = self.export_trust_bundle().await?;
        self.nearby_pairing_decisions.write().await.insert(
            request_id.to_string(),
            NearbyPairingDecisionRecord {
                decision: NearbyPairingDecision::Approved {
                    responder_bundle: responder_bundle.clone(),
                },
                decided_at: Utc::now(),
            },
        );
        let _ = self
            .request_peer_reconnect_and_reset(&peer_id)
            .await
            .context("request reconnect after nearby pairing code confirmation")?;
        Ok(responder_bundle)
    }

    pub async fn reject_nearby_pairing_request(&self, request_id: &str) -> bool {
        self.expire_nearby_pairing_challenges().await;

        let removed = self
            .pending_nearby_pairing_requests
            .write()
            .await
            .remove(request_id);
        if removed.is_none() {
            return false;
        }

        self.nearby_pairing_decisions.write().await.insert(
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

    async fn expire_nearby_pairing_challenges(&self) {
        let now = Utc::now();
        let mut expired_ids = Vec::<String>::new();

        {
            let mut pending = self.pending_nearby_pairing_requests.write().await;
            pending.retain(|request_id, record| match &record.mode {
                PendingNearbyPairingMode::ManualApproval => true,
                PendingNearbyPairingMode::CodeChallenge { expires_at, .. } => {
                    let keep = *expires_at >= now;
                    if !keep {
                        expired_ids.push(request_id.clone());
                    }
                    keep
                }
            });
        }

        if expired_ids.is_empty() {
            return;
        }

        let mut decisions = self.nearby_pairing_decisions.write().await;
        for request_id in expired_ids {
            decisions.insert(
                request_id,
                NearbyPairingDecisionRecord {
                    decision: NearbyPairingDecision::Rejected {
                        message: "verification code expired".to_string(),
                    },
                    decided_at: now,
                },
            );
        }
    }
}
