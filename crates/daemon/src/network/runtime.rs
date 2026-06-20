use std::collections::HashMap;

use peer_transport::{
    outbound_target_candidates as transport_outbound_target_candidates,
    wait_for_runtime_wake_or_backoff,
};

use super::*;

pub(super) async fn listener_loop(state: AppState, listener: TcpListener) {
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

        let discovered_endpoint = state.discovered_endpoint(&peer_id).await;
        let target_candidates = outbound_target_candidates(&peer.address, discovered_endpoint);
        if target_candidates.is_empty() {
            wait_for_reconcile_or_backoff(&reconcile_wake, Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECONDS);
            continue;
        }

        let mut connected = false;
        let mut last_error: Option<anyhow::Error> = None;
        for target_address in &target_candidates {
            match connect_and_run_outbound(state.clone(), &peer_id, target_address).await {
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
            warn!(
                peer_id = %peer_id,
                configured_address = %peer.address,
                target_candidates = ?target_candidates,
                discovered_endpoint = ?discovered_endpoint,
                error = ?last_error,
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

pub(super) fn outbound_target_candidates(
    configured_address: &str,
    discovered_endpoint: Option<SocketAddr>,
) -> Vec<String> {
    transport_outbound_target_candidates(configured_address, discovered_endpoint)
}
