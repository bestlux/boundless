use std::{
    collections::{HashMap, VecDeque},
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

pub const FILE_TRANSFER_INITIAL_CHUNK_CREDITS: u32 = 8;
pub const FILE_TRANSFER_MAX_TRACKED_CHUNK_CREDITS: u32 = 256;

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
            transport_events: Mutex::new(VecDeque::with_capacity(512)),
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
}
