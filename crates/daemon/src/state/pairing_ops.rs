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
    ) -> PendingNearbyPairingRequest {
        let summary = PendingNearbyPairingRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            requester_machine_id: requester_bundle.machine_id.clone(),
            requester_display_name: requester_bundle.display_name.clone(),
            created_at: Utc::now(),
        };
        let request_id = summary.request_id.clone();

        self.pending_nearby_pairing_requests.write().await.insert(
            request_id,
            PendingNearbyPairingRequestRecord {
                summary: summary.clone(),
                requester_bundle,
                requester_alias: requester_alias.and_then(normalize_optional_alias),
            },
        );
        summary
    }

    pub async fn list_pending_nearby_pairing_requests(&self) -> Vec<PendingNearbyPairingRequest> {
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
        let pending = {
            self.pending_nearby_pairing_requests
                .write()
                .await
                .remove(request_id)
        }
        .ok_or_else(|| anyhow::anyhow!("nearby pairing request not found"))?;
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
        self.request_peer_reconnect(&peer_id).await;
        Ok(responder_bundle)
    }

    pub async fn reject_nearby_pairing_request(&self, request_id: &str) -> bool {
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
}
