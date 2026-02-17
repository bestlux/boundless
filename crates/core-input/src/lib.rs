use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EasyMouseMode {
    Disable,
    Enable,
    Ctrl,
    Shift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwitchDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyState {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbsolute {
        x_norm: u16,
        y_norm: u16,
    },
    MouseButton {
        button: MouseButton,
        state: KeyState,
    },
    MouseWheel {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        scan_code: u16,
        state: KeyState,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFrame {
    pub source_peer_id: String,
    pub sequence: u64,
    pub timestamp_unix_ms: i64,
    pub events: Vec<InputEvent>,
}

pub const MAX_EVENTS_PER_FRAME: usize = 256;

#[derive(Debug, Error)]
pub enum InputFrameError {
    #[error("source_peer_id must not be empty")]
    EmptyPeer,
    #[error("input frame must include at least one event")]
    EmptyEvents,
    #[error("input frame event count exceeds limit: {count} > {limit}")]
    TooManyEvents { count: usize, limit: usize },
    #[error("input sequence regressed for peer {peer_id}: last={last} next={next}")]
    SequenceRegressed {
        peer_id: String,
        last: u64,
        next: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    Applied { event_count: usize },
    IgnoredFeatureDisabled,
    IgnoredNoOwner,
    IgnoredWrongOwner { owner_peer_id: String },
}

#[derive(Debug, Error)]
pub enum InputRouteError {
    #[error(transparent)]
    InvalidFrame(#[from] InputFrameError),
    #[error("sink apply failed at event index {index}: {message}")]
    SinkFailure { index: usize, message: String },
}

pub trait InputSink {
    fn apply(&mut self, event: &InputEvent) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct InputRouter {
    input_enabled: bool,
    owner_peer_id: Option<String>,
    last_sequence_by_peer: HashMap<String, u64>,
}

impl InputRouter {
    pub fn new(input_enabled: bool) -> Self {
        Self {
            input_enabled,
            owner_peer_id: None,
            last_sequence_by_peer: HashMap::new(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.input_enabled = enabled;
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner_peer_id.as_deref()
    }

    pub fn is_enabled(&self) -> bool {
        self.input_enabled
    }

    pub fn claim_owner(&mut self, peer_id: &str, force: bool) -> bool {
        if peer_id.trim().is_empty() {
            return false;
        }

        match self.owner_peer_id.as_deref() {
            None => {
                self.owner_peer_id = Some(peer_id.to_string());
                true
            }
            Some(current) if current == peer_id => true,
            Some(_) if force => {
                self.owner_peer_id = Some(peer_id.to_string());
                true
            }
            Some(_) => false,
        }
    }

    pub fn release_owner(&mut self, peer_id: &str) -> bool {
        if self.owner_peer_id.as_deref() == Some(peer_id) {
            self.owner_peer_id = None;
            return true;
        }

        false
    }

    pub fn clear_peer_state(&mut self, peer_id: &str) {
        self.last_sequence_by_peer.remove(peer_id);
    }

    pub fn validate_frame(&self, frame: &InputFrame) -> Result<(), InputFrameError> {
        if frame.source_peer_id.trim().is_empty() {
            return Err(InputFrameError::EmptyPeer);
        }

        if frame.events.is_empty() {
            return Err(InputFrameError::EmptyEvents);
        }

        if frame.events.len() > MAX_EVENTS_PER_FRAME {
            return Err(InputFrameError::TooManyEvents {
                count: frame.events.len(),
                limit: MAX_EVENTS_PER_FRAME,
            });
        }

        if let Some(last) = self.last_sequence_by_peer.get(&frame.source_peer_id)
            && frame.sequence <= *last
        {
            return Err(InputFrameError::SequenceRegressed {
                peer_id: frame.source_peer_id.clone(),
                last: *last,
                next: frame.sequence,
            });
        }

        Ok(())
    }

    pub fn route_frame<S: InputSink>(
        &mut self,
        frame: &InputFrame,
        sink: &mut S,
    ) -> Result<RouteDecision, InputRouteError> {
        self.validate_frame(frame)?;

        if !self.input_enabled {
            return Ok(RouteDecision::IgnoredFeatureDisabled);
        }

        let Some(owner_peer_id) = self.owner_peer_id.as_deref() else {
            return Ok(RouteDecision::IgnoredNoOwner);
        };

        if owner_peer_id != frame.source_peer_id {
            return Ok(RouteDecision::IgnoredWrongOwner {
                owner_peer_id: owner_peer_id.to_string(),
            });
        }

        for (index, event) in frame.events.iter().enumerate() {
            sink.apply(event)
                .map_err(|message| InputRouteError::SinkFailure { index, message })?;
        }

        self.last_sequence_by_peer
            .insert(frame.source_peer_id.clone(), frame.sequence);

        Ok(RouteDecision::Applied {
            event_count: frame.events.len(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSwitchRequest {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub wrap_mouse: bool,
    pub mode: EasyMouseMode,
    pub modifier_held: bool,
}

pub fn should_switch(req: EdgeSwitchRequest) -> Option<SwitchDirection> {
    let enabled = match req.mode {
        EasyMouseMode::Disable => false,
        EasyMouseMode::Enable => true,
        EasyMouseMode::Ctrl | EasyMouseMode::Shift => req.modifier_held,
    };

    if !enabled {
        return None;
    }

    if req.x <= 0 {
        return Some(SwitchDirection::Left);
    }

    if req.x >= req.width - 1 {
        return Some(SwitchDirection::Right);
    }

    if req.wrap_mouse {
        if req.y <= 0 {
            return Some(SwitchDirection::Up);
        }

        if req.y >= req.height - 1 {
            return Some(SwitchDirection::Down);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemorySink {
        events: Vec<InputEvent>,
        fail_on_index: Option<usize>,
    }

    impl InputSink for MemorySink {
        fn apply(&mut self, event: &InputEvent) -> Result<(), String> {
            if self.fail_on_index == Some(self.events.len()) {
                return Err("forced failure".to_string());
            }

            self.events.push(event.clone());
            Ok(())
        }
    }

    fn sample_frame(peer_id: &str, sequence: u64) -> InputFrame {
        InputFrame {
            source_peer_id: peer_id.to_string(),
            sequence,
            timestamp_unix_ms: 1234,
            events: vec![
                InputEvent::MouseMove { dx: 5, dy: -3 },
                InputEvent::Key {
                    scan_code: 30,
                    state: KeyState::Down,
                },
            ],
        }
    }

    #[test]
    fn blocks_when_easy_mouse_disabled() {
        assert!(
            should_switch(EdgeSwitchRequest {
                x: 1919,
                y: 50,
                width: 1920,
                height: 1080,
                wrap_mouse: true,
                mode: EasyMouseMode::Disable,
                modifier_held: true,
            })
            .is_none()
        );
    }

    #[test]
    fn owner_claim_requires_force_to_steal() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));
        assert!(!router.claim_owner("peer-b", false));
        assert_eq!(router.owner(), Some("peer-a"));
        assert!(router.claim_owner("peer-b", true));
        assert_eq!(router.owner(), Some("peer-b"));
    }

    #[test]
    fn route_requires_owner() {
        let mut router = InputRouter::new(true);
        let mut sink = MemorySink::default();
        let decision = router
            .route_frame(&sample_frame("peer-a", 1), &mut sink)
            .expect("route");
        assert_eq!(decision, RouteDecision::IgnoredNoOwner);
        assert!(sink.events.is_empty());
    }

    #[test]
    fn route_ignores_wrong_owner() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));

        let mut sink = MemorySink::default();
        let decision = router
            .route_frame(&sample_frame("peer-b", 1), &mut sink)
            .expect("route");

        assert_eq!(
            decision,
            RouteDecision::IgnoredWrongOwner {
                owner_peer_id: "peer-a".to_string()
            }
        );
        assert!(sink.events.is_empty());
    }

    #[test]
    fn route_applies_events_for_owner() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));

        let mut sink = MemorySink::default();
        let decision = router
            .route_frame(&sample_frame("peer-a", 1), &mut sink)
            .expect("route");
        assert_eq!(decision, RouteDecision::Applied { event_count: 2 });
        assert_eq!(sink.events.len(), 2);
    }

    #[test]
    fn route_rejects_sequence_regression() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));
        let mut sink = MemorySink::default();

        router
            .route_frame(&sample_frame("peer-a", 10), &mut sink)
            .expect("first frame");

        let err = router
            .route_frame(&sample_frame("peer-a", 9), &mut sink)
            .expect_err("must reject");
        assert!(matches!(
            err,
            InputRouteError::InvalidFrame(InputFrameError::SequenceRegressed { .. })
        ));
    }

    #[test]
    fn route_bubbles_sink_error() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));
        let mut sink = MemorySink {
            events: Vec::new(),
            fail_on_index: Some(1),
        };

        let err = router
            .route_frame(&sample_frame("peer-a", 2), &mut sink)
            .expect_err("must fail");
        assert!(matches!(err, InputRouteError::SinkFailure { index: 1, .. }));
    }

    #[test]
    fn clear_peer_state_allows_sequence_restart() {
        let mut router = InputRouter::new(true);
        assert!(router.claim_owner("peer-a", false));
        let mut sink = MemorySink::default();

        router
            .route_frame(&sample_frame("peer-a", 10), &mut sink)
            .expect("first frame");

        router.clear_peer_state("peer-a");

        let decision = router
            .route_frame(&sample_frame("peer-a", 1), &mut sink)
            .expect("sequence restart should pass after clear");
        assert_eq!(decision, RouteDecision::Applied { event_count: 2 });
    }
}
