use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AntiIdleRuntimeState {
    pub(crate) supported: bool,
    pub(crate) enabled: bool,
    pub(crate) active: bool,
    pub(crate) display_required: bool,
    pub(crate) battery_suppressed: bool,
    pub(crate) reason: AntiIdleAssertionReason,
    pub(crate) desired_execution_state_flags: u32,
}

impl Default for AntiIdleRuntimeState {
    fn default() -> Self {
        Self {
            supported: cfg!(windows),
            enabled: false,
            active: false,
            display_required: false,
            battery_suppressed: false,
            reason: AntiIdleAssertionReason::None,
            desired_execution_state_flags: 0,
        }
    }
}

#[derive(Debug)]
pub(super) struct AntiIdleState {
    pub(super) last_real_local_input_at: RwLock<Option<Instant>>,
    pub(super) remote_activity_until_by_peer: RwLock<HashMap<String, RemoteAntiIdleLease>>,
    pub(super) runtime: RwLock<AntiIdleRuntimeState>,
}

impl Default for AntiIdleState {
    fn default() -> Self {
        Self {
            last_real_local_input_at: RwLock::new(None),
            remote_activity_until_by_peer: RwLock::new(HashMap::new()),
            runtime: RwLock::new(AntiIdleRuntimeState::default()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RemoteAntiIdleLease {
    pub(super) until: Instant,
    pub(super) keep_display_on: bool,
}
