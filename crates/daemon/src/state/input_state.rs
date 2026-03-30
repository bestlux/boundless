use super::*;

#[derive(Debug)]
pub(super) struct InputState {
    pub(super) router: RwLock<InputRouter>,
    pub(super) sequence_by_peer: RwLock<HashMap<String, u64>>,
    pub(super) pending_inject_frames: RwLock<VecDeque<PendingInjectInputFrame>>,
    pub(super) capture_target_peer_id: RwLock<Option<String>>,
    pub(super) owner_last_changed_at: RwLock<Option<Instant>>,
    pub(super) lock_active: RwLock<bool>,
    pub(super) lock_supported: RwLock<bool>,
    pub(super) pending_inject_high_water: std::sync::atomic::AtomicUsize,
}

impl InputState {
    pub(super) fn new(input_enabled: bool) -> Self {
        Self {
            router: RwLock::new(InputRouter::new(input_enabled)),
            sequence_by_peer: RwLock::new(HashMap::new()),
            pending_inject_frames: RwLock::new(VecDeque::new()),
            capture_target_peer_id: RwLock::new(None),
            owner_last_changed_at: RwLock::new(None),
            lock_active: RwLock::new(false),
            lock_supported: RwLock::new(cfg!(windows)),
            pending_inject_high_water: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) async fn reset(&self, input_enabled: bool) {
        *self.router.write().await = InputRouter::new(input_enabled);
        self.sequence_by_peer.write().await.clear();
        self.pending_inject_frames.write().await.clear();
        *self.capture_target_peer_id.write().await = None;
        *self.owner_last_changed_at.write().await = None;
        *self.lock_active.write().await = false;
        self.pending_inject_high_water
            .store(0, std::sync::atomic::Ordering::Release);
    }
}
