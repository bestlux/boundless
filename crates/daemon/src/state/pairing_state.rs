use std::{collections::HashMap, net::IpAddr};

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use core_security::TrustBundle;

#[derive(Debug, Clone)]
pub struct PendingNearbyPairingRequest {
    pub request_id: String,
    pub requester_machine_id: String,
    pub requester_display_name: String,
    pub created_at: DateTime<Utc>,
    pub verification_code: Option<String>,
    pub verification_nonce: Option<String>,
    pub verification_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub enum NearbyPairingStatus {
    Pending,
    Approved { responder_bundle: TrustBundle },
    Rejected { message: String },
    Missing,
}

#[derive(Debug, Clone)]
pub(super) struct PendingNearbyPairingRequestRecord {
    pub(super) summary: PendingNearbyPairingRequest,
    pub(super) requester_bundle: TrustBundle,
    pub(super) requester_alias: Option<String>,
    pub(super) mode: PendingNearbyPairingMode,
}

#[derive(Debug, Clone)]
pub(super) enum PendingNearbyPairingMode {
    ManualApproval,
    CodeChallenge {
        code: String,
        nonce: String,
        expires_at: DateTime<Utc>,
        attempts_left: u8,
    },
}

#[derive(Debug, Clone)]
pub(super) enum NearbyPairingDecision {
    Approved { responder_bundle: TrustBundle },
    Rejected { message: String },
}

#[derive(Debug, Clone)]
pub(super) struct NearbyPairingDecisionRecord {
    pub(super) decision: NearbyPairingDecision,
    pub(super) decided_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub(super) struct PairingState {
    pub(super) pairing_codes: RwLock<HashMap<String, DateTime<Utc>>>,
    pub(super) pending_requests: RwLock<HashMap<String, PendingNearbyPairingRequestRecord>>,
    pub(super) decisions: RwLock<HashMap<String, NearbyPairingDecisionRecord>>,
    pub(super) code_request_last_seen_by_ip: RwLock<HashMap<IpAddr, DateTime<Utc>>>,
    pub(super) code_submission_failures_by_ip: RwLock<HashMap<IpAddr, Vec<DateTime<Utc>>>>,
    pub(super) code_submission_lockout_by_ip: RwLock<HashMap<IpAddr, DateTime<Utc>>>,
}

impl PairingState {
    pub(super) async fn clear(&self) {
        self.pairing_codes.write().await.clear();
        self.pending_requests.write().await.clear();
        self.decisions.write().await.clear();
        self.code_request_last_seen_by_ip.write().await.clear();
        self.code_submission_failures_by_ip.write().await.clear();
        self.code_submission_lockout_by_ip.write().await.clear();
    }
}
