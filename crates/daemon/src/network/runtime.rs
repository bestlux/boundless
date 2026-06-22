use std::collections::HashMap;

use app_services::desktop::{TcpEndpointCandidate, TcpEndpointSource, tcp_endpoint_candidate};
use peer_transport::{
    outbound_target_candidates as transport_outbound_target_candidates,
    wait_for_runtime_wake_or_backoff,
};

use super::*;

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
            let handle = tokio::spawn(async move {
                peer_worker(worker_state, peer_id).await;
            });
            let session_id = registration_state
                .register_transport_session_for_peer(&peer.peer_id, handle.abort_handle());
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

async fn peer_worker(state: AppState, peer_id: String) {
    let mut backoff_secs: u64 = 1;
    let reconcile_wake = state.peer_reconcile_wake_signal();

    loop {
        let Some(peer) = state.get_peer(&peer_id).await else {
            info!(peer_id = %peer_id, "peer worker exiting; peer removed");
            return;
        };

        let discovered_endpoints = state.discovered_endpoint_candidates(&peer_id).await;
        let target_candidates = outbound_transport_candidates(&peer.address, &discovered_endpoints);
        if target_candidates.is_empty() {
            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            continue;
        }

        let mut connected = false;
        let mut last_error: Option<anyhow::Error> = None;
        for target in &target_candidates {
            match connect_and_run_outbound(state.clone(), &peer_id, &target.endpoint).await {
                Ok(()) => {
                    backoff_secs = 1;
                    connected = true;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        if !connected {
            state.record_transport_event(crate::state::TransportEventRecord {
                timestamp: chrono::Utc::now(),
                direction: "outbound".to_string(),
                kind: "transport_reachability_failed".to_string(),
                peer_id: peer_id.clone(),
                detail: transport_reachability_failure_detail(&target_candidates),
                size_bytes: 0,
            });
            warn!(
                peer_id = %peer_id,
                configured_address = %app_services::desktop::redacted_tcp_endpoint_label(&peer.address),
                target_candidates = %redacted_tcp_endpoint_labels_for_runtime(&target_candidates),
                discovered_endpoint_count = discovered_endpoints.len(),
                discovered_endpoints = %redacted_tcp_socketaddr_labels_for_runtime(&discovered_endpoints),
                error = %transport_error_summary(last_error.as_ref()),
                "outbound connect failed"
            );
            if let Err(mark_error) = state.set_peer_connected(&peer_id, false).await {
                warn!(%mark_error, "failed to mark peer disconnected");
            }

            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
        }
    }
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

fn transport_reachability_failure_detail(candidates: &[TcpEndpointCandidate]) -> String {
    format!(
        "mdns_discovered={} tcp_transport_reachability=failed attempted=[{}] next_action=verify Private network, VLAN routing, and manual admin-approved firewall policy for the listed TCP ports",
        candidates
            .iter()
            .any(|candidate| candidate.source == TcpEndpointSource::Discovery),
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
    if message.contains("tcp connect") {
        "tcp_connect_failed"
    } else if message.contains("tls connect") {
        "tls_connect_failed"
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
        let detail = transport_reachability_failure_detail(&candidates);

        assert!(detail.contains("mdns_discovered=true"));
        assert!(detail.contains("tcp_transport_reachability=failed"));
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
}
