//! Real TCP/TLS fixtures. These deliberately report loopback, never physical-PC evidence.
use app_services::paired_testing::{EvidenceCategory, MAX_PROBE_BYTES, PairedTestOptions};
use core_security::{SecurityPaths, TrustRecord, upsert_trust_record};
use tokio::task::JoinHandle;

use super::*;

#[test]
fn paired_testing_mapped_loopback_cannot_masquerade_as_non_loopback_evidence() {
    for address in ["127.1.2.3", "::1", "::ffff:127.0.0.1"] {
        assert_eq!(
            session::diagnostic_peer_address_category(address.parse().unwrap()),
            EvidenceCategory::Loopback
        );
    }
    for address in ["192.0.2.1", "2001:db8::1", "::ffff:192.0.2.1"] {
        assert_eq!(
            session::diagnostic_peer_address_category(address.parse().unwrap()),
            EvidenceCategory::RealPaired
        );
    }
}

struct Fixture {
    state: AppState,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new(machine_id: &str, peer_id: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "boundless-paired-functional-{}",
            uuid::Uuid::new_v4()
        ));
        let mut config = crate::config::RuntimeConfig {
            machine_id: machine_id.into(),
            device_name: "paired fixture".into(),
            ..Default::default()
        };
        config.file_transfer.receive_dir = root.join("inbox").to_string_lossy().into_owned();
        config.features.insert("share_clipboard".into(), false);
        config.features.insert("share_input".into(), false);
        config.peers.push(crate::config::PeerConfig {
            peer_id: peer_id.into(),
            display_name: "fixture peer".into(),
            address: "127.0.0.1:1".into(),
            connected: false,
            last_seen: chrono::Utc::now(),
        });
        crate::config::save_config_at(&root.join("config.json"), &config).unwrap();
        let state =
            AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
                .unwrap();
        Self { state, root }
    }

    fn trust(&self, other: &Self, machine_id: &str) {
        upsert_trust_record(
            &SecurityPaths::for_root(self.root.join("security")),
            TrustRecord {
                machine_id: machine_id.into(),
                ca_cert_pem: other.state.identity().ca_cert_pem.clone(),
                added_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Exactly the unique fixture directory created above; no user config/runtime paths.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn start_pair(
    a: &Fixture,
    b: &Fixture,
    b_id: &str,
) -> (JoinHandle<Result<()>>, JoinHandle<Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server_state = b.state.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await?;
        session::handle_incoming_connection(server_state, socket, None).await
    });
    let stream = connect_fixture(&a.state, b_id, &address).await.unwrap();
    let client_state = a.state.clone();
    let peer_id = b_id.to_owned();
    let client = tokio::spawn(async move {
        session::run_authenticated_outbound_session(client_state, peer_id, stream, None).await
    });
    time::timeout(Duration::from_secs(5), async {
        while !a
            .state
            .get_peer(b_id)
            .await
            .is_some_and(|peer| peer.connected)
        {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("TLS/Hello session established");
    (client, server)
}

async fn connect_fixture(
    state: &AppState,
    peer_id: &str,
    address: &str,
) -> Result<tokio_rustls::TlsStream<TcpStream>> {
    // Use the real socket directly: the separate dial-fault tests install a process-global
    // fake TCP hook. TLS configuration, certificate validation and the session remain production code.
    let socket = TcpStream::connect(address).await?;
    session::configure_low_latency_socket(&socket)?;
    let connector = build_tls_connector(state).await?;
    let stream = connector
        .connect(parse_server_name_for_peer(peer_id, address)?, socket)
        .await?;
    let stream = tokio_rustls::TlsStream::Client(stream);
    anyhow::ensure!(
        session::authenticated_peer_machine_id(state, &stream).await? == peer_id,
        "fixture TLS identity mismatch"
    );
    Ok(stream)
}

fn options(peer_id: &str) -> PairedTestOptions {
    PairedTestOptions {
        peer_id: peer_id.into(),
        samples: 8,
        payload_bytes: MAX_PROBE_BYTES as u32,
        timeout_ms: 5000,
    }
}

#[tokio::test]
async fn paired_testing_real_tls_denies_then_measures_then_revokes_without_user_data_mutation() {
    let a_id = "10000000-0000-0000-0000-000000000001";
    let b_id = "20000000-0000-0000-0000-000000000002";
    let a = Fixture::new(a_id, b_id);
    let b = Fixture::new(b_id, a_id);
    // A probe must neither create the sender's receive folder nor touch an
    // existing receiver file, even when the connection permits file sharing.
    let receiver_inbox = b.root.join("inbox");
    std::fs::create_dir(&receiver_inbox).unwrap();
    let existing_file = receiver_inbox.join("existing-user-file.txt");
    std::fs::write(&existing_file, b"keep this user content").unwrap();
    a.trust(&b, b_id);
    b.trust(&a, a_id);
    let (client, server) = start_pair(&a, &b, b_id).await;

    let denied = a.state.run_paired_test(options(b_id)).await.unwrap();
    assert!(!denied.passed);
    assert_eq!(denied.tests[0].errors, ["peer_rejected:consent_required"]);
    assert!(
        denied.remote.is_none(),
        "no version or binary disclosure before consent"
    );

    b.state
        .set_paired_test_consent(a_id.into(), 60)
        .await
        .unwrap();
    let before = b.state.paired_test_consent();
    let report = a.state.run_paired_test(options(b_id)).await.unwrap();
    assert!(report.passed, "{report:?}");
    assert_eq!(report.evidence_category, Some(EvidenceCategory::Loopback));
    assert_eq!(report.remote.as_ref().unwrap().machine_id, b_id);
    assert_ne!(
        report.local.daemon_instance_id,
        report.remote.as_ref().unwrap().daemon_instance_id
    );
    assert!(
        report
            .local
            .binary_sha256
            .as_ref()
            .is_some_and(|hash| hash.len() == 64)
    );
    assert!(report.local_transport_session_id.unwrap() > 0);
    assert!(report.remote_transport_session_id.unwrap() > 0);
    for summary in &report.tests {
        assert_eq!(summary.completed_samples, 8);
        assert!(summary.errors.is_empty());
        assert_eq!(summary.latency_us.len(), 8);
        assert!(summary.p95_us >= summary.p50_us);
    }
    let after = b.state.paired_test_consent();
    assert_eq!(before.remaining_requests - after.remaining_requests, 16);
    assert_eq!(
        before.remaining_bytes - after.remaining_bytes,
        8 * (64 + MAX_PROBE_BYTES as u64)
    );
    assert!(!a.root.join("inbox").exists());
    assert_eq!(std::fs::read_dir(&receiver_inbox).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(&existing_file).unwrap(),
        b"keep this user content"
    );
    assert_eq!(b.state.pending_inject_input_frame_count().await, 0);

    // Emits actual local measurements for an optional captured benchmark artifact.
    println!(
        "PAIRED_TEST_REPORT={}",
        serde_json::to_string(&report).unwrap()
    );
    b.state
        .set_paired_test_consent(String::new(), 0)
        .await
        .unwrap();
    assert!(!b.state.paired_test_consent().enabled);
    let revoked = a.state.run_paired_test(options(b_id)).await.unwrap();
    assert!(!revoked.passed);
    assert_eq!(revoked.tests[0].completed_samples, 0);

    client.abort();
    server.abort();
    let _ = client.await;
    let _ = server.await;
}

#[tokio::test]
async fn paired_testing_untrusted_peer_cannot_grant_consent_or_authenticate() {
    let a_id = "10000000-0000-0000-0000-000000000001";
    let b_id = "20000000-0000-0000-0000-000000000002";
    let a = Fixture::new(a_id, b_id);
    let b = Fixture::new(b_id, a_id);
    assert!(
        a.state
            .set_paired_test_consent(b_id.into(), 60)
            .await
            .unwrap_err()
            .to_string()
            .contains("not trusted")
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let state = b.state.clone();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        session::handle_incoming_connection(state, socket, None).await
    });
    let result = time::timeout(
        Duration::from_secs(5),
        connect_fixture(&a.state, b_id, &address),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "an untrusted CA must fail actual TLS authentication"
    );
    assert!(!a.state.has_active_transport_session(b_id));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn paired_testing_real_tls_enforces_request_budget_and_lease_expiry() {
    let a_id = "10000000-0000-0000-0000-000000000001";
    let b_id = "20000000-0000-0000-0000-000000000002";
    let a = Fixture::new(a_id, b_id);
    let b = Fixture::new(b_id, a_id);
    a.trust(&b, b_id);
    b.trust(&a, a_id);
    let (client, server) = start_pair(&a, &b, b_id).await;
    b.state
        .set_paired_test_consent(a_id.into(), 60)
        .await
        .unwrap();
    let options = PairedTestOptions {
        samples: 100,
        payload_bytes: 1,
        ..options(b_id)
    };
    let first = a.state.run_paired_test(options.clone()).await.unwrap();
    assert!(first.passed, "{first:?}");
    let exhausted = a.state.run_paired_test(options.clone()).await.unwrap();
    assert!(!exhausted.passed);
    assert_eq!(
        exhausted.tests[0].completed_samples, 56,
        "200 prior requests leave exactly 56 of the 256-request lease"
    );
    assert_eq!(
        exhausted.tests[0].errors,
        ["peer_rejected:lease_budget_exhausted"]
    );
    assert_eq!(b.state.paired_test_consent().remaining_requests, 0);

    b.state
        .set_paired_test_consent(a_id.into(), 1)
        .await
        .unwrap();
    time::sleep(Duration::from_millis(1100)).await;
    let expired = a.state.run_paired_test(options).await.unwrap();
    assert!(!expired.passed);
    assert_eq!(expired.tests[0].completed_samples, 0);
    assert_eq!(expired.tests[0].errors, ["peer_rejected:consent_required"]);
    assert!(!b.state.paired_test_consent().enabled);
    client.abort();
    server.abort();
    let _ = client.await;
    let _ = server.await;
}

#[tokio::test]
async fn paired_testing_replacement_cannot_complete_old_request_and_new_session_recovers() {
    let a_id = "10000000-0000-0000-0000-000000000001";
    let b_id = "20000000-0000-0000-0000-000000000002";
    let a = Fixture::new(a_id, b_id);
    let b = Fixture::new(b_id, a_id);
    a.trust(&b, b_id);
    b.trust(&a, a_id);
    // Begin on the authenticated reverse/nonpreferred path, then replace it with preferred A->B.
    let (old_client, old_server) = start_pair(&b, &a, a_id).await;
    b.state
        .set_paired_test_consent(a_id.into(), 60)
        .await
        .unwrap();
    let initial = a
        .state
        .run_paired_test(PairedTestOptions {
            samples: 1,
            payload_bytes: 1,
            ..options(b_id)
        })
        .await
        .unwrap();
    assert!(initial.passed);
    let old_session = initial.local_transport_session_id.unwrap();
    let (accepted, release) = b.state.pause_next_diagnostic_reply_for_test();
    let state = a.state.clone();
    let run = tokio::spawn(async move {
        state
            .run_paired_test(PairedTestOptions {
                samples: 2,
                payload_bytes: 1,
                timeout_ms: 500,
                ..options(b_id)
            })
            .await
    });
    time::timeout(Duration::from_secs(5), accepted)
        .await
        .unwrap()
        .unwrap();
    let (client, server) = start_pair(&a, &b, b_id).await;
    time::timeout(Duration::from_secs(5), async {
        while a
            .state
            .acquire_transport_session_egress(b_id, old_session)
            .await
            .is_some()
        {
            time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("preferred session replaced old ownership");
    let _ = release.send(());
    let interrupted = time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!interrupted.passed);
    assert_eq!(interrupted.tests[0].completed_samples, 0);
    assert_eq!(interrupted.tests[0].errors, ["response_timeout"]);
    let recovered = a
        .state
        .run_paired_test(PairedTestOptions {
            samples: 2,
            payload_bytes: 1,
            ..options(b_id)
        })
        .await
        .unwrap();
    assert!(recovered.passed, "{recovered:?}");
    assert_ne!(recovered.local_transport_session_id.unwrap(), old_session);
    for handle in [old_client, old_server, client, server] {
        handle.abort();
        let _ = handle.await;
    }
}
