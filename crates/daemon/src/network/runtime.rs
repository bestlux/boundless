use std::collections::HashMap;

use app_services::desktop::{TcpEndpointCandidate, TcpEndpointSource, tcp_endpoint_candidate};
use peer_transport::{
    outbound_target_candidates as transport_outbound_target_candidates,
    wait_for_runtime_wake_or_backoff,
};

use super::session::{connect_outbound_authenticated, run_authenticated_outbound_session};
use super::*;

const OUTBOUND_FAILURE_CONNECTED_GRACE: Duration = Duration::from_secs(10);
const TRANSPORT_CANDIDATE_STAGGER: Duration = Duration::from_millis(75);

pub(super) async fn listener_loop(state: AppState, mut listeners: Vec<TcpListener>) {
    if listeners.is_empty() {
        warn!("transport listener task started without listeners");
        return;
    }

    let first = listeners.remove(0);
    if listeners.is_empty() {
        accept_loop(state, first).await;
        return;
    }

    let second = listeners.remove(0);
    tokio::select! {
        _ = accept_loop(state.clone(), first) => {}
        _ = accept_loop(state, second) => {}
    }
}

async fn accept_loop(state: AppState, listener: TcpListener) {
    let bind = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    info!(bind = %bind, "transport listener started");

    loop {
        match listener.accept().await {
            Ok((socket, remote)) => {
                let task_state = state.clone();
                let registration_state = state.clone();
                let (session_id_tx, session_id_rx) = oneshot::channel::<u64>();
                let task = tokio::spawn(async move {
                    let session_id = session_id_rx.await.ok();
                    if let Err(error) =
                        handle_incoming_connection(task_state, socket, session_id).await
                    {
                        warn!(error = ?error, remote = %remote, "incoming session ended with error");
                    }
                });

                let session_id =
                    registration_state.register_pending_transport_session(task.abort_handle());
                let _ = session_id_tx.send(session_id);
            }
            Err(error) => {
                warn!(%error, "transport accept failed");
                time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
}

pub(super) async fn supervisor_loop(state: AppState) {
    let mut workers: HashMap<String, JoinHandle<()>> = HashMap::new();
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
            let (session_id_tx, session_id_rx) = oneshot::channel::<u64>();
            let handle = tokio::spawn(async move {
                let session_id = session_id_rx.await.ok();
                peer_worker(worker_state, peer_id, session_id).await;
            });
            let session_id = registration_state
                .register_transport_session_for_peer(&peer.peer_id, handle.abort_handle());
            let _ = session_id_tx.send(session_id);
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
    let mut backoff_secs: u64 = 1;
    let reconcile_wake = state.peer_reconcile_wake_signal();

    loop {
        let Some(peer) = state.get_peer(&peer_id).await else {
            info!(peer_id = %peer_id, "peer worker exiting; peer removed");
            return;
        };

        if state.has_active_transport_session(&peer_id) {
            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(1)).await;
            backoff_secs = 1;
            continue;
        }

        let discovered_endpoints = state.discovered_endpoint_candidates(&peer_id).await;
        let target_candidates = outbound_transport_candidates(&peer.address, &discovered_endpoints);
        if target_candidates.is_empty() {
            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            continue;
        }

        if let Err(error) = connect_and_run_outbound_to_candidates(
            state.clone(),
            &peer_id,
            &target_candidates,
            session_registration_id,
        )
        .await
        {
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "outbound".to_string(),
                kind: "transport_reachability_failed".to_string(),
                peer_id: peer_id.clone(),
                detail: transport_reachability_failure_detail(&target_candidates, Some(&error)),
                size_bytes: 0,
            });
            warn!(
                peer_id = %peer_id,
                configured_address = %app_services::desktop::redacted_tcp_endpoint_label(&peer.address),
                target_candidates = %redacted_tcp_endpoint_labels_for_runtime(&target_candidates),
                discovered_endpoint_count = discovered_endpoints.len(),
                discovered_endpoints = %redacted_tcp_socketaddr_labels_for_runtime(&discovered_endpoints),
                error = %transport_error_summary(Some(&error)),
                "outbound connect failed"
            );
            let keep_recent_connected = state
                .get_peer(&peer_id)
                .await
                .is_some_and(|peer| should_preserve_connected_after_outbound_failure(&peer));
            if keep_recent_connected {
                info!(
                    peer_id = %peer_id,
                    "outbound connect failed but recent connected session state is preserved"
                );
            } else if let Err(mark_error) = state.set_peer_connected(&peer_id, false).await {
                warn!(%mark_error, "failed to mark peer disconnected");
            }

            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
        } else {
            backoff_secs = 1;
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

    let (tx, mut rx) = tokio::sync::mpsc::channel(target_candidates.len());
    let mut handles = Vec::with_capacity(target_candidates.len());
    for (ordinal, candidate) in target_candidates.iter().cloned().enumerate() {
        let tx = tx.clone();
        let peer_id = peer_id.to_string();
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            if ordinal > 0 {
                time::sleep(stagger_delay(TRANSPORT_CANDIDATE_STAGGER, ordinal)).await;
            }
            let result = connect_outbound_authenticated(state, &peer_id, &candidate.endpoint).await;
            let _ = tx.send((ordinal, candidate, result)).await;
        }));
    }
    drop(tx);

    let mut failures = Vec::new();
    while let Some((ordinal, candidate, result)) = rx.recv().await {
        match result {
            Ok(stream) => {
                for handle in handles {
                    handle.abort();
                }
                return Ok((stream, candidate));
            }
            Err(error) => failures.push((ordinal, candidate, error)),
        }
    }

    for handle in handles {
        if !handle.is_finished() {
            handle.abort();
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
    let target_endpoints =
        transport_outbound_target_candidates(configured_address, discovered_endpoints);
    target_endpoints
        .iter()
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
mod tests {
    use super::*;

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
}
