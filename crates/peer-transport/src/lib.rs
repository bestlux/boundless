use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use core_input::InputEvent;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Notify, RwLock, watch},
    task::AbortHandle,
    time,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportLane {
    Control,
    Realtime,
    Bulk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportTuning {
    pub heartbeat_interval: Duration,
    pub outgoing_input_flush_interval: Duration,
    pub outgoing_bulk_flush_interval: Duration,
    pub outgoing_bulk_max_payloads_per_flush: usize,
}

impl TransportTuning {
    pub const fn new(
        heartbeat_interval: Duration,
        outgoing_input_flush_interval: Duration,
        outgoing_bulk_flush_interval: Duration,
        outgoing_bulk_max_payloads_per_flush: usize,
    ) -> Self {
        Self {
            heartbeat_interval,
            outgoing_input_flush_interval,
            outgoing_bulk_flush_interval,
            outgoing_bulk_max_payloads_per_flush,
        }
    }
}

impl Default for TransportTuning {
    fn default() -> Self {
        DEFAULT_TRANSPORT_TUNING
    }
}

pub const DEFAULT_TRANSPORT_TUNING: TransportTuning = TransportTuning::new(
    Duration::from_secs(2),
    Duration::from_millis(4),
    Duration::from_millis(16),
    4,
);

#[derive(Debug, Default)]
pub struct OutboundTransferFlow {
    pub available_chunk_credits: u32,
}

pub type OutboundTransferFlows = HashMap<String, OutboundTransferFlow>;

pub const FILE_TRANSFER_INITIAL_CHUNK_CREDITS: u32 = 8;
pub const FILE_TRANSFER_MAX_TRACKED_CHUNK_CREDITS: u32 = 256;
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_INBOUND_TRANSFERS_PER_PEER: usize = 4;
pub const CLIPBOARD_IMAGE_CHUNK_BYTES: usize = 128 * 1024;
pub const MAX_TRANSPORT_EVENTS: usize = 512;

#[derive(Debug, Clone)]
pub enum OutboundPayload {
    ClipboardText {
        text: String,
    },
    ClipboardImage {
        image_bmp: Vec<u8>,
    },
    FileStart {
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        source_path: PathBuf,
        offset_bytes: u64,
        length_bytes: usize,
    },
    FileEnd {
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    InputFrame {
        sequence: u64,
        timestamp_unix_ms: i64,
        events: Vec<InputEvent>,
    },
}

#[derive(Debug, Default)]
pub struct OutgoingPeerQueues {
    pub input: VecDeque<OutboundPayload>,
    pub bulk: VecDeque<OutboundPayload>,
}

impl OutgoingPeerQueues {
    pub fn push(&mut self, payload: OutboundPayload) {
        if matches!(payload, OutboundPayload::InputFrame { .. }) {
            self.input.push_back(payload);
        } else {
            self.bulk.push_back(payload);
        }
    }
}

pub fn register_outbound_transfer_flow(
    outbound_transfer_flow: &mut OutboundTransferFlows,
    transfer_id: String,
) {
    outbound_transfer_flow.insert(
        transfer_id,
        OutboundTransferFlow {
            available_chunk_credits: 0,
        },
    );
}

pub fn remove_outbound_transfer_flow(
    outbound_transfer_flow: &mut OutboundTransferFlows,
    transfer_id: &str,
) {
    outbound_transfer_flow.remove(transfer_id);
}

pub fn has_available_outbound_chunk_credit(
    outbound_transfer_flow: &OutboundTransferFlows,
    transfer_id: &str,
) -> Option<bool> {
    outbound_transfer_flow
        .get(transfer_id)
        .map(|flow| flow.available_chunk_credits > 0)
}

pub fn consume_outbound_chunk_credit(
    outbound_transfer_flow: &mut OutboundTransferFlows,
    transfer_id: &str,
) {
    if let Some(flow) = outbound_transfer_flow.get_mut(transfer_id) {
        flow.available_chunk_credits = flow.available_chunk_credits.saturating_sub(1);
    }
}

pub fn apply_outbound_chunk_credits(
    outbound_transfer_flow: &mut OutboundTransferFlows,
    transfer_id: &str,
    chunk_credits: u32,
) -> Option<u32> {
    let flow = outbound_transfer_flow.get_mut(transfer_id)?;
    flow.available_chunk_credits = flow
        .available_chunk_credits
        .saturating_add(chunk_credits)
        .min(FILE_TRANSFER_MAX_TRACKED_CHUNK_CREDITS);
    Some(flow.available_chunk_credits)
}

pub fn restore_outbound_chunk_credits_for_payloads(
    outbound_transfer_flow: &mut OutboundTransferFlows,
    payloads: &[OutboundPayload],
) {
    for payload in payloads {
        let OutboundPayload::FileChunk { transfer_id, .. } = payload else {
            continue;
        };
        let Some(flow) = outbound_transfer_flow.get_mut(transfer_id) else {
            continue;
        };
        flow.available_chunk_credits = flow
            .available_chunk_credits
            .saturating_add(1)
            .min(FILE_TRANSFER_MAX_TRACKED_CHUNK_CREDITS);
    }
}

pub fn outbound_target_candidates(
    configured_address: &str,
    discovered_endpoint: Option<SocketAddr>,
) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(endpoint) = discovered_endpoint {
        targets.push(endpoint.to_string());
    }

    let manual = configured_address.trim();
    if !manual.is_empty() && !targets.iter().any(|target| target == manual) {
        targets.push(manual.to_string());
    }
    targets
}

pub fn reconnect_generation_advanced(
    observed_generation: &mut u64,
    current_generation: u64,
) -> bool {
    if current_generation <= *observed_generation {
        return false;
    }
    *observed_generation = current_generation;
    true
}

pub async fn wait_for_runtime_wake_or_backoff(
    wake_signal: &Arc<RuntimeWakeSignal>,
    backoff: Duration,
) {
    let wake_notified = wake_signal.notified();
    tokio::pin!(wake_notified);
    if wake_signal.take_pending() {
        return;
    }

    tokio::select! {
        _ = &mut wake_notified => {
            let _ = wake_signal.take_pending();
        }
        _ = time::sleep(backoff) => {}
    }
}

#[derive(Debug)]
pub struct InboundTransfer {
    pub peer_id: String,
    pub file_name: String,
    pub total_bytes: u64,
    pub bytes_received: u64,
    pub temp_path: PathBuf,
    pub temp_file: tokio::fs::File,
}

#[derive(Debug)]
pub struct InboundClipboardImageTransfer {
    pub peer_id: String,
    pub total_bytes: u64,
    pub bytes_received: u64,
    pub hash_hex: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportEventRecord {
    pub timestamp: DateTime<Utc>,
    pub direction: String,
    pub kind: String,
    pub peer_id: String,
    pub detail: String,
    pub size_bytes: u64,
}

#[derive(Debug, Default)]
pub struct RuntimeWakeSignal {
    notify: Notify,
    pending: AtomicBool,
}

impl RuntimeWakeSignal {
    pub fn trigger(&self) -> bool {
        !self.pending.swap(true, Ordering::AcqRel)
    }

    pub fn clear(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    pub fn take_pending(&self) -> bool {
        self.clear()
    }

    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    pub fn notify_one(&self) {
        self.notify.notify_one();
    }
}

#[derive(Debug)]
pub struct TransportRuntimeState {
    pub reconnect_generation_by_peer: RwLock<HashMap<String, u64>>,
    pub outgoing_input_payloads: RwLock<HashMap<String, VecDeque<OutboundPayload>>>,
    pub outgoing_bulk_payloads: RwLock<HashMap<String, VecDeque<OutboundPayload>>>,
    pub transport_events: Mutex<VecDeque<TransportEventRecord>>,
    pub outgoing_input_high_water_by_peer: Mutex<HashMap<String, usize>>,
    pub outgoing_flush_signal: watch::Sender<u64>,
    pub outgoing_flush_generation: AtomicU64,
    pub peer_reconcile_wake: Arc<RuntimeWakeSignal>,
    pub pending_transport_session_abort_handles: RwLock<HashMap<u64, AbortHandle>>,
    pub transport_session_abort_handles_by_peer: RwLock<HashMap<String, HashMap<u64, AbortHandle>>>,
    pub next_transport_session_id: AtomicU64,
}

impl Default for TransportRuntimeState {
    fn default() -> Self {
        let (outgoing_flush_signal, _outgoing_flush_rx) = watch::channel(0u64);
        Self {
            reconnect_generation_by_peer: RwLock::new(HashMap::new()),
            outgoing_input_payloads: RwLock::new(HashMap::new()),
            outgoing_bulk_payloads: RwLock::new(HashMap::new()),
            transport_events: Mutex::new(VecDeque::with_capacity(MAX_TRANSPORT_EVENTS)),
            outgoing_input_high_water_by_peer: Mutex::new(HashMap::new()),
            outgoing_flush_signal,
            outgoing_flush_generation: AtomicU64::new(0),
            peer_reconcile_wake: Arc::new(RuntimeWakeSignal::default()),
            pending_transport_session_abort_handles: RwLock::new(HashMap::new()),
            transport_session_abort_handles_by_peer: RwLock::new(HashMap::new()),
            next_transport_session_id: AtomicU64::new(1),
        }
    }
}

impl TransportRuntimeState {
    pub async fn clear(&self) {
        self.reconnect_generation_by_peer.write().await.clear();
        self.outgoing_input_payloads.write().await.clear();
        self.outgoing_bulk_payloads.write().await.clear();
        if let Ok(mut events) = self.transport_events.lock() {
            events.clear();
        }
        if let Ok(mut high_water) = self.outgoing_input_high_water_by_peer.lock() {
            high_water.clear();
        }
        self.pending_transport_session_abort_handles
            .write()
            .await
            .clear();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .clear();
        self.next_transport_session_id.store(1, Ordering::Release);
    }

    pub fn allocate_transport_session_id(&self) -> u64 {
        self.next_transport_session_id
            .fetch_add(1, Ordering::Relaxed)
    }

    pub async fn request_peer_reconnect(&self, peer_id: &str) -> u64 {
        let mut generations = self.reconnect_generation_by_peer.write().await;
        let entry = generations.entry(peer_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    }

    pub async fn peer_reconnect_generation(&self, peer_id: &str) -> u64 {
        *self
            .reconnect_generation_by_peer
            .read()
            .await
            .get(peer_id)
            .unwrap_or(&0)
    }

    pub fn subscribe_outgoing_flush_signal(&self) -> watch::Receiver<u64> {
        self.outgoing_flush_signal.subscribe()
    }

    pub fn notify_outgoing_flush_signal(&self) {
        let next = self
            .outgoing_flush_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let _ = self.outgoing_flush_signal.send(next);
    }

    pub fn observe_outgoing_input_high_water(&self, peer_id: &str, depth: usize) -> Option<usize> {
        let Ok(mut high_water) = self.outgoing_input_high_water_by_peer.lock() else {
            return None;
        };
        let entry = high_water.entry(peer_id.to_string()).or_insert(0);
        if depth > *entry {
            *entry = depth;
            return Some(depth);
        }
        None
    }

    pub fn record_transport_event(&self, event: TransportEventRecord) {
        let Ok(mut events) = self.transport_events.lock() else {
            return;
        };

        events.push_back(event);
        while events.len() > MAX_TRANSPORT_EVENTS {
            events.pop_front();
        }
    }

    pub fn transport_event_count(&self) -> usize {
        self.transport_events
            .lock()
            .map(|events| events.len())
            .unwrap_or(0)
    }

    pub async fn transport_events_snapshot(&self) -> Vec<TransportEventRecord> {
        let Ok(events) = self.transport_events.lock() else {
            return Vec::new();
        };
        events.iter().cloned().collect()
    }

    pub async fn register_pending_transport_session(&self, abort_handle: AbortHandle) -> u64 {
        let session_id = self.allocate_transport_session_id();
        self.pending_transport_session_abort_handles
            .write()
            .await
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: AbortHandle,
    ) -> u64 {
        let session_id = self.allocate_transport_session_id();
        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        session_id
    }

    pub async fn bind_pending_transport_session_to_peer(
        &self,
        session_id: u64,
        peer_id: &str,
    ) -> bool {
        let abort_handle = self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id);
        let Some(abort_handle) = abort_handle else {
            return false;
        };

        self.transport_session_abort_handles_by_peer
            .write()
            .await
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        true
    }

    pub async fn clear_transport_session_registration(&self, session_id: u64) {
        if self
            .pending_transport_session_abort_handles
            .write()
            .await
            .remove(&session_id)
            .is_some()
        {
            return;
        }

        let mut by_peer = self.transport_session_abort_handles_by_peer.write().await;
        let mut empty_peers = Vec::<String>::new();
        for (peer_id, sessions) in by_peer.iter_mut() {
            if sessions.remove(&session_id).is_some() && sessions.is_empty() {
                empty_peers.push(peer_id.clone());
            }
        }
        for peer_id in empty_peers {
            by_peer.remove(&peer_id);
        }
    }

    pub async fn abort_transport_sessions_for_peer(&self, peer_id: &str) -> usize {
        let sessions = self
            .transport_session_abort_handles_by_peer
            .write()
            .await
            .remove(peer_id)
            .unwrap_or_default();
        let aborted = sessions.len();
        for handle in sessions.into_values() {
            handle.abort();
        }
        aborted
    }
}
