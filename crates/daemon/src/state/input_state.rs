use super::*;

#[derive(Debug)]
pub(super) struct InputState {
    pub(super) control: InputControlState,
    pub(super) inject: InputInjectState,
}

#[derive(Debug)]
pub(super) struct InputControlState {
    pub(super) authorization: RwLock<InputAuthorizationState>,
    pub(super) sequence_by_peer: RwLock<HashMap<String, u64>>,
    pub(super) capture_target_peer_id: RwLock<Option<String>>,
    pub(super) lock_active: RwLock<bool>,
    pub(super) lock_supported: RwLock<bool>,
    pub(super) capture_backend_mode: RwLock<String>,
}

#[derive(Debug)]
pub(super) struct InputAuthorizationState {
    router: InputRouter,
    generation: u64,
    owner_last_changed_at: Option<Instant>,
    auto_claim_quarantined_peers: HashSet<String>,
    explicit_handoff_required: bool,
    broker_paused: bool,
}

impl InputAuthorizationState {
    fn new(input_enabled: bool, generation: u64) -> Self {
        Self {
            router: InputRouter::new(input_enabled),
            generation: generation.max(1),
            owner_last_changed_at: None,
            auto_claim_quarantined_peers: HashSet::new(),
            explicit_handoff_required: false,
            broker_paused: false,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
    }

    fn record_owner_transition(&mut self) {
        self.advance_generation();
        self.owner_last_changed_at = Some(Instant::now());
    }

    pub(super) fn allows_peer(&self, peer_id: &str) -> bool {
        !self.broker_paused && self.router.is_enabled() && self.router.owner() == Some(peer_id)
    }

    pub(super) fn authorizes_peer_generation(&self, peer_id: &str, generation: u64) -> bool {
        generation != 0 && generation == self.generation && self.allows_peer(peer_id)
    }

    pub(super) fn authorizes_held_generation(&self, generation: u64) -> bool {
        generation != 0
            && generation == self.generation
            && self.router.is_enabled()
            && !self.broker_paused
            && self.router.owner().is_some()
    }

    pub(super) fn owner(&self) -> Option<&str> {
        self.router.owner()
    }

    pub(super) fn route_frame<S: InputSink>(
        &mut self,
        frame: &InputFrame,
        sink: &mut S,
    ) -> Result<RouteDecision, core_input::InputRouteError> {
        if self.broker_paused {
            return Ok(RouteDecision::IgnoredFeatureDisabled);
        }
        self.router.route_frame(frame, sink)
    }

    pub(super) fn claim_owner(&mut self, peer_id: &str, force: bool) -> (bool, bool) {
        if self.broker_paused {
            return (false, false);
        }
        let previous_owner = self.router.owner().map(str::to_string);
        let claimed = self.router.claim_owner(peer_id, force);
        let owner_changed = claimed && previous_owner.as_deref() != self.router.owner();
        if owner_changed {
            self.record_owner_transition();
        }
        (claimed, owner_changed)
    }

    pub(super) fn claim_owner_explicit(&mut self, peer_id: &str, force: bool) -> (bool, bool) {
        let outcome = self.claim_owner(peer_id, force);
        if outcome.0 {
            self.explicit_handoff_required = false;
            self.auto_claim_quarantined_peers.remove(peer_id);
        }
        outcome
    }

    pub(super) fn auto_claim_quarantined(&self, peer_id: &str) -> bool {
        self.explicit_handoff_required || self.auto_claim_quarantined_peers.contains(peer_id)
    }

    /// A broker process died without a trustworthy side-effect receipt. Stop
    /// every automatic owner claim until a new explicit remote handoff.
    pub(super) fn require_explicit_handoff(&mut self) {
        if let Some(owner) = self.router.owner().map(str::to_string) {
            self.router.release_owner(&owner);
        }
        self.explicit_handoff_required = true;
        self.record_owner_transition();
    }

    pub(super) fn quarantine_auto_claim_peers(
        &mut self,
        peer_ids: impl IntoIterator<Item = String>,
    ) {
        self.auto_claim_quarantined_peers.extend(peer_ids);
    }

    pub(super) fn release_owner(&mut self, peer_id: &str) -> bool {
        let released = self.router.release_owner(peer_id);
        if released {
            self.record_owner_transition();
        }
        released
    }

    pub(super) fn clear_peer_state(&mut self, peer_id: &str) {
        self.router.clear_peer_state(peer_id);
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        self.router.set_enabled(enabled);
        self.advance_generation();
    }

    pub(super) fn set_broker_paused(&mut self, paused: bool) -> bool {
        if self.broker_paused == paused {
            return false;
        }
        self.broker_paused = paused;
        // Resume restores capability, never the previous remote owner's claim.
        self.require_explicit_handoff();
        true
    }

    pub(super) fn owner_last_changed_at(&self) -> Option<Instant> {
        self.owner_last_changed_at
    }

    #[cfg(test)]
    pub(super) fn set_owner_last_changed_at_for_test(&mut self, changed_at: Option<Instant>) {
        self.owner_last_changed_at = changed_at;
    }
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
                authorization: RwLock::new(InputAuthorizationState::new(input_enabled, 1)),
                sequence_by_peer: RwLock::new(HashMap::new()),
                capture_target_peer_id: RwLock::new(None),
                lock_active: RwLock::new(false),
                lock_supported: RwLock::new(cfg!(windows)),
                capture_backend_mode: RwLock::new("unknown".to_string()),
            },
            inject: InputInjectState {
                pending_inject_frames: RwLock::new(VecDeque::new()),
                pending_inject_high_water: std::sync::atomic::AtomicUsize::new(0),
            },
        }
    }

    pub(super) async fn reset(&self, input_enabled: bool) {
        {
            let mut authorization = self.control.authorization.write().await;
            let next_generation = authorization.generation().wrapping_add(1).max(1);
            *authorization = InputAuthorizationState::new(input_enabled, next_generation);
        }
        self.control.sequence_by_peer.write().await.clear();
        self.inject.pending_inject_frames.write().await.clear();
        *self.control.capture_target_peer_id.write().await = None;
        *self.control.lock_active.write().await = false;
        *self.control.capture_backend_mode.write().await = "unknown".to_string();
        self.inject
            .pending_inject_high_water
            .store(0, std::sync::atomic::Ordering::Release);
    }
}
