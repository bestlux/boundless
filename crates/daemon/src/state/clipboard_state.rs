use std::collections::{HashMap, HashSet, VecDeque};

use tokio::sync::RwLock;

use core_clipboard::ClipboardPayload;

#[derive(Debug, Clone)]
pub struct PendingRemoteClipboardPayload {
    pub peer_id: String,
    pub payload: ClipboardPayload,
    pub hash: String,
    pub retry_count: u8,
}

#[derive(Debug, Clone)]
pub(super) struct ClipboardReplayState {
    pub(super) payload: ClipboardPayload,
    pub(super) hash: String,
    pub(super) source_peer_ids: HashSet<String>,
    pub(super) scheduled_peer_ids: HashSet<String>,
    pub(super) inflight_peer_ids: HashSet<String>,
}

#[derive(Debug, Default)]
pub(super) struct ClipboardSyncState {
    pub(super) last_observed_hash: Option<String>,
    pub(super) suppress_echo_hash: Option<String>,
    pub(super) pending_remote: VecDeque<PendingRemoteClipboardPayload>,
    pub(super) pending_replay: Option<ClipboardReplayState>,
    pub(super) obsolete_inflight_replay_hashes_by_peer: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Default)]
pub(super) struct ClipboardState {
    pub(super) sync: RwLock<ClipboardSyncState>,
}

impl ClipboardState {
    pub(super) async fn clear(&self) {
        *self.sync.write().await = ClipboardSyncState::default();
    }
}
