use std::collections::HashMap;

use app_services::desktop::{TcpEndpointCandidate, TcpEndpointSource, tcp_endpoint_candidate};
use peer_transport::outbound_target_candidates as transport_outbound_target_candidates;
#[cfg(test)]
use peer_transport::wait_for_runtime_wake_or_backoff;

use super::session::{connect_outbound_authenticated, run_authenticated_outbound_session};
use super::*;

const OUTBOUND_FAILURE_CONNECTED_GRACE: Duration = Duration::from_secs(10);
const TRANSPORT_CANDIDATE_STAGGER: Duration = Duration::from_millis(75);
const MAX_INCOMING_SESSIONS: usize = 32;
const MAX_ENDPOINT_CANDIDATES: usize = 16;

pub(super) async fn listener_loop(state: AppState, mut listeners: Vec<TcpListener>) {
    if listeners.is_empty() {
        warn!("transport listener task started without listeners");
        return;
    }

    let admission = Arc::new(tokio::sync::Semaphore::new(MAX_INCOMING_SESSIONS));
    let first = listeners.remove(0);
    if listeners.is_empty() {
        accept_loop(state, first, admission).await;
        return;
    }

    let second = listeners.remove(0);
    tokio::select! {
        _ = accept_loop(state.clone(), first, admission.clone()) => {}
        _ = accept_loop(state, second, admission) => {}
    }
}

async fn accept_loop(
    state: AppState,
    listener: TcpListener,
    admission: Arc<tokio::sync::Semaphore>,
) {
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = sessions.join_next(), if !sessions.is_empty() => {}
            accepted = listener.accept() => {
                match accepted {
                    Ok((socket, _remote)) => {
                        let Ok(permit) = admission.clone().try_acquire_owned() else {
                            // Admission rejection must remain cheap and quiet
                            // even when an unauthenticated source floods it.
                            drop(socket);
                            continue;
                        };
                        let task_state = state.clone();
                        let (registration_tx, registration_rx) = oneshot::channel::<crate::state::TransportSessionRegistrationGuard>();
                        let abort = sessions.spawn(async move {
                            let _permit = permit;
                            let Ok(registration) = registration_rx.await else { return; };
                            if let Err(error) = handle_incoming_connection(task_state, socket, Some(registration.session_id)).await {
                                tracing::debug!(error = %transport_error_summary(Some(&error)), "incoming session ended");
                            }
                        });
                        let session_id = state.register_pending_transport_session(abort);
                        let _ = registration_tx.send(state.transport_session_registration_guard(session_id));
                    }
                    Err(error) => {
                        warn!(%error, "transport accept failed");
                        time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        }
    }
}

pub(super) async fn supervisor_loop(state: AppState) {
    let mut workers: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    let mut worker_tasks = tokio::task::JoinSet::new();
    let mut worker_session_ids: HashMap<String, u64> = HashMap::new();
    let reconcile_wake = state.peer_reconcile_wake_signal();
    let mut safety_ticker = time::interval(SUPERVISOR_TICK);
    safety_ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        let finished_peers = workers
            .iter()
            .filter_map(|(peer_id, handle)| {
                if handle.is_finished() {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for peer_id in finished_peers {
            workers.remove(&peer_id);
            if let Some(session_id) = worker_session_ids.remove(&peer_id) {
                state.clear_transport_session_registration(session_id).await;
            }
        }

        while worker_tasks.try_join_next().is_some() {}
        let snapshot = state.snapshot().await;
        for peer in snapshot.peers {
            if peer.peer_id == snapshot.machine_id {
                continue;
            }

            let has_manual_address = !peer.address.trim().is_empty();
            let has_discovered_endpoint = state.discovered_endpoint(&peer.peer_id).await.is_some();
            if !has_manual_address && !has_discovered_endpoint {
                continue;
            }

            if workers.contains_key(&peer.peer_id) {
                continue;
            }

            let worker_state = state.clone();
            let registration_state = state.clone();
            let peer_id = peer.peer_id.clone();
            let (registration_tx, registration_rx) =
                oneshot::channel::<crate::state::TransportSessionRegistrationGuard>();
            let handle = worker_tasks.spawn(async move {
                let Ok(registration) = registration_rx.await else {
                    return;
                };
                peer_worker(worker_state, peer_id, Some(registration.session_id)).await;
            });
            let session_id = registration_state
                .register_transport_session_for_peer(&peer.peer_id, handle.clone());
            let _ = registration_tx
                .send(registration_state.transport_session_registration_guard(session_id));
            workers.insert(peer.peer_id.clone(), handle);
            worker_session_ids.insert(peer.peer_id, session_id);
        }

        let wake_notified = reconcile_wake.notified();
        tokio::pin!(wake_notified);
        if reconcile_wake.take_pending() {
            continue;
        }

        tokio::select! {
            _ = &mut wake_notified => {
                let _ = reconcile_wake.take_pending();
            }
            _ = safety_ticker.tick() => {}
        }
    }
}

async fn peer_worker(state: AppState, peer_id: String, session_registration_id: Option<u64>) {
    peer_worker_with_connector(
        state,
        peer_id,
        session_registration_id,
        |state, peer_id, candidates, registration| async move {
            connect_and_run_outbound_to_candidates(state, &peer_id, &candidates, registration).await
        },
    )
    .await;
}

async fn peer_worker_with_connector<F, Fut>(
    state: AppState,
    peer_id: String,
    session_registration_id: Option<u64>,
    mut connect: F,
) where
    F: FnMut(AppState, String, Vec<TcpEndpointCandidate>, Option<u64>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut backoff_secs = 1;
    let mut next_attempt_at = time::Instant::now();
    let mut last_warning_at: Option<time::Instant> = None;
    let mut failures_since_warning = 0u64;

    loop {
        // This deadline belongs to this peer. Reconcile/activity notifications
        // cannot shorten it. Explicit reconnect already cancels/recreates the
        // registered worker; ordinary peer/discovery changes do not.
        time::sleep_until(next_attempt_at).await;
        let Some(peer) = state.get_peer(&peer_id).await else {
            return;
        };
        if state.has_active_transport_session(&peer_id) {
            next_attempt_at = time::Instant::now() + Duration::from_secs(1);
            continue;
        }

        let discovered_endpoints = state.discovered_endpoint_candidates(&peer_id).await;
        let target_candidates = outbound_transport_candidates(&peer.address, &discovered_endpoints);
        if target_candidates.is_empty() {
            next_attempt_at = time::Instant::now() + Duration::from_secs(backoff_secs);
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            continue;
        }

        let attempt_started = time::Instant::now();
        let result = connect(
            state.clone(),
            peer_id.clone(),
            target_candidates.clone(),
            session_registration_id,
        )
        .await;
        if attempt_started.elapsed() >= Duration::from_secs(10) {
            backoff_secs = 1;
        }
        // A successfully authenticated peer that immediately closes is still
        // a failed availability attempt, not permission for an unbounded loop.
        next_attempt_at = time::Instant::now() + Duration::from_secs(backoff_secs);
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);

        if let Err(error) = result {
            failures_since_warning = failures_since_warning.saturating_add(1);
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "outbound".to_string(),
                kind: "transport_reachability_failed".to_string(),
                peer_id: peer_id.clone(),
                detail: transport_reachability_failure_detail(&target_candidates, Some(&error)),
                size_bytes: 0,
            });
            if last_warning_at.is_none_or(|last| last.elapsed() >= Duration::from_secs(60)) {
                warn!(
                    peer_id = %peer_id,
                    target_candidates = %redacted_tcp_endpoint_labels_for_runtime(&target_candidates),
                    error = %transport_error_summary(Some(&error)),
                    failed_attempts = failures_since_warning,
                    retry_after_ms = next_attempt_at.saturating_duration_since(time::Instant::now()).as_millis(),
                    "outbound connection unavailable; repeated failures are summarized"
                );
                last_warning_at = Some(time::Instant::now());
                failures_since_warning = 0;
            }
            let keep_recent_connected = state
                .get_peer(&peer_id)
                .await
                .is_some_and(|peer| should_preserve_connected_after_outbound_failure(&peer));
            if !keep_recent_connected
                && let Err(error) = state
                    .mark_peer_disconnected_if_no_active_transport_session(&peer_id)
                    .await
            {
                warn!(%error, "failed to update peer runtime state");
            }
        }
    }
}

async fn connect_and_run_outbound_to_candidates(
    state: AppState,
    peer_id: &str,
    target_candidates: &[TcpEndpointCandidate],
    session_registration_id: Option<u64>,
) -> Result<()> {
    let (stream, selected) =
        connect_first_authenticated_outbound_candidate(state.clone(), peer_id, target_candidates)
            .await?;
    state.record_transport_event(crate::state::TransportEventRecord {
        timestamp: chrono::Utc::now(),
        direction: "outbound".to_string(),
        kind: "transport_candidate_selected".to_string(),
        peer_id: peer_id.to_string(),
        detail: format!(
            "transport=direct_initiated selected=[{}]",
            selected.redacted_provenance_label()
        ),
        size_bytes: 0,
    });
    run_authenticated_outbound_session(state, peer_id.to_string(), stream, session_registration_id)
        .await
}

async fn connect_first_authenticated_outbound_candidate(
    state: AppState,
    peer_id: &str,
    target_candidates: &[TcpEndpointCandidate],
) -> Result<(tokio_rustls::TlsStream<TcpStream>, TcpEndpointCandidate)> {
    if target_candidates.is_empty() {
        anyhow::bail!("transport has no endpoint candidates");
    }

    let mut tasks = tokio::task::JoinSet::new();
    for (ordinal, candidate) in target_candidates
        .iter()
        .take(MAX_ENDPOINT_CANDIDATES)
        .cloned()
        .enumerate()
    {
        let peer_id = peer_id.to_string();
        let state = state.clone();
        tasks.spawn(async move {
            if ordinal > 0 {
                time::sleep(stagger_delay(TRANSPORT_CANDIDATE_STAGGER, ordinal)).await;
            }
            let result = connect_outbound_authenticated(state, &peer_id, &candidate.endpoint).await;
            (ordinal, candidate, result)
        });
    }
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (ordinal, candidate, result) = joined.context("outbound candidate task failed")?;
        match result {
            Ok(stream) => {
                tasks.shutdown().await;
                return Ok((stream, candidate));
            }
            Err(error) => failures.push((ordinal, candidate, error)),
        }
    }

    failures.sort_by_key(|(ordinal, _, _)| *ordinal);
    let attempted = failures
        .iter()
        .map(|(_, candidate, error)| {
            format!(
                "{} {}",
                candidate.redacted_provenance_label(),
                transport_error_summary(Some(error))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!("transport candidate racing failed; attempted=[{attempted}]")
}

fn stagger_delay(base: Duration, ordinal: usize) -> Duration {
    base.saturating_mul(ordinal.try_into().unwrap_or(u32::MAX))
}

fn should_preserve_connected_after_outbound_failure(peer: &crate::config::PeerConfig) -> bool {
    if !peer.connected {
        return false;
    }
    let Ok(age) = chrono::Utc::now()
        .signed_duration_since(peer.last_seen)
        .to_std()
    else {
        return false;
    };
    age <= OUTBOUND_FAILURE_CONNECTED_GRACE
}

#[cfg(test)]
pub(super) async fn wait_for_reconcile_or_backoff(
    reconcile_wake: &std::sync::Arc<crate::state::RuntimeWakeSignal>,
    backoff: Duration,
) {
    wait_for_runtime_wake_or_backoff(reconcile_wake, backoff).await;
}

fn outbound_transport_candidates(
    configured_address: &str,
    discovered_endpoints: &[SocketAddr],
) -> Vec<TcpEndpointCandidate> {
    let mut target_endpoints =
        transport_outbound_target_candidates(configured_address, discovered_endpoints);
    if target_endpoints.len() > MAX_ENDPOINT_CANDIDATES {
        target_endpoints.truncate(MAX_ENDPOINT_CANDIDATES);
        let configured = configured_address.trim();
        if !configured.is_empty()
            && !target_endpoints
                .iter()
                .any(|candidate| candidate == configured)
        {
            target_endpoints[MAX_ENDPOINT_CANDIDATES - 1] = configured.to_string();
        }
    }
    target_endpoints
        .iter()
        .take(MAX_ENDPOINT_CANDIDATES)
        .enumerate()
        .map(|(ordinal, endpoint)| {
            let source = if discovered_endpoints
                .iter()
                .any(|discovered| discovered.to_string() == *endpoint)
            {
                TcpEndpointSource::Discovery
            } else {
                TcpEndpointSource::ConfiguredPeer
            };
            tcp_endpoint_candidate(endpoint, source, ordinal)
        })
        .collect()
}

#[cfg(test)]
pub(super) fn outbound_target_candidates(
    configured_address: &str,
    discovered_endpoints: &[SocketAddr],
) -> Vec<String> {
    outbound_transport_candidates(configured_address, discovered_endpoints)
        .into_iter()
        .map(|candidate| candidate.endpoint)
        .collect()
}

fn transport_reachability_failure_detail(
    candidates: &[TcpEndpointCandidate],
    error: Option<&anyhow::Error>,
) -> String {
    format!(
        "mdns_discovered={} tcp_transport_reachability=failed failure_reason={} attempted=[{}] next_action=verify Private network, VLAN routing, and manual admin-approved firewall policy for the listed TCP ports",
        candidates
            .iter()
            .any(|candidate| candidate.source == TcpEndpointSource::Discovery),
        transport_error_summary(error),
        redacted_tcp_endpoint_labels_for_runtime(candidates)
    )
}

fn redacted_tcp_endpoint_labels_for_runtime(candidates: &[TcpEndpointCandidate]) -> String {
    if candidates.is_empty() {
        return "none".to_string();
    }
    candidates
        .iter()
        .map(TcpEndpointCandidate::redacted_provenance_label)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
fn redacted_tcp_socketaddr_labels_for_runtime(candidates: &[SocketAddr]) -> String {
    if candidates.is_empty() {
        return "none".to_string();
    }
    candidates
        .iter()
        .map(|candidate| app_services::desktop::redacted_tcp_endpoint_label(&candidate.to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn transport_error_summary(error: Option<&anyhow::Error>) -> &'static str {
    let Some(error) = error else {
        return "none";
    };
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                )
            })
    }) {
        return "refused";
    }
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("peer identity mismatch") {
        "peer_identity_mismatch"
    } else if message.contains("tls_connect_failed") || message.contains("tls connect") {
        "tls_connect_failed"
    } else if message.contains("refused") {
        "refused"
    } else if message.contains("tcp_connect_failed")
        || message.contains("tcp connect")
        || message.contains("timed out")
    {
        "tcp_connect_failed"
    } else {
        "failed"
    }
}

#[cfg(test)]
pub(super) async fn measure_worker_retry_cadence(
    early_close: bool,
    observation: Duration,
) -> serde_json::Value {
    let root =
        std::env::temp_dir().join(format!("boundless-retry-cadence-{}", uuid::Uuid::new_v4()));
    let state =
        AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
            .unwrap();
    let (code, _) = state.create_pairing_code(120).await;
    let peer = state
        .join_peer(code, "127.0.0.42:15100".into(), None)
        .await
        .unwrap();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_count = attempts.clone();
    let worker_state = state.clone();
    let started = time::Instant::now();
    let worker = tokio::spawn(peer_worker_with_connector(
        worker_state,
        peer,
        None,
        move |_, _, _, _| {
            attempt_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if early_close {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "synthetic unavailable peer",
                    )
                    .into())
                }
            }
        },
    ));
    let until = started + observation;
    while time::Instant::now() < until {
        // Neither fresh unrelated events nor a stored Notify permit may
        // shorten this worker's retry deadline.
        state.notify_peer_reconcile_wake("unrelated_peer_noise");
        time::sleep(Duration::from_millis(1)).await;
    }
    worker.abort();
    let _ = worker.await;
    let elapsed_ms = started.elapsed().as_millis();
    let attempts = attempts.load(std::sync::atomic::Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(root);
    serde_json::json!({"kind": "synthetic_worker", "scenario": if early_close {"immediate_session_close"} else {"connection_refused"}, "attempts": attempts, "elapsed_ms": elapsed_ms, "noisy_reconcile": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn peer_worker_deadlines_survive_noisy_wakes_and_immediate_session_close() {
        for early_close in [false, true] {
            let metric =
                measure_worker_retry_cadence(early_close, Duration::from_millis(250)).await;
            assert_eq!(metric["attempts"], 1, "retry deadline bypassed: {metric}");
        }
    }

    #[test]
    fn transport_reachability_failure_detail_redacts_candidates() {
        let candidates = vec![
            tcp_endpoint_candidate("[fe80::1%4]:15100", TcpEndpointSource::Discovery, 0),
            tcp_endpoint_candidate("10.0.0.9:15100", TcpEndpointSource::Discovery, 1),
        ];
        let detail = transport_reachability_failure_detail(&candidates, None);

        assert!(detail.contains("mdns_discovered=true"));
        assert!(detail.contains("tcp_transport_reachability=failed"));
        assert!(detail.contains("failure_reason=none"));
        assert!(detail.contains("source=mdns tcp ipv6 port 15100"));
        assert!(detail.contains("source=mdns tcp ipv4 port 15100"));
        assert!(detail.contains("next_action=verify Private network"));
        assert!(!detail.contains("10.0.0.9"));
        assert!(!detail.contains("fe80::1"));
    }

    #[test]
    fn transport_runtime_log_labels_redact_configured_and_discovered_endpoints() {
        let configured = app_services::desktop::redacted_tcp_endpoint_label("10.0.0.10:15100");
        let discovered = redacted_tcp_socketaddr_labels_for_runtime(&[
            "10.0.0.9:15100".parse().expect("ipv4 endpoint"),
            "[2001:db8::7]:15100".parse().expect("ipv6 endpoint"),
        ]);
        let targets = redacted_tcp_endpoint_labels_for_runtime(&[
            tcp_endpoint_candidate("10.0.0.10:15100", TcpEndpointSource::ConfiguredPeer, 0),
            tcp_endpoint_candidate("[2001:db8::7]:15100", TcpEndpointSource::Discovery, 1),
        ]);

        assert_eq!(configured, "tcp ipv4 port 15100");
        assert!(discovered.contains("tcp ipv4 port 15100"));
        assert!(discovered.contains("tcp ipv6 port 15100"));
        assert!(targets.contains("source=configured-peer tcp ipv4 port 15100"));
        assert!(targets.contains("source=mdns tcp ipv6 port 15100"));
        assert!(!discovered.contains("10.0.0.9"));
        assert!(!discovered.contains("2001:db8::7"));
        assert!(!targets.contains("10.0.0.10"));
        assert!(!targets.contains("2001:db8::7"));
    }

    #[test]
    fn outbound_transport_candidates_preserve_order_and_provenance() {
        let discovered = vec![
            "[2001:db8::7]:15100".parse().expect("ipv6 endpoint"),
            "10.0.0.9:15100".parse().expect("ipv4 endpoint"),
        ];

        let candidates = outbound_transport_candidates("peer.example.test:15100", &discovered);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].endpoint, "[2001:db8::7]:15100");
        assert_eq!(candidates[0].source, TcpEndpointSource::Discovery);
        assert_eq!(
            candidates[0].family,
            app_services::desktop::TcpEndpointFamily::Ipv6
        );
        assert_eq!(candidates[0].port, Some(15100));
        assert_eq!(candidates[0].ordinal, 0);
        assert_eq!(candidates[1].endpoint, "10.0.0.9:15100");
        assert_eq!(candidates[1].source, TcpEndpointSource::Discovery);
        assert_eq!(
            candidates[1].family,
            app_services::desktop::TcpEndpointFamily::Ipv4
        );
        assert_eq!(candidates[1].port, Some(15100));
        assert_eq!(candidates[1].ordinal, 1);
        assert_eq!(candidates[2].endpoint, "peer.example.test:15100");
        assert_eq!(candidates[2].source, TcpEndpointSource::ConfiguredPeer);
        assert_eq!(
            candidates[2].family,
            app_services::desktop::TcpEndpointFamily::Hostname
        );
        assert_eq!(candidates[2].port, Some(15100));
        assert_eq!(candidates[2].ordinal, 2);
    }

    #[test]
    fn endpoint_candidate_budget_preserves_configured_fallback() {
        let discovered = (1..=40)
            .map(|host| format!("10.0.0.{host}:15100").parse().unwrap())
            .collect::<Vec<SocketAddr>>();
        let candidates = outbound_transport_candidates("manual.example:15100", &discovered);
        assert_eq!(candidates.len(), MAX_ENDPOINT_CANDIDATES);
        assert_eq!(candidates.last().unwrap().endpoint, "manual.example:15100");
        assert_eq!(candidates[0].endpoint, "10.0.0.1:15100");
    }

    #[test]
    fn outbound_failure_preserves_only_recent_connected_session_state() {
        let recent_connected = crate::config::PeerConfig {
            peer_id: "peer-a".to_string(),
            display_name: "Peer A".to_string(),
            address: "peer-a.example:15100".to_string(),
            connected: true,
            last_seen: chrono::Utc::now(),
        };
        assert!(should_preserve_connected_after_outbound_failure(
            &recent_connected
        ));

        let stale_connected = crate::config::PeerConfig {
            last_seen: chrono::Utc::now() - chrono::Duration::seconds(60),
            ..recent_connected.clone()
        };
        assert!(!should_preserve_connected_after_outbound_failure(
            &stale_connected
        ));

        let recent_disconnected = crate::config::PeerConfig {
            connected: false,
            last_seen: chrono::Utc::now(),
            ..recent_connected
        };
        assert!(!should_preserve_connected_after_outbound_failure(
            &recent_disconnected
        ));
    }

    #[tokio::test]
    async fn stale_outbound_failure_preserves_active_reverse_session_state() {
        let root = std::env::temp_dir().join(format!(
            "boundless-stale-outbound-failure-test-{}",
            uuid::Uuid::new_v4()
        ));
        let state =
            AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
                .expect("load state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("reverse-owner".to_string()),
            )
            .await
            .expect("join peer");
        let session_id = state.allocate_transport_session_id();
        assert_eq!(
            state
                .claim_transport_session(
                    &peer_id,
                    session_id,
                    false,
                    std::sync::Arc::new(crate::state::RuntimeWakeSignal::default()),
                )
                .await,
            crate::state::TransportSessionClaim::Claimed
        );
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("mark reverse owner connected");
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim input owner")
        );
        state
            .set_input_capture_target(Some(&peer_id))
            .await
            .expect("set capture target");

        assert!(
            !state
                .mark_peer_disconnected_if_no_active_transport_session(&peer_id)
                .await
                .expect("process stale outbound failure"),
            "an outbound failure that started before the reverse claim must not disconnect it"
        );
        assert!(
            state
                .snapshot()
                .await
                .peers
                .iter()
                .any(|peer| peer.peer_id == peer_id && peer.connected)
        );
        assert_eq!(state.input_owner().await.as_deref(), Some(peer_id.as_str()));
        assert_eq!(
            state.input_capture_target().await.as_deref(),
            Some(peer_id.as_str())
        );

        assert!(
            state
                .close_active_transport_session(&peer_id, session_id)
                .await
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transport_error_summary_redacts_endpoint_context() {
        let error = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "blocked",
        ))
        .context("tcp connect 10.0.0.9:15100")
        .expect_err("build contextual error");

        assert_eq!(transport_error_summary(Some(&error)), "refused");
        assert!(!transport_error_summary(Some(&error)).contains("10.0.0.9"));
    }

    #[test]
    fn transport_error_summary_preserves_redacted_racing_category() {
        let error = anyhow::anyhow!(
            "transport candidate racing failed; attempted=[source=mdns tcp ipv4 port 15100 tls_connect_failed]"
        );

        assert_eq!(transport_error_summary(Some(&error)), "tls_connect_failed");
    }

    #[tokio::test]
    async fn peer_worker_honors_backoff_after_immediate_connection_refusal() {
        const MAX_FAILED_ATTEMPTS: usize = 10_000;
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_attempts = attempts.clone();
        let _hook = super::session::install_test_tcp_connect_hook(
            Duration::from_secs(30),
            move |_address| {
                let attempt = hook_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    // Independently cap the repro even if the runtime's timer is starved.
                    if attempt >= MAX_FAILED_ATTEMPTS {
                        std::future::pending::<std::io::Result<tokio::net::TcpStream>>().await
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            "bounded audit: paired peer is unavailable",
                        ))
                    }
                })
            },
        );
        let root = std::env::temp_dir().join(format!(
            "boundless-peer-worker-refusal-rate-audit-{}",
            uuid::Uuid::new_v4()
        ));
        let state =
            AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
                .expect("load isolated audit state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.42:15100".to_string(),
                Some("unreachable-audit-peer".to_string()),
            )
            .await
            .expect("join isolated audit peer");

        // Pairing sets both pending state and a Notify permit. Drain both so the
        // observation only measures the worker's own failed-connect behavior.
        let wake = state.peer_reconcile_wake_signal();
        assert!(
            wake.take_pending(),
            "pairing should have queued reconciliation"
        );
        time::timeout(Duration::from_millis(100), wake.notified())
            .await
            .expect("drain pairing notification permit");
        assert!(!wake.take_pending(), "reconcile signal should start clear");

        let started = std::time::Instant::now();
        let worker = tokio::spawn(peer_worker(state, peer_id, None));
        time::sleep(Duration::from_millis(250)).await;
        worker.abort();
        let _ = worker.await;
        let elapsed = started.elapsed();
        let attempt_count = attempts.load(std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(root);

        println!(
            "bounded_refusal_audit attempts={attempt_count} elapsed_ms={} observation_ms=250 failed_attempt_guard={MAX_FAILED_ATTEMPTS}",
            elapsed.as_millis()
        );
        assert_eq!(
            attempt_count,
            1,
            "a refused connection must wait at least the initial 1-second backoff before retrying; observed {attempt_count} attempts in {}ms",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn peer_worker_bounds_stalled_candidate_and_records_reachability_failure() {
        let stalled_endpoint = "127.0.0.41:15100";
        let fallback_endpoint = "127.0.0.42:15100";
        let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let hook_attempts = attempts.clone();
        let _hook = super::session::install_test_tcp_connect_hook(
            Duration::from_millis(25),
            move |address| {
                hook_attempts
                    .lock()
                    .expect("attempts mutex poisoned")
                    .push(address.clone());
                Box::pin(async move {
                    if address == stalled_endpoint {
                        std::future::pending::<std::io::Result<tokio::net::TcpStream>>().await
                    } else if address == fallback_endpoint {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            "fallback candidate refused",
                        ))
                    } else {
                        tokio::net::TcpStream::connect(address).await
                    }
                })
            },
        );
        let root = std::env::temp_dir().join(format!(
            "boundless-peer-worker-timeout-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                fallback_endpoint.to_string(),
                Some("remote".to_string()),
            )
            .await
            .expect("join peer");
        state
            .set_discovered_endpoint(
                &peer_id,
                "remote",
                stalled_endpoint.parse().expect("stalled endpoint"),
            )
            .await;

        let worker = tokio::spawn(peer_worker(state.clone(), peer_id.clone(), None));
        let event = time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(event) = state.transport_events().await.into_iter().find(|event| {
                    event.kind == "transport_reachability_failed" && event.peer_id == peer_id
                }) {
                    return event;
                }
                time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bounded candidate failure should emit reachability event");
        worker.abort();
        let _ = worker.await;

        let attempts = attempts.lock().expect("attempts mutex poisoned");
        assert!(
            attempts.starts_with(&[stalled_endpoint.to_string(), fallback_endpoint.to_string()]),
            "candidate loop should continue past the stalled first candidate before retrying; attempts={attempts:?}"
        );
        assert!(event.detail.contains("tcp_transport_reachability=failed"));
        assert!(event.detail.contains("failure_reason=refused"));
        assert!(event.detail.contains("source=mdns tcp ipv4 port 15100"));
        assert!(
            event
                .detail
                .contains("source=configured-peer tcp ipv4 port 15100")
        );

        let _ = std::fs::remove_dir_all(root);
    }
    #[tokio::test]
    async fn inbound_admission_is_bounded_and_listener_cancellation_releases_registrations() {
        let root = std::env::temp_dir().join(format!(
            "boundless-inbound-admission-{}",
            uuid::Uuid::new_v4()
        ));
        let state =
            AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serving_state = state.clone();
        let server = tokio::spawn(listener_loop(serving_state, vec![listener]));
        let mut clients = Vec::new();
        for _ in 0..MAX_INCOMING_SESSIONS + 8 {
            clients.push(TcpStream::connect(addr).await.unwrap());
        }
        time::timeout(Duration::from_secs(2), async {
            while state.transport_session_registration_count() < MAX_INCOMING_SESSIONS {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("admission reaches cap");
        assert_eq!(
            state.transport_session_registration_count(),
            MAX_INCOMING_SESSIONS
        );
        server.abort();
        let _ = server.await;
        time::timeout(Duration::from_secs(1), async {
            while state.transport_session_registration_count() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation clears every pending registration");
        drop(clients);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn invalid_and_silent_tls_connections_release_registration() {
        let root = std::env::temp_dir().join(format!(
            "boundless-inbound-deadline-{}",
            uuid::Uuid::new_v4()
        ));
        let state =
            AppState::load_or_create_with_paths(root.join("config.json"), root.join("security"))
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(listener_loop(state.clone(), vec![listener]));
        let mut invalid = TcpStream::connect(addr).await.unwrap();
        invalid.write_all(b"not a TLS handshake").await.unwrap();
        invalid.shutdown().await.unwrap();
        drop(invalid);
        time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state.transport_session_registration_count(),
            0,
            "TLS rejection must not retain completed abort handles"
        );
        let silent = TcpStream::connect(addr).await.unwrap();
        time::timeout(Duration::from_secs(1), async {
            while state.transport_session_registration_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        time::timeout(Duration::from_secs(6), async {
            while state.transport_session_registration_count() > 0 {
                time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("silent TLS connection must expire");
        server.abort();
        let _ = server.await;
        drop(silent);
        let _ = std::fs::remove_dir_all(root);
    }
}
