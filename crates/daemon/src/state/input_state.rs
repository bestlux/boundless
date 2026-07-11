use super::*;

#[derive(Debug)]
pub(super) struct InputState {
    pub(super) control: InputControlState,
    pub(super) inject: InputInjectState,
}

#[derive(Debug)]
pub(super) struct InputControlState {
    pub(super) router: RwLock<InputRouter>,
    pub(super) sequence_by_peer: RwLock<HashMap<String, u64>>,
    pub(super) capture_target_peer_id: RwLock<Option<String>>,
    pub(super) owner_last_changed_at: RwLock<Option<Instant>>,
    pub(super) lock_active: RwLock<bool>,
    pub(super) lock_supported: RwLock<bool>,
    pub(super) capture_backend_mode: RwLock<String>,
    pub(super) authorization_generation: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
pub(super) struct InputInjectState {
    pub(super) pending_inject_frames: RwLock<VecDeque<PendingInjectInputFrame>>,
    pub(super) pending_inject_high_water: std::sync::atomic::AtomicUsize,
}

impl InputState {
    pub(super) fn new(input_enabled: bool) -> Self {
        Self {
            control: InputControlState {
                router: RwLock::new(InputRouter::new(input_enabled)),
                sequence_by_peer: RwLock::new(HashMap::new()),
                capture_target_peer_id: RwLock::new(None),
                owner_last_changed_at: RwLock::new(None),
                lock_active: RwLock::new(false),
                lock_supported: RwLock::new(cfg!(windows)),
                capture_backend_mode: RwLock::new("unknown".to_string()),
                authorization_generation: std::sync::atomic::AtomicU64::new(1),
            },
            inject: InputInjectState {
                pending_inject_frames: RwLock::new(VecDeque::new()),
                pending_inject_high_water: std::sync::atomic::AtomicUsize::new(0),
            },
        }
    }

    pub(super) async fn reset(&self, input_enabled: bool) {
        *self.control.router.write().await = InputRouter::new(input_enabled);
        self.control.sequence_by_peer.write().await.clear();
        self.inject.pending_inject_frames.write().await.clear();
        *self.control.capture_target_peer_id.write().await = None;
        *self.control.owner_last_changed_at.write().await = None;
        *self.control.lock_active.write().await = false;
        *self.control.capture_backend_mode.write().await = "unknown".to_string();
        self.control
            .authorization_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.inject
            .pending_inject_high_water
            .store(0, std::sync::atomic::Ordering::Release);
    }
}
