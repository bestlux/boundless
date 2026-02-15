use serde::{Deserialize, Serialize};

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
}
