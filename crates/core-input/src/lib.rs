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

/// Source-side interpretation metadata for a physical keyboard event.
///
/// Windows can map the same non-extended keypad scan code to either a digit or
/// a navigation virtual key depending on Num Lock. Carrying the source virtual
/// key and effective toggle state lets the destination reproduce that intent
/// while retaining the physical scan/E0 identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySemantics {
    /// Only the physical scan/E0 identity is known (for example, a diagnostic
    /// command that supplies a scan code directly).
    #[default]
    Physical,
    Windows {
        virtual_key: u16,
        num_lock_on: bool,
    },
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
        semantics: KeySemantics,
    },
}

/// Exact process-local ledger of input that Boundless has successfully placed
/// in the down state on a destination Windows session.
///
/// Callers must observe only the committed prefix reported by the native
/// injector. Keeping this policy in `core-input` lets the ordinary tray
/// injector and the elevated injector synthesize the same deterministic
/// cleanup without duplicating held-state rules across integrity levels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldInputState {
    pressed_keys: Vec<(u16, KeySemantics)>,
    pressed_buttons: Vec<MouseButton>,
}

impl HeldInputState {
    pub fn observe(&mut self, events: &[InputEvent]) {
        for event in events {
            match event {
                InputEvent::Key {
                    scan_code,
                    state,
                    semantics,
                } => match state {
                    KeyState::Down => {
                        if !self
                            .pressed_keys
                            .iter()
                            .any(|(pressed_scan_code, _)| pressed_scan_code == scan_code)
                        {
                            self.pressed_keys.push((*scan_code, *semantics));
                        }
                    }
                    KeyState::Up => self
                        .pressed_keys
                        .retain(|(pressed_scan_code, _)| pressed_scan_code != scan_code),
                },
                InputEvent::MouseButton { button, state } => match state {
                    KeyState::Down => {
                        if !self.pressed_buttons.contains(button) {
                            self.pressed_buttons.push(*button);
                        }
                    }
                    KeyState::Up => self.pressed_buttons.retain(|pressed| pressed != button),
                },
                InputEvent::MouseMove { .. }
                | InputEvent::MouseMoveAbsolute { .. }
                | InputEvent::MouseWheel { .. } => {}
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pressed_keys.is_empty() && self.pressed_buttons.is_empty()
    }

    /// Replays the intended held state with keys before buttons so modifiers
    /// are restored before a drag or chord continues.
    pub fn held_down_events(&self) -> Vec<InputEvent> {
        let mut held = self
            .pressed_keys
            .iter()
            .map(|(scan_code, semantics)| InputEvent::Key {
                scan_code: *scan_code,
                state: KeyState::Down,
                semantics: *semantics,
            })
            .collect::<Vec<_>>();
        held.extend(
            self.pressed_buttons
                .iter()
                .map(|button| InputEvent::MouseButton {
                    button: *button,
                    state: KeyState::Down,
                }),
        );
        held
    }

    /// Synthesizes a deterministic fail-open cleanup: buttons first, then
    /// keys. Releasing buttons before modifier keys preserves the current tray
    /// broker's shutdown semantics.
    pub fn release_events(&self) -> Vec<InputEvent> {
        let mut pressed_buttons = self.pressed_buttons.clone();
        pressed_buttons.sort_by_key(|button| match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::X1 => 3,
            MouseButton::X2 => 4,
        });
        let mut pressed_keys = self.pressed_keys.clone();
        pressed_keys.sort_unstable_by_key(|(scan_code, _)| *scan_code);

        let mut releases = pressed_buttons
            .into_iter()
            .map(|button| InputEvent::MouseButton {
                button,
                state: KeyState::Up,
            })
            .collect::<Vec<_>>();
        releases.extend(
            pressed_keys
                .into_iter()
                .map(|(scan_code, semantics)| InputEvent::Key {
                    scan_code,
                    state: KeyState::Up,
                    semantics,
                }),
        );
        releases
    }

    pub fn clear(&mut self) {
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
    }
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
    pub block_screen_corners: bool,
    pub corner_block_px: u32,
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

    if req.x <= 0 && !edge_switch_corner_blocked(&req, SwitchDirection::Left) {
        return Some(SwitchDirection::Left);
    }

    if req.x >= req.width - 1 && !edge_switch_corner_blocked(&req, SwitchDirection::Right) {
        return Some(SwitchDirection::Right);
    }

    if req.wrap_mouse {
        if req.y <= 0 && !edge_switch_corner_blocked(&req, SwitchDirection::Up) {
            return Some(SwitchDirection::Up);
        }

        if req.y >= req.height - 1 && !edge_switch_corner_blocked(&req, SwitchDirection::Down) {
            return Some(SwitchDirection::Down);
        }
    }

    None
}

fn edge_switch_corner_blocked(req: &EdgeSwitchRequest, direction: SwitchDirection) -> bool {
    if !req.block_screen_corners || req.corner_block_px == 0 {
        return false;
    }

    let corner = req.corner_block_px as i32;
    let right = req.width.saturating_sub(1);
    let bottom = req.height.saturating_sub(1);
    let near_left = req.x <= corner;
    let near_right = req.x >= right.saturating_sub(corner);
    let near_top = req.y <= corner;
    let near_bottom = req.y >= bottom.saturating_sub(corner);

    match direction {
        SwitchDirection::Left | SwitchDirection::Right => near_top || near_bottom,
        SwitchDirection::Up | SwitchDirection::Down => near_left || near_right,
    }
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
                    semantics: KeySemantics::Physical,
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
                block_screen_corners: true,
                corner_block_px: 24,
                mode: EasyMouseMode::Disable,
                modifier_held: true,
            })
            .is_none()
        );
    }

    #[test]
    fn edge_switch_blocks_corners_when_enabled() {
        assert!(
            should_switch(EdgeSwitchRequest {
                x: 1919,
                y: 4,
                width: 1920,
                height: 1080,
                wrap_mouse: true,
                block_screen_corners: true,
                corner_block_px: 24,
                mode: EasyMouseMode::Enable,
                modifier_held: false,
            })
            .is_none()
        );
    }

    #[test]
    fn edge_switch_allows_corners_when_disabled() {
        assert_eq!(
            should_switch(EdgeSwitchRequest {
                x: 1919,
                y: 4,
                width: 1920,
                height: 1080,
                wrap_mouse: true,
                block_screen_corners: false,
                corner_block_px: 24,
                mode: EasyMouseMode::Enable,
                modifier_held: false,
            }),
            Some(SwitchDirection::Right)
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

    #[test]
    fn held_input_tracks_only_transitions_and_releases_deterministically() {
        let windows_semantics = KeySemantics::Windows {
            virtual_key: 0x61,
            num_lock_on: true,
        };
        let mut held = HeldInputState::default();
        held.observe(&[
            InputEvent::Key {
                scan_code: 0x4f,
                state: KeyState::Down,
                semantics: windows_semantics,
            },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                state: KeyState::Down,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
            InputEvent::Key {
                scan_code: 0x2a,
                state: KeyState::Down,
                semantics: KeySemantics::Physical,
            },
            InputEvent::Key {
                scan_code: 0x4f,
                state: KeyState::Down,
                semantics: KeySemantics::Physical,
            },
            InputEvent::MouseMove { dx: 3, dy: -2 },
        ]);

        assert_eq!(
            held.held_down_events(),
            vec![
                InputEvent::Key {
                    scan_code: 0x4f,
                    state: KeyState::Down,
                    semantics: windows_semantics,
                },
                InputEvent::Key {
                    scan_code: 0x2a,
                    state: KeyState::Down,
                    semantics: KeySemantics::Physical,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    state: KeyState::Down,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: KeyState::Down,
                },
            ]
        );
        assert_eq!(
            held.release_events(),
            vec![
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: KeyState::Up,
                },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    state: KeyState::Up,
                },
                InputEvent::Key {
                    scan_code: 0x2a,
                    state: KeyState::Up,
                    semantics: KeySemantics::Physical,
                },
                InputEvent::Key {
                    scan_code: 0x4f,
                    state: KeyState::Up,
                    semantics: windows_semantics,
                },
            ]
        );

        held.observe(&[
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Up,
            },
            InputEvent::Key {
                scan_code: 0x4f,
                state: KeyState::Up,
                semantics: KeySemantics::Physical,
            },
        ]);
        assert_eq!(held.held_down_events().len(), 2);
    }

    #[test]
    fn held_input_can_observe_an_exact_committed_prefix() {
        let events = [
            InputEvent::Key {
                scan_code: 0x1d,
                state: KeyState::Down,
                semantics: KeySemantics::Physical,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
        ];
        let mut held = HeldInputState::default();
        held.observe(&events[..1]);

        assert_eq!(held.held_down_events(), vec![events[0].clone()]);
        held.clear();
        assert!(held.is_empty());
    }
}
