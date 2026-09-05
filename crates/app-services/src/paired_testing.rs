//! Stable, content-free evidence from explicitly permitted paired transport tests.
use serde::{Deserialize, Serialize};

pub const MAX_LEASE_SECONDS: u32 = 600;
pub const MAX_PROBE_BYTES: usize = 64 * 1024;
pub const MAX_LEASE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_LEASE_REQUESTS: u32 = 256;
pub const MAX_SAMPLES: u32 = 100;
pub const MAX_RUN_SECONDS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedTestOptions {
    pub peer_id: String,
    pub samples: u32,
    pub payload_bytes: u32,
    pub timeout_ms: u32,
}

impl PairedTestOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.peer_id.is_empty() && self.peer_id.len() <= 128,
            "invalid peer id"
        );
        anyhow::ensure!(
            (1..=MAX_SAMPLES).contains(&self.samples),
            "samples must be 1..={MAX_SAMPLES}"
        );
        anyhow::ensure!(
            (1..=MAX_PROBE_BYTES).contains(&(self.payload_bytes as usize)),
            "payload bytes must be 1..={MAX_PROBE_BYTES}"
        );
        anyhow::ensure!(
            (100..=5000).contains(&self.timeout_ms),
            "timeout must be 100..=5000 ms"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairedTestConsent {
    pub schema_version: u32,
    pub peer_id: Option<String>,
    pub enabled: bool,
    pub remaining_seconds: u32,
    pub remaining_requests: u32,
    /// Request payload budget. Responses echo at most this many additional bytes.
    pub remaining_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCategory {
    /// Actual authenticated TCP/TLS exchange via a loopback socket.
    Loopback,
    /// Actual authenticated TCP/TLS exchange via a non-loopback socket.
    /// This does not attest that the endpoints are different physical PCs.
    RealPaired,
    /// In-memory post-authentication fixture, never hardware evidence.
    Synthetic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedTestIdentity {
    pub machine_id: String,
    pub daemon_version: String,
    pub protocol_version: String,
    pub platform: String,
    pub architecture: String,
    pub debug_assertions: bool,
    pub process_id: u32,
    /// Exact, non-invasive operations this daemon exposes under the diagnostic lease.
    pub capabilities: Vec<String>,
    pub daemon_instance_id: String,
    pub binary_sha256: Option<String>,
    /// Set only when supplied by the build; absence is explicit, never inferred from a checkout.
    pub source_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSummary {
    pub name: String,
    pub requested_samples: u32,
    pub completed_samples: u32,
    pub payload_bytes_per_sample: u32,
    pub verified_round_trip_bytes: u64,
    /// End-to-end daemon queue + transport + remote handling, measured on one monotonic clock.
    pub latency_us: Vec<u64>,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub errors: Vec<String>,
}

impl ProbeSummary {
    pub fn finish(&mut self) {
        self.completed_samples = self.latency_us.len() as u32;
        self.verified_round_trip_bytes =
            u64::from(self.completed_samples) * u64::from(self.payload_bytes_per_sample) * 2;
        let mut sorted = self.latency_us.clone();
        sorted.sort_unstable();
        let percentile = |percent: usize| {
            sorted
                .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
                .copied()
        };
        self.p50_us = percentile(50);
        self.p95_us = percentile(95);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedTestReport {
    pub schema_version: u32,
    pub run_id: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub local: PairedTestIdentity,
    pub remote: Option<PairedTestIdentity>,
    pub evidence_category: Option<EvidenceCategory>,
    pub local_transport_session_id: Option<u64>,
    pub remote_transport_session_id: Option<u64>,
    pub passed: bool,
    pub tests: Vec<ProbeSummary>,
    pub not_tested: Vec<String>,
}
