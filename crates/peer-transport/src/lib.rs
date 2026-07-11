use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use core_clipboard::sanitize_clipboard_event_detail;
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
const MAX_ACTIVITY_EVENT_SUMMARIES: usize = 64;
const MAX_DIAGNOSTIC_EVENT_SUMMARIES: usize = 128;

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
    FileTransferCursor {
        transfer_id: String,
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
    LayoutMatrix {
        matrix_spec: String,
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
        let transfer_id = match payload {
            OutboundPayload::FileChunk { transfer_id, .. }
            | OutboundPayload::FileTransferCursor { transfer_id } => transfer_id,
            _ => continue,
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
    discovered_endpoints: &[SocketAddr],
) -> Vec<String> {
    let mut targets = Vec::new();
    for endpoint in discovered_endpoints {
        let endpoint = endpoint.to_string();
        if !targets.iter().any(|target| target == &endpoint) {
            targets.push(endpoint);
        }
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
    pub remaining_chunk_credits: u32,
    pub final_path: PathBuf,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportEventPriority {
    Diagnostic,
    Activity,
}

#[derive(Debug, Clone)]
struct RetainedTransportEvent {
    event: TransportEventRecord,
    first_timestamp: DateTime<Utc>,
    sample_count: u64,
    retained_size_bytes: u64,
    counter_totals: Option<TransportEventCounterTotals>,
    aggregation_key: Option<String>,
    priority: TransportEventPriority,
}

#[derive(Debug, Clone)]
enum TransportEventCounterTotals {
    HookQueueDropped {
        dropped_events: u64,
    },
    BrokerInjectReport {
        injected_frames: u64,
        failed_frames: u64,
    },
}

impl TransportEventCounterTotals {
    fn from_event(event: &TransportEventRecord) -> Option<Self> {
        match event.kind.as_str() {
            "input_hook_queue_dropped" => Some(Self::HookQueueDropped {
                dropped_events: detail_u64(&event.detail, "dropped_events").unwrap_or(0),
            }),
            "input_broker_inject_report" => Some(Self::BrokerInjectReport {
                injected_frames: detail_u64(&event.detail, "injected_frames").unwrap_or(0),
                failed_frames: detail_u64(&event.detail, "failed_frames").unwrap_or(0),
            }),
            _ => None,
        }
    }

    fn merge(&mut self, next: Self) {
        match (self, next) {
            (
                Self::HookQueueDropped { dropped_events },
                Self::HookQueueDropped {
                    dropped_events: next_dropped_events,
                },
            ) => {
                *dropped_events = dropped_events.saturating_add(next_dropped_events);
            }
            (
                Self::BrokerInjectReport {
                    injected_frames,
                    failed_frames,
                },
                Self::BrokerInjectReport {
                    injected_frames: next_injected_frames,
                    failed_frames: next_failed_frames,
                },
            ) => {
                *injected_frames = injected_frames.saturating_add(next_injected_frames);
                *failed_frames = failed_frames.saturating_add(next_failed_frames);
            }
            _ => {}
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::HookQueueDropped { dropped_events } => {
                format!("dropped_events_total={dropped_events}")
            }
            Self::BrokerInjectReport {
                injected_frames,
                failed_frames,
            } => format!(
                "injected_frames_total={injected_frames} failed_frames_total={failed_frames}"
            ),
        }
    }
}

impl RetainedTransportEvent {
    fn new(mut event: TransportEventRecord) -> Self {
        event.detail = sanitize_transport_event_detail_for_retention(&event.kind, &event.detail);
        let first_timestamp = event.timestamp;
        let retained_size_bytes = event.size_bytes;
        let counter_totals = TransportEventCounterTotals::from_event(&event);
        let priority = transport_event_priority(&event.kind);
        let aggregation_key = transport_event_is_aggregated(&event.kind)
            .then(|| transport_event_aggregation_key(&event));
        Self {
            event,
            first_timestamp,
            sample_count: 1,
            retained_size_bytes,
            counter_totals,
            aggregation_key,
            priority,
        }
    }

    fn merge(&mut self, mut event: TransportEventRecord) {
        event.detail = sanitize_transport_event_detail_for_retention(&event.kind, &event.detail);
        self.sample_count = self.sample_count.saturating_add(1);
        self.retained_size_bytes =
            if transport_event_uses_latest_size(&event.kind, &event.direction) {
                event.size_bytes
            } else {
                self.retained_size_bytes.saturating_add(event.size_bytes)
            };
        if let Some(next_totals) = TransportEventCounterTotals::from_event(&event) {
            if let Some(counter_totals) = self.counter_totals.as_mut() {
                counter_totals.merge(next_totals);
            } else {
                self.counter_totals = Some(next_totals);
            }
        }
        self.event = event;
    }

    fn snapshot(&self) -> TransportEventRecord {
        let mut event = self.event.clone();
        event.size_bytes = self.retained_size_bytes;
        if let Some(counter_totals) = &self.counter_totals {
            event.detail = counter_totals.detail();
        }
        if self.sample_count > 1 {
            event.detail = format!(
                "{} sample_count={} first_seen={} last_seen={}",
                event.detail,
                self.sample_count,
                self.first_timestamp.to_rfc3339(),
                event.timestamp.to_rfc3339()
            );
        }
        event
    }
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
    transport_events: Mutex<VecDeque<RetainedTransportEvent>>,
    pub outgoing_input_high_water_by_peer: Mutex<HashMap<String, usize>>,
    pub outgoing_flush_signal: watch::Sender<u64>,
    pub outgoing_flush_generation: AtomicU64,
    pub peer_reconcile_wake: Arc<RuntimeWakeSignal>,
    pub transport_session_registry: Mutex<TransportSessionRegistry>,
    pub next_transport_session_id: AtomicU64,
}

#[derive(Debug, Default)]
pub struct TransportSessionRegistry {
    closed: bool,
    pending_abort_handles: HashMap<u64, AbortHandle>,
    abort_handles_by_peer: HashMap<String, HashMap<u64, AbortHandle>>,
    active_session_by_peer: HashMap<String, ActiveTransportSession>,
}

#[derive(Debug)]
struct ActiveTransportSession {
    session_id: u64,
    preferred: bool,
    cancellation: Arc<RuntimeWakeSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSessionClaim {
    Claimed,
    Replaced { active_session_id: u64 },
    Duplicate { active_session_id: u64 },
    Closed,
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
            transport_session_registry: Mutex::new(TransportSessionRegistry::default()),
            next_transport_session_id: AtomicU64::new(1),
        }
    }
}

impl TransportRuntimeState {
    pub async fn clear(&self) -> usize {
        self.reconnect_generation_by_peer.write().await.clear();
        self.outgoing_input_payloads.write().await.clear();
        self.outgoing_bulk_payloads.write().await.clear();
        if let Ok(mut events) = self.transport_events.lock() {
            events.clear();
        }
        if let Ok(mut high_water) = self.outgoing_input_high_water_by_peer.lock() {
            high_water.clear();
        }
        let (pending_sessions, peer_sessions) =
            if let Ok(mut registry) = self.transport_session_registry.lock() {
                let pending_sessions = registry
                    .pending_abort_handles
                    .drain()
                    .map(|(_, handle)| handle)
                    .collect::<Vec<_>>();
                let peer_sessions = registry
                    .abort_handles_by_peer
                    .drain()
                    .flat_map(|(_, sessions)| sessions.into_values())
                    .collect::<Vec<_>>();
                registry.active_session_by_peer.clear();
                (pending_sessions, peer_sessions)
            } else {
                (Vec::new(), Vec::new())
            };
        let aborted = pending_sessions.len() + peer_sessions.len();
        for handle in pending_sessions.into_iter().chain(peer_sessions) {
            handle.abort();
        }
        aborted
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

        let retained = RetainedTransportEvent::new(event);
        if let Some(key) = retained.aggregation_key.as_deref()
            && let Some(index) = events
                .iter()
                .position(|existing| existing.aggregation_key.as_deref() == Some(key))
        {
            let Some(mut existing) = events.remove(index) else {
                return;
            };
            existing.merge(retained.event);
            events.push_back(existing);
            return;
        }

        if retained.priority == TransportEventPriority::Activity
            && events
                .iter()
                .filter(|event| event.priority == TransportEventPriority::Activity)
                .count()
                >= MAX_ACTIVITY_EVENT_SUMMARIES
            && let Some(index) = events
                .iter()
                .position(|event| event.priority == TransportEventPriority::Activity)
        {
            events.remove(index);
        }

        if retained.priority == TransportEventPriority::Diagnostic
            && retained.aggregation_key.is_some()
            && events
                .iter()
                .filter(|event| {
                    event.priority == TransportEventPriority::Diagnostic
                        && event.aggregation_key.is_some()
                })
                .count()
                >= MAX_DIAGNOSTIC_EVENT_SUMMARIES
            && let Some(index) = events.iter().position(|event| {
                event.priority == TransportEventPriority::Diagnostic
                    && event.aggregation_key.is_some()
            })
        {
            events.remove(index);
        }

        if events.len() >= MAX_TRANSPORT_EVENTS {
            let activity_index = events
                .iter()
                .position(|event| event.priority == TransportEventPriority::Activity);
            match (retained.priority, activity_index) {
                (_, Some(index)) => {
                    events.remove(index);
                }
                (TransportEventPriority::Diagnostic, None) => {
                    events.pop_front();
                }
                (TransportEventPriority::Activity, None) => return,
            }
        }
        events.push_back(retained);
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
        events
            .iter()
            .map(RetainedTransportEvent::snapshot)
            .collect()
    }

    pub fn register_pending_transport_session(&self, abort_handle: AbortHandle) -> u64 {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            abort_handle.abort();
            return 0;
        };

        if registry.closed {
            abort_handle.abort();
            return 0;
        }
        let session_id = self.allocate_transport_session_id();
        registry
            .pending_abort_handles
            .insert(session_id, abort_handle);
        session_id
    }

    pub fn claim_transport_session(
        &self,
        peer_id: &str,
        session_id: u64,
        preferred: bool,
        cancellation: Arc<RuntimeWakeSignal>,
    ) -> TransportSessionClaim {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            return TransportSessionClaim::Closed;
        };
        if registry.closed {
            return TransportSessionClaim::Closed;
        }
        if let Some(active) = registry.active_session_by_peer.get(peer_id) {
            if !preferred || active.preferred {
                return TransportSessionClaim::Duplicate {
                    active_session_id: active.session_id,
                };
            }

            let active_session_id = active.session_id;
            let active_cancellation = active.cancellation.clone();
            registry.active_session_by_peer.insert(
                peer_id.to_string(),
                ActiveTransportSession {
                    session_id,
                    preferred,
                    cancellation,
                },
            );
            let abort_handle = registry
                .abort_handles_by_peer
                .get_mut(peer_id)
                .and_then(|sessions| sessions.remove(&active_session_id));
            if registry
                .abort_handles_by_peer
                .get(peer_id)
                .is_some_and(HashMap::is_empty)
            {
                registry.abort_handles_by_peer.remove(peer_id);
            }
            drop(registry);
            if active_cancellation.trigger() {
                active_cancellation.notify_one();
            }
            if let Some(abort_handle) = abort_handle {
                abort_handle.abort();
            }
            return TransportSessionClaim::Replaced { active_session_id };
        }
        registry.active_session_by_peer.insert(
            peer_id.to_string(),
            ActiveTransportSession {
                session_id,
                preferred,
                cancellation,
            },
        );
        TransportSessionClaim::Claimed
    }

    pub fn clear_active_transport_session(&self, peer_id: &str, session_id: u64) -> bool {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            return false;
        };
        if registry
            .active_session_by_peer
            .get(peer_id)
            .is_some_and(|active| active.session_id == session_id)
        {
            registry.active_session_by_peer.remove(peer_id);
            return true;
        }
        false
    }

    pub fn has_active_transport_session(&self, peer_id: &str) -> bool {
        self.transport_session_registry
            .lock()
            .is_ok_and(|registry| registry.active_session_by_peer.contains_key(peer_id))
    }

    pub fn register_transport_session_for_peer(
        &self,
        peer_id: &str,
        abort_handle: AbortHandle,
    ) -> u64 {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            abort_handle.abort();
            return 0;
        };

        if registry.closed {
            abort_handle.abort();
            return 0;
        }
        let session_id = self.allocate_transport_session_id();
        registry
            .abort_handles_by_peer
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        session_id
    }

    pub fn bind_pending_transport_session_to_peer(&self, session_id: u64, peer_id: &str) -> bool {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            return false;
        };

        let abort_handle = registry.pending_abort_handles.remove(&session_id);
        let Some(abort_handle) = abort_handle else {
            return false;
        };
        if registry.closed {
            abort_handle.abort();
            return false;
        }

        registry
            .abort_handles_by_peer
            .entry(peer_id.to_string())
            .or_default()
            .insert(session_id, abort_handle);
        true
    }

    pub async fn clear_transport_session_registration(&self, session_id: u64) {
        let Ok(mut registry) = self.transport_session_registry.lock() else {
            return;
        };

        if registry.pending_abort_handles.remove(&session_id).is_some() {
            return;
        }
        let mut empty_peers = Vec::<String>::new();
        for (peer_id, sessions) in registry.abort_handles_by_peer.iter_mut() {
            if sessions.remove(&session_id).is_some() && sessions.is_empty() {
                empty_peers.push(peer_id.clone());
            }
        }
        for peer_id in empty_peers {
            registry.abort_handles_by_peer.remove(&peer_id);
        }
        registry
            .active_session_by_peer
            .retain(|_, active| active.session_id != session_id);
    }

    pub async fn abort_transport_sessions_for_peer(&self, peer_id: &str) -> usize {
        let sessions = self
            .transport_session_registry
            .lock()
            .ok()
            .map(|mut registry| {
                registry.active_session_by_peer.remove(peer_id);
                registry
                    .abort_handles_by_peer
                    .remove(peer_id)
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let aborted = sessions.len();
        for handle in sessions.into_values() {
            handle.abort();
        }
        aborted
    }

    pub fn begin_transport_session_shutdown(&self) {
        if let Ok(mut registry) = self.transport_session_registry.lock() {
            registry.closed = true;
        }
    }

    pub async fn abort_all_transport_sessions(&self) -> usize {
        let (pending_sessions, peer_sessions) =
            if let Ok(mut registry) = self.transport_session_registry.lock() {
                let pending_sessions = registry
                    .pending_abort_handles
                    .drain()
                    .map(|(_, handle)| handle)
                    .collect::<Vec<_>>();
                let peer_sessions = registry
                    .abort_handles_by_peer
                    .drain()
                    .flat_map(|(_, sessions)| sessions.into_values())
                    .collect::<Vec<_>>();
                registry.active_session_by_peer.clear();
                (pending_sessions, peer_sessions)
            } else {
                (Vec::new(), Vec::new())
            };
        let aborted = pending_sessions.len() + peer_sessions.len();
        for handle in pending_sessions.into_iter().chain(peer_sessions) {
            handle.abort();
        }
        aborted
    }
}

fn transport_event_priority(kind: &str) -> TransportEventPriority {
    if matches!(
        kind,
        "runtime_wake"
            | "input_runtime_wake"
            | "input_frame"
            | "input_inject_queued"
            | "input_inject_applied"
            | "input_broker_inject_dispatched"
            | "input_queue_coalesced"
            | "anti_idle_pulse_sent"
            | "anti_idle_pulse_received"
            | "file_transfer_progress"
            | "peer_reconcile_trigger"
    ) {
        TransportEventPriority::Activity
    } else {
        TransportEventPriority::Diagnostic
    }
}

fn transport_event_is_aggregated(kind: &str) -> bool {
    let repeated_failure = transport_event_is_repeated_failure(kind);
    transport_event_priority(kind) == TransportEventPriority::Activity
        || repeated_failure
        || kind == "clipboard_image_rejected"
        || matches!(
            kind,
            "input_inject_failed"
                | "input_inject_skipped"
                | "input_inject_dropped"
                | "input_inject_dropped_permanent"
                | "input_inject_retry_scheduled"
                | "input_broker_inject_report"
                | "input_queue_coalesced"
                | "input_queue_overflow_drop"
        )
}

fn transport_event_is_repeated_failure(kind: &str) -> bool {
    !kind.starts_with("clipboard")
        && (kind.ends_with("_failed") || kind.ends_with("_rejected") || kind.ends_with("_dropped"))
}

fn sanitize_transport_event_detail_for_retention(kind: &str, detail: &str) -> String {
    let detail = sanitize_clipboard_event_detail(kind, detail);
    if matches!(
        kind,
        "transport_transfer_rejected" | "file_transfer_rejected"
    ) {
        canonical_file_transfer_rejection_reason(&detail)
    } else {
        detail
    }
}

fn transport_event_aggregation_key(event: &TransportEventRecord) -> String {
    let repeated_failure = transport_event_is_repeated_failure(&event.kind);
    let bounded_failure = repeated_failure || event.kind == "clipboard_image_rejected";
    let dimensions = if bounded_failure {
        canonical_failure_cause(&event.kind, &event.detail)
    } else {
        match event.kind.as_str() {
            "input_queue_coalesced" => canonical_detail_tokens(&event.detail, &["queue"]),
            "input_queue_overflow_drop" => {
                canonical_detail_tokens(&event.detail, &["queue", "reason"])
            }
            "input_hook_queue_dropped" | "input_broker_inject_report" => String::new(),
            "file_transfer_progress" => canonical_detail_tokens(&event.detail, &["transfer_id"]),
            _ => event
                .detail
                .split_whitespace()
                .filter(|token| !transport_event_token_is_volatile(token))
                .collect::<Vec<_>>()
                .join(" "),
        }
    };
    let peer_id = if bounded_failure {
        pseudonymous_peer_dimension(&event.peer_id)
    } else {
        event.peer_id.clone()
    };
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{dimensions}",
        event.direction, event.kind, peer_id
    )
}

fn pseudonymous_peer_dimension(peer_id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    peer_id.hash(&mut hasher);
    format!("peer={:016x}", hasher.finish())
}

fn canonical_failure_cause(kind: &str, detail: &str) -> String {
    if matches!(
        kind,
        "transport_transfer_rejected" | "file_transfer_rejected"
    ) {
        return canonical_file_transfer_rejection_reason(detail);
    }

    if let Some(reason) = detail_token(detail, "reason") {
        return format!("reason={}", bounded_dimension(reason));
    }

    let cause = match kind {
        "input_inject_failed" => classify_input_inject_failure(detail),
        "pairing_reconnect_failed" => classify_connection_failure(detail),
        "transport_reachability_failed" => detail_token(detail, "failure_reason")
            .map(classify_connection_failure)
            .unwrap_or_else(|| classify_connection_failure(detail)),
        "input_inject_dropped"
            if detail
                .split_whitespace()
                .any(|token| token == "dropped_oldest") =>
        {
            "dropped_oldest"
        }
        _ => "unspecified",
    };
    format!("cause={cause}")
}

fn classify_input_inject_failure(detail: &str) -> &'static str {
    let normalized = detail.to_ascii_lowercase();
    if normalized.contains("interactive_session")
        || normalized.contains("interactive session")
        || normalized.contains("session 0")
    {
        "interactive_session"
    } else if normalized.contains("access_denied")
        || normalized.contains("access denied")
        || normalized.contains("os error 5")
    {
        "access_denied"
    } else if normalized.contains("timeout") || normalized.contains("timed out") {
        "timeout"
    } else if normalized.contains("transient") {
        "transient"
    } else {
        "inject_error"
    }
}

fn classify_connection_failure(detail: &str) -> &'static str {
    let normalized = detail.to_ascii_lowercase();
    if normalized.contains("refused") {
        "refused"
    } else if normalized.contains("timeout") || normalized.contains("timed out") {
        "timeout"
    } else if normalized.contains("unreachable") {
        "unreachable"
    } else if normalized.contains("dns") || normalized.contains("resolve") {
        "resolution"
    } else if normalized.contains("tls") || normalized.contains("certificate") {
        "tls"
    } else {
        "connection_error"
    }
}

fn canonical_file_transfer_rejection_reason(detail: &str) -> String {
    let reason = detail_token(detail, "reason").unwrap_or("other");
    let reason = match reason {
        "too_many_transfers"
        | "duplicate_transfer_id"
        | "receive_policy_denied"
        | "invalid_total_size"
        | "temp_reserve_failed"
        | "chunk_size_invalid"
        | "chunk_exceeds_total"
        | "temp_write_failed"
        | "size_mismatch"
        | "temp_flush_failed"
        | "temp_sync_failed" => reason,
        _ => "other",
    };
    format!("reason={reason}")
}

fn detail_token<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    detail
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{key}=")))
}

fn bounded_dimension(value: &str) -> String {
    let bounded = value
        .chars()
        .take(48)
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .collect::<String>();
    if bounded.is_empty() {
        "other".to_string()
    } else {
        bounded
    }
}

fn canonical_detail_tokens(detail: &str, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|expected_key| {
            detail.split_whitespace().find(|token| {
                token
                    .split_once('=')
                    .is_some_and(|(key, _value)| key == *expected_key)
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn transport_event_uses_latest_size(kind: &str, direction: &str) -> bool {
    (kind == "file_transfer_progress" && direction == "incoming")
        || kind == "clipboard_image_rejected"
}

fn detail_u64(detail: &str, expected_key: &str) -> Option<u64> {
    detail.split_whitespace().find_map(|token| {
        let (key, value) = token.split_once('=')?;
        if key == expected_key {
            value.parse::<u64>().ok()
        } else {
            None
        }
    })
}

fn transport_event_token_is_volatile(token: &str) -> bool {
    let Some((key, _value)) = token.split_once('=') else {
        return false;
    };
    key == "sequence"
        || key == "older_sequence"
        || key == "newer_sequence"
        || key == "attempt"
        || key == "attempts"
        || key == "generation"
        || key == "queue_depth"
        || key == "retry_count"
        || key == "events"
        || key == "injected_frames"
        || key == "failed_frames"
        || key == "merged_events"
        || key == "dropped_events"
        || key == "bytes_received"
        || key == "total_bytes"
        || key == "offset_bytes"
        || key == "length_bytes"
        || key.ends_with("_count")
        || key.ends_with("_ms")
        || key.ends_with("_unix_ms")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event(
        timestamp: DateTime<Utc>,
        kind: &str,
        direction: &str,
        peer_id: &str,
        detail: String,
    ) -> TransportEventRecord {
        TransportEventRecord {
            timestamp,
            direction: direction.to_string(),
            kind: kind.to_string(),
            peer_id: peer_id.to_string(),
            detail,
            size_bytes: 1,
        }
    }

    #[tokio::test]
    async fn sixty_seconds_of_input_activity_preserves_causal_diagnostics() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        state.record_transport_event(test_event(
            started,
            "input_handoff",
            "local",
            "peer-a",
            "direction=left activated=true".to_string(),
        ));
        state.record_transport_event(test_event(
            started + chrono::Duration::milliseconds(1),
            "transport_reachability_failed",
            "outgoing",
            "peer-a",
            "stage=send reason=unreachable".to_string(),
        ));

        // Four thousand 15 ms samples model one minute of sustained pointer activity.
        for sample in 0..4_000i64 {
            let timestamp = started + chrono::Duration::milliseconds(sample * 15);
            state.record_transport_event(test_event(
                timestamp,
                "runtime_wake",
                "local",
                "none",
                "channel=input_capture source=input_broker_exchange".to_string(),
            ));
            state.record_transport_event(test_event(
                timestamp,
                "input_frame",
                "outgoing",
                "peer-a",
                format!("sequence={sample} capture_to_send_ms=1 captured_at_unix_ms={sample}"),
            ));
            state.record_transport_event(test_event(
                timestamp,
                "input_inject_failed",
                "local",
                "peer-a",
                format!(
                    "sequence={sample} queue_wait_ms=1 capture_to_fail_ms=2 receive_to_fail_ms=1 interactive_session_unavailable"
                ),
            ));
            state.record_transport_event(test_event(
                timestamp,
                "anti_idle_pulse_sent",
                "outgoing",
                "peer-a",
                "source=local_activity".to_string(),
            ));
            state.record_transport_event(test_event(
                timestamp,
                "peer_reconcile_trigger",
                "local",
                "peer-a",
                "source=runtime_wake".to_string(),
            ));
            state.record_transport_event(test_event(
                timestamp,
                "pairing_reconnect_failed",
                "outgoing",
                "peer-a",
                format!("attempt={sample} reason=unreachable"),
            ));
        }

        let events = state.transport_events_snapshot().await;
        assert!(events.len() <= MAX_TRANSPORT_EVENTS);
        assert!(events.iter().any(|event| event.kind == "input_handoff"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "transport_reachability_failed")
        );
        for kind in [
            "runtime_wake",
            "input_frame",
            "input_inject_failed",
            "anti_idle_pulse_sent",
            "peer_reconcile_trigger",
            "pairing_reconnect_failed",
        ] {
            let summary = events
                .iter()
                .find(|event| event.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind} summary"));
            assert!(summary.detail.contains("sample_count=4000"));
            assert!(summary.detail.contains("first_seen="));
            assert!(summary.detail.contains("last_seen="));
        }
    }

    #[tokio::test]
    async fn producer_shaped_activity_uses_canonical_keys_and_size_semantics() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        state.record_transport_event(test_event(
            started,
            "input_handoff",
            "local",
            "peer-a",
            "direction=left activated=true".to_string(),
        ));

        let mut expected_merged_events = 0u64;
        let mut expected_hook_drops = 0u64;
        let mut expected_injected_frames = 0u64;
        let mut expected_failed_frames = 0u64;
        let mut expected_failed_input_events = 0u64;
        for sample in 0..4_000u64 {
            let timestamp = started + chrono::Duration::milliseconds((sample * 15) as i64);
            let merged_events = sample % 4 + 1;
            expected_merged_events += merged_events;

            let mut coalesced = test_event(
                timestamp,
                "input_queue_coalesced",
                "local",
                "peer-a",
                format!(
                    "queue=outgoing_input older_sequence={sample} newer_sequence={} merged_events={merged_events}",
                    sample + 1
                ),
            );
            coalesced.size_bytes = merged_events;
            state.record_transport_event(coalesced);

            let dropped_events = sample % 5 + 1;
            expected_hook_drops += dropped_events;
            let mut hook_drop = test_event(
                timestamp,
                "input_hook_queue_dropped",
                "local",
                "none",
                format!("dropped_events={dropped_events}"),
            );
            hook_drop.size_bytes = 0;
            state.record_transport_event(hook_drop);

            let injected_frames = sample % 7;
            let failed_frames = sample % 3 + 1;
            expected_injected_frames += injected_frames;
            expected_failed_frames += failed_frames;
            let mut broker_report = test_event(
                timestamp,
                "input_broker_inject_report",
                "local",
                "none",
                format!("injected_frames={injected_frames} failed_frames={failed_frames}"),
            );
            broker_report.size_bytes = 0;
            state.record_transport_event(broker_report);

            let failed_input_events = sample % 4 + 1;
            expected_failed_input_events += failed_input_events;
            let mut inject_failed = test_event(
                timestamp,
                "input_inject_failed",
                "local",
                "peer-a",
                format!(
                    "sequence={sample} queue_wait_ms=1 capture_to_fail_ms=2 receive_to_fail_ms=1 transient_inject_failure_{sample}"
                ),
            );
            inject_failed.size_bytes = failed_input_events;
            state.record_transport_event(inject_failed);

            let received = (sample + 1) * 64;
            let mut inbound_progress = test_event(
                timestamp,
                "file_transfer_progress",
                "incoming",
                "peer-a",
                format!(
                    "transfer_id=file-in bytes_received={received} total_bytes={}",
                    4_000 * 64
                ),
            );
            inbound_progress.size_bytes = received;
            state.record_transport_event(inbound_progress);

            let mut outbound_progress = test_event(
                timestamp,
                "file_transfer_progress",
                "outgoing",
                "peer-a",
                format!(
                    "transfer_id=file-out offset_bytes={} length_bytes=64",
                    sample * 64
                ),
            );
            outbound_progress.size_bytes = 64;
            state.record_transport_event(outbound_progress);
        }

        let events = state.transport_events_snapshot().await;
        assert!(events.iter().any(|event| event.kind == "input_handoff"));

        let coalesced = events
            .iter()
            .filter(|event| event.kind == "input_queue_coalesced")
            .collect::<Vec<_>>();
        assert_eq!(coalesced.len(), 1);
        assert!(coalesced[0].detail.contains("sample_count=4000"));
        assert_eq!(coalesced[0].size_bytes, expected_merged_events);

        let hook_drops = events
            .iter()
            .filter(|event| event.kind == "input_hook_queue_dropped")
            .collect::<Vec<_>>();
        assert_eq!(hook_drops.len(), 1);
        assert!(hook_drops[0].detail.contains("sample_count=4000"));
        assert!(
            hook_drops[0]
                .detail
                .contains(&format!("dropped_events_total={expected_hook_drops}"))
        );

        let broker_reports = events
            .iter()
            .filter(|event| event.kind == "input_broker_inject_report")
            .collect::<Vec<_>>();
        assert_eq!(broker_reports.len(), 1);
        assert!(broker_reports[0].detail.contains("sample_count=4000"));
        assert!(
            broker_reports[0]
                .detail
                .contains(&format!("injected_frames_total={expected_injected_frames}"))
        );
        assert!(
            broker_reports[0]
                .detail
                .contains(&format!("failed_frames_total={expected_failed_frames}"))
        );

        let inject_failures = events
            .iter()
            .filter(|event| event.kind == "input_inject_failed")
            .collect::<Vec<_>>();
        assert_eq!(inject_failures.len(), 1);
        assert!(inject_failures[0].detail.contains("sample_count=4000"));
        assert_eq!(inject_failures[0].size_bytes, expected_failed_input_events);

        let inbound_progress = events
            .iter()
            .find(|event| event.kind == "file_transfer_progress" && event.direction == "incoming")
            .expect("inbound transfer summary");
        assert!(inbound_progress.detail.contains("sample_count=4000"));
        assert_eq!(inbound_progress.size_bytes, 4_000 * 64);

        let outbound_progress = events
            .iter()
            .find(|event| event.kind == "file_transfer_progress" && event.direction == "outgoing")
            .expect("outbound transfer summary");
        assert!(outbound_progress.detail.contains("sample_count=4000"));
        assert_eq!(outbound_progress.size_bytes, 4_000 * 64);
    }

    #[tokio::test]
    async fn untrusted_failure_identifiers_do_not_create_unbounded_diagnostic_keys() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        state.record_transport_event(test_event(
            started,
            "input_handoff",
            "local",
            "trusted-peer",
            "direction=left activated=true".to_string(),
        ));

        for sample in 0..4_000u64 {
            let mut rejection = test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "transport_transfer_rejected",
                "incoming",
                "remote-peer",
                format!(
                    "reason=invalid_total_size transfer_id=remote-transfer-{sample} error=remote-error-{sample}"
                ),
            );
            rejection.size_bytes = 1;
            state.record_transport_event(rejection);
        }

        let events = state.transport_events_snapshot().await;
        assert!(events.iter().any(|event| event.kind == "input_handoff"));
        let rejections = events
            .iter()
            .filter(|event| event.kind == "transport_transfer_rejected")
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].detail.contains("sample_count=4000"));
        assert!(rejections[0].detail.contains("reason=invalid_total_size"));
        assert!(!rejections[0].detail.contains("remote-transfer-"));
        assert!(!rejections[0].detail.contains("remote-error-"));
        assert_eq!(rejections[0].size_bytes, 4_000);
    }

    #[tokio::test]
    async fn clipboard_image_rejections_are_bounded_and_keep_safe_causal_metadata() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_clipboard_reject_878bfb34";
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        state.record_transport_event(test_event(
            started,
            "input_handoff",
            "local",
            "trusted-peer",
            "direction=left activated=true".to_string(),
        ));

        for sample in 0..4_000u64 {
            let mut rejection = test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "clipboard_image_rejected",
                "incoming",
                "remote-peer",
                format!(
                    "payload_type=bmp disposition=rejected reason=size_mismatch expected_bytes=4000 received_bytes={sample} expected_hash={SECRET} sample_count=999 first_seen=1999-01-01T00:00:00Z"
                ),
            );
            rejection.size_bytes = sample;
            state.record_transport_event(rejection);
        }

        let events = state.transport_events_snapshot().await;
        assert!(events.iter().any(|event| event.kind == "input_handoff"));
        let rejections = events
            .iter()
            .filter(|event| event.kind == "clipboard_image_rejected")
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].detail.contains("sample_count=4000"));
        assert!(rejections[0].detail.contains("expected_bytes=4000"));
        assert!(rejections[0].detail.contains("received_bytes=3999"));
        assert!(!rejections[0].detail.contains("sample_count=999"));
        assert!(!rejections[0].detail.contains("1999-01-01"));
        assert!(!rejections[0].detail.contains(SECRET));
        assert_eq!(rejections[0].size_bytes, 3_999);
    }

    #[test]
    fn failure_aggregation_keys_preserve_peer_and_bounded_cause_dimensions() {
        let started = Utc::now();
        let key = |kind: &str, peer_id: &str, detail: &str| {
            transport_event_aggregation_key(&test_event(
                started,
                kind,
                "local",
                peer_id,
                detail.to_string(),
            ))
        };

        assert_ne!(
            key(
                "input_inject_failed",
                "peer-a",
                "sequence=1 interactive_session_unavailable"
            ),
            key(
                "input_inject_failed",
                "peer-b",
                "sequence=1 interactive_session_unavailable"
            )
        );
        assert_ne!(
            key(
                "input_inject_failed",
                "peer-a",
                "sequence=1 interactive_session_unavailable"
            ),
            key(
                "input_inject_failed",
                "peer-a",
                "sequence=2 access denied (os error 5)"
            )
        );
        assert_ne!(
            key(
                "pairing_reconnect_failed",
                "peer-a",
                "trust_committed=true error=connection refused"
            ),
            key(
                "pairing_reconnect_failed",
                "peer-a",
                "trust_committed=true error=operation timed out"
            )
        );
        assert_ne!(
            key(
                "transport_reachability_failed",
                "peer-a",
                "tcp_transport_reachability=failed failure_reason=refused"
            ),
            key(
                "transport_reachability_failed",
                "peer-a",
                "tcp_transport_reachability=failed failure_reason=timeout"
            )
        );
        assert_ne!(
            key(
                "input_inject_dropped",
                "peer-a",
                "sequence=1 dropped_oldest capture_age_ms=10"
            ),
            key(
                "input_inject_dropped",
                "peer-a",
                "sequence=2 reason=queue_full capture_age_ms=20"
            )
        );
    }

    #[tokio::test]
    async fn repeated_failures_do_not_attribute_one_peers_cause_to_another() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        for sample in 0..4u64 {
            state.record_transport_event(test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "input_inject_failed",
                "local",
                "peer-interactive",
                format!("sequence={sample} interactive_session_unavailable"),
            ));
            state.record_transport_event(test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "input_inject_failed",
                "local",
                "peer-denied",
                format!("sequence={sample} access denied (os error 5)"),
            ));
        }

        let failures = state
            .transport_events_snapshot()
            .await
            .into_iter()
            .filter(|event| event.kind == "input_inject_failed")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        let interactive = failures
            .iter()
            .find(|event| event.peer_id == "peer-interactive")
            .expect("interactive peer failure summary");
        assert!(
            interactive
                .detail
                .contains("interactive_session_unavailable")
        );
        assert!(!interactive.detail.contains("access denied"));
        assert!(interactive.detail.contains("sample_count=4"));
        let denied = failures
            .iter()
            .find(|event| event.peer_id == "peer-denied")
            .expect("access-denied peer failure summary");
        assert!(denied.detail.contains("access denied"));
        assert!(!denied.detail.contains("interactive_session_unavailable"));
        assert!(denied.detail.contains("sample_count=4"));
    }

    #[tokio::test]
    async fn remote_file_transfer_rejection_detail_is_bounded_before_retention() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_remote_transfer_50d281a4";
        let state = TransportRuntimeState::default();
        let started = Utc::now();

        for sample in 0..4_000u64 {
            let mut rejection = test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "file_transfer_rejected",
                "outgoing",
                "remote-peer",
                format!("transfer_id={SECRET}-{sample} reason={SECRET}-{sample} trailing={SECRET}"),
            );
            rejection.size_bytes = 1;
            state.record_transport_event(rejection);
        }

        let rejections = state
            .transport_events_snapshot()
            .await
            .into_iter()
            .filter(|event| event.kind == "file_transfer_rejected")
            .collect::<Vec<_>>();
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].detail.starts_with("reason=other "));
        assert!(rejections[0].detail.contains("sample_count=4000"));
        assert!(!rejections[0].detail.contains(SECRET));
        assert!(rejections[0].detail.len() <= 192);
        assert_eq!(rejections[0].size_bytes, 4_000);
    }

    #[tokio::test]
    async fn unique_failure_reasons_are_capped_below_the_diagnostic_ring_budget() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        state.record_transport_event(test_event(
            started,
            "input_handoff",
            "local",
            "trusted-peer",
            "direction=left activated=true".to_string(),
        ));

        for sample in 0..4_000u64 {
            state.record_transport_event(test_event(
                started + chrono::Duration::milliseconds(sample as i64),
                "transport_transfer_rejected",
                "incoming",
                &format!("remote-{sample}"),
                format!("reason=remote_reason_{sample} transfer_id=remote-transfer-{sample}"),
            ));
        }

        let events = state.transport_events_snapshot().await;
        assert!(events.iter().any(|event| event.kind == "input_handoff"));
        assert!(
            events
                .iter()
                .filter(|event| event.kind == "transport_transfer_rejected")
                .count()
                <= MAX_DIAGNOSTIC_EVENT_SUMMARIES
        );
        assert!(events.len() < MAX_TRANSPORT_EVENTS);
    }

    #[tokio::test]
    async fn activity_budget_cannot_evict_diagnostic_records() {
        let state = TransportRuntimeState::default();
        let started = Utc::now();
        for index in 0..480i64 {
            state.record_transport_event(test_event(
                started + chrono::Duration::milliseconds(index),
                "causal_transition",
                "local",
                "peer-a",
                format!("transition={index}"),
            ));
        }
        for source in 0..200i64 {
            state.record_transport_event(test_event(
                started + chrono::Duration::seconds(source + 1),
                "runtime_wake",
                "local",
                "none",
                format!("channel=input_capture source=source_{source}"),
            ));
        }

        let events = state.transport_events_snapshot().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == "causal_transition")
                .count(),
            480
        );
        assert!(
            events
                .iter()
                .filter(|event| event.kind == "runtime_wake")
                .count()
                <= MAX_ACTIVITY_EVENT_SUMMARIES
        );
        assert!(events.len() <= MAX_TRANSPORT_EVENTS);
    }

    #[tokio::test]
    async fn clipboard_content_is_removed_before_event_retention() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_945bbd71";
        let state = TransportRuntimeState::default();

        state.record_transport_event(test_event(
            Utc::now(),
            "clipboard_text",
            "incoming",
            "peer-a",
            format!("payload_type=text disposition=received hash=abc preview={SECRET} {SECRET}"),
        ));

        let events = state.transport_events_snapshot().await;
        let rendered = format!("{events:?}");
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("hash="));
        assert_eq!(events[0].detail, "payload_type=text disposition=received");
    }

    #[tokio::test]
    async fn closed_session_registry_aborts_child_spawned_before_registration() {
        let state = TransportRuntimeState::default();
        let child = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        state.begin_transport_session_shutdown();
        let session_id = state.register_transport_session_for_peer("peer-a", child.abort_handle());

        assert_eq!(
            session_id, 0,
            "registration should be refused after shutdown begins"
        );
        let join_error = child
            .await
            .expect_err("child spawned before registration should be aborted");
        assert!(join_error.is_cancelled());
        assert_eq!(
            state.abort_all_transport_sessions().await,
            0,
            "refused registration must not leave a drainable session behind"
        );
    }

    #[test]
    fn preferred_transport_session_claim_wins_until_cleared() {
        let state = TransportRuntimeState::default();

        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                10,
                true,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Claimed
        );
        assert!(state.has_active_transport_session("peer-a"));
        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                11,
                false,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Duplicate {
                active_session_id: 10
            }
        );
        assert!(
            !state.clear_active_transport_session("peer-a", 11),
            "non-owner session must not clear the active claim"
        );
        assert!(state.clear_active_transport_session("peer-a", 10));
        assert!(!state.has_active_transport_session("peer-a"));
        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                12,
                false,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Claimed
        );
    }

    #[test]
    fn crossed_claim_permutations_converge_on_the_same_preferred_connection() {
        for local_outbound_finishes_first in [false, true] {
            let endpoint_a = TransportRuntimeState::default();
            let endpoint_b = TransportRuntimeState::default();
            let pair_one_a = Arc::new(RuntimeWakeSignal::default());
            let pair_one_b = Arc::new(RuntimeWakeSignal::default());
            let pair_two_a = Arc::new(RuntimeWakeSignal::default());
            let pair_two_b = Arc::new(RuntimeWakeSignal::default());

            if local_outbound_finishes_first {
                assert_eq!(
                    endpoint_a.claim_transport_session("peer-b", 10, true, pair_one_a.clone()),
                    TransportSessionClaim::Claimed
                );
                assert_eq!(
                    endpoint_b.claim_transport_session("peer-a", 21, false, pair_two_b.clone()),
                    TransportSessionClaim::Claimed
                );
                assert_eq!(
                    endpoint_b.claim_transport_session("peer-a", 20, true, pair_one_b.clone()),
                    TransportSessionClaim::Replaced {
                        active_session_id: 21
                    }
                );
                assert_eq!(
                    endpoint_a.claim_transport_session("peer-b", 11, false, pair_two_a.clone()),
                    TransportSessionClaim::Duplicate {
                        active_session_id: 10
                    }
                );
                assert!(pair_two_b.take_pending());
            } else {
                assert_eq!(
                    endpoint_a.claim_transport_session("peer-b", 11, false, pair_two_a.clone()),
                    TransportSessionClaim::Claimed
                );
                assert_eq!(
                    endpoint_b.claim_transport_session("peer-a", 20, true, pair_one_b.clone()),
                    TransportSessionClaim::Claimed
                );
                assert_eq!(
                    endpoint_a.claim_transport_session("peer-b", 10, true, pair_one_a.clone()),
                    TransportSessionClaim::Replaced {
                        active_session_id: 11
                    }
                );
                assert_eq!(
                    endpoint_b.claim_transport_session("peer-a", 21, false, pair_two_b.clone()),
                    TransportSessionClaim::Duplicate {
                        active_session_id: 20
                    }
                );
                assert!(pair_two_a.take_pending());
            }

            assert!(endpoint_a.clear_active_transport_session("peer-b", 10));
            assert!(endpoint_b.clear_active_transport_session("peer-a", 20));
            assert!(!endpoint_a.clear_active_transport_session("peer-b", 11));
            assert!(!endpoint_b.clear_active_transport_session("peer-a", 21));
            assert!(!pair_one_a.take_pending());
            assert!(!pair_one_b.take_pending());
        }
    }

    #[tokio::test]
    async fn preferred_claim_aborts_registered_nonpreferred_owner() {
        let state = TransportRuntimeState::default();
        let child = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let nonpreferred_id =
            state.register_transport_session_for_peer("peer-a", child.abort_handle());
        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                nonpreferred_id,
                false,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Claimed
        );
        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                99,
                true,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Replaced {
                active_session_id: nonpreferred_id
            }
        );
        let join_error = child
            .await
            .expect_err("registered nonpreferred owner should be aborted");
        assert!(join_error.is_cancelled());
        assert!(state.clear_active_transport_session("peer-a", 99));
    }

    #[tokio::test]
    async fn aborting_peer_sessions_clears_active_claim() {
        let state = TransportRuntimeState::default();
        let child = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let session_id = state.register_transport_session_for_peer("peer-a", child.abort_handle());
        assert_eq!(
            state.claim_transport_session(
                "peer-a",
                session_id,
                true,
                Arc::new(RuntimeWakeSignal::default()),
            ),
            TransportSessionClaim::Claimed
        );

        assert_eq!(state.abort_transport_sessions_for_peer("peer-a").await, 1);
        assert!(!state.has_active_transport_session("peer-a"));
        let join_error = child.await.expect_err("registered child should be aborted");
        assert!(join_error.is_cancelled());
    }
}
