use super::*;
use app_services::paired_testing::{PairedTestConsent, PairedTestReport};
use ipc_api::boundless::v1::{PairedTestConsentRequest, PairedTestRunRequest};

#[derive(Debug, Subcommand)]
pub(super) enum PairedTestCommand {
    /// Permit only in-memory transport probes from this paired peer, temporarily.
    Allow {
        peer: String,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=600))]
        seconds: u32,
    },
    /// Revoke this daemon's paired-test permission immediately.
    Revoke,
    /// Show the current local permission and remaining budget.
    Status,
    /// Measure authenticated transport RTT and synthetic bulk echo integrity.
    /// Run `paired-test allow <this-PC-id>` on the other PC first.
    Run {
        peer: String,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
        samples: u32,
        #[arg(long, default_value_t = 65536, value_parser = clap::value_parser!(u32).range(1..=65536))]
        payload_bytes: u32,
        #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u32).range(100..=5000))]
        timeout_ms: u32,
    },
}

pub(super) async fn execute(
    endpoint: &str,
    command: PairedTestCommand,
    output: OutputFormat,
) -> Result<()> {
    let mut client = connect_control_plane(endpoint).await?;
    let (json, is_run) = match command {
        PairedTestCommand::Allow { peer, seconds } => {
            let reply = client
                .paired_test_consent(PairedTestConsentRequest {
                    peer_id: peer,
                    duration_seconds: seconds,
                })
                .await?
                .into_inner();
            (reply.json, false)
        }
        PairedTestCommand::Revoke => {
            let reply = client
                .paired_test_consent(PairedTestConsentRequest {
                    peer_id: String::new(),
                    duration_seconds: 0,
                })
                .await?
                .into_inner();
            (reply.json, false)
        }
        PairedTestCommand::Status => (
            client
                .get_paired_test_consent(Empty {})
                .await?
                .into_inner()
                .json,
            false,
        ),
        PairedTestCommand::Run {
            peer,
            samples,
            payload_bytes,
            timeout_ms,
        } => {
            let reply = tokio::time::timeout(
                Duration::from_secs(45),
                client.run_paired_test(PairedTestRunRequest {
                    peer_id: peer,
                    samples,
                    payload_bytes,
                    timeout_ms,
                }),
            )
            .await
            .context("paired test control request timed out")??
            .into_inner();
            (reply.json, true)
        }
    };
    if is_run {
        let report: PairedTestReport =
            serde_json::from_str(&json).context("parse paired test report")?;
        if output == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "Paired transport test: {}",
                if report.passed { "passed" } else { "failed" }
            );
            println!(
                "Run: {}  Evidence: {:?}",
                report.run_id, report.evidence_category
            );
            for test in &report.tests {
                println!(
                    "{}: {}/{} samples; p50={} us; p95={} us; {} bytes verified",
                    test.name,
                    test.completed_samples,
                    test.requested_samples,
                    test.p50_us.map_or("n/a".into(), |n| n.to_string()),
                    test.p95_us.map_or("n/a".into(), |n| n.to_string()),
                    test.verified_round_trip_bytes
                );
                for error in &test.errors {
                    println!("  {error}");
                }
            }
            println!("Not tested: {}", report.not_tested.join(", "));
        }
        anyhow::ensure!(
            report.passed,
            "paired transport test failed; see structured results"
        );
    } else {
        let consent: PairedTestConsent =
            serde_json::from_str(&json).context("parse paired test permission")?;
        if output == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&consent)?);
        } else if consent.enabled {
            println!(
                "In-memory transport probes allowed from {} for {} seconds ({} requests, {} request bytes remaining).",
                consent.peer_id.as_deref().unwrap_or("unknown"),
                consent.remaining_seconds,
                consent.remaining_requests,
                consent.remaining_bytes
            );
        } else {
            println!(
                "Paired transport tests are not allowed. Use paired-test allow <peer-id> to grant temporary permission."
            );
        }
    }
    Ok(())
}
