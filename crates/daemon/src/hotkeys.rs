use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::time;
use tracing::{info, warn};

use crate::state::AppState;

const HOTKEY_TICK: Duration = Duration::from_millis(50);
const HOTKEY_RELOAD_EVERY_TICKS: usize = 20;

const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_ALT: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

#[cfg(windows)]
use windows_sys::Win32::{
    System::Shutdown::LockWorkStation, UI::Input::KeyboardAndMouse::GetAsyncKeyState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HotkeyAction {
    ToggleEasyMouse,
    LockMachine,
    SwitchAll,
    Reconnect,
}

impl HotkeyAction {
    fn from_config_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "toggle_easy_mouse" => Some(Self::ToggleEasyMouse),
            "lock_machine" => Some(Self::LockMachine),
            "switch_all" => Some(Self::SwitchAll),
            "reconnect" => Some(Self::Reconnect),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyCombo {
    key: u16,
    require_ctrl: bool,
    require_alt: bool,
    require_shift: bool,
    require_win: bool,
}

impl KeyCombo {
    fn is_active<F>(self, key_down: F) -> bool
    where
        F: Fn(u16) -> bool + Copy,
    {
        if self.require_ctrl && !key_down(VK_CONTROL) {
            return false;
        }
        if self.require_alt && !key_down(VK_ALT) {
            return false;
        }
        if self.require_shift && !key_down(VK_SHIFT) {
            return false;
        }
        if self.require_win && !(key_down(VK_LWIN) || key_down(VK_RWIN)) {
            return false;
        }

        key_down(self.key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HotkeyBinding {
    action: HotkeyAction,
    combo: KeyCombo,
}

#[derive(Debug, Default)]
struct HotkeyEngine {
    bindings: Vec<HotkeyBinding>,
    active_actions: HashSet<HotkeyAction>,
}

impl HotkeyEngine {
    fn update_bindings(&mut self, hotkeys: &BTreeMap<String, String>) {
        let mut bindings = Vec::<HotkeyBinding>::new();
        for (action_name, combo_spec) in hotkeys {
            let Some(action) = HotkeyAction::from_config_name(action_name) else {
                continue;
            };

            match parse_key_combo(combo_spec) {
                Ok(Some(combo)) => bindings.push(HotkeyBinding { action, combo }),
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        action = %action_name,
                        combo = %combo_spec,
                        error = %error,
                        "ignoring invalid hotkey combo"
                    );
                }
            }
        }

        let bound_actions = bindings
            .iter()
            .map(|binding| binding.action)
            .collect::<HashSet<_>>();
        self.bindings = bindings;
        self.active_actions
            .retain(|action| bound_actions.contains(action));
    }

    fn poll<F>(&mut self, key_down: F) -> Vec<HotkeyAction>
    where
        F: Fn(u16) -> bool + Copy,
    {
        let mut active_actions = HashSet::<HotkeyAction>::new();
        let mut fired = Vec::<HotkeyAction>::new();

        for binding in &self.bindings {
            if !binding.combo.is_active(key_down) {
                continue;
            }
            active_actions.insert(binding.action);
            if !self.active_actions.contains(&binding.action) {
                fired.push(binding.action);
            }
        }

        self.active_actions = active_actions;
        fired
    }
}

pub fn start(state: AppState) {
    tokio::spawn(async move {
        #[cfg(windows)]
        {
            if let Err(error) = run_windows_hotkeys(state).await {
                warn!(error = ?error, "hotkey runtime stopped");
            }
        }

        #[cfg(not(windows))]
        {
            let _ = state;
        }
    });
}

#[cfg(windows)]
async fn run_windows_hotkeys(state: AppState) -> Result<()> {
    let mut engine = HotkeyEngine::default();
    let mut ticker = time::interval(HOTKEY_TICK);
    let mut ticks_since_reload = HOTKEY_RELOAD_EVERY_TICKS;

    loop {
        ticker.tick().await;

        if ticks_since_reload >= HOTKEY_RELOAD_EVERY_TICKS {
            engine.update_bindings(&state.hotkey_map().await);
            ticks_since_reload = 0;
        } else {
            ticks_since_reload += 1;
        }

        for action in engine.poll(is_virtual_key_down) {
            if let Err(error) = apply_hotkey_action(&state, action).await {
                warn!(action = ?action, error = ?error, "hotkey action failed");
            }
        }
    }
}

async fn apply_hotkey_action(state: &AppState, action: HotkeyAction) -> Result<()> {
    match action {
        HotkeyAction::ToggleEasyMouse => {
            let enabled = state
                .feature_map()
                .await
                .get("easy_mouse")
                .copied()
                .unwrap_or(true);
            let next = !enabled;
            state
                .set_feature("easy_mouse".to_string(), next)
                .await
                .context("set easy_mouse feature")?;
            info!(enabled = next, "hotkey toggled easy_mouse");
        }
        HotkeyAction::LockMachine => {
            lock_machine().context("lock machine action")?;
            info!("hotkey lock_machine executed");
        }
        HotkeyAction::Reconnect => {
            let peers = state.list_peers().await;
            for peer in &peers {
                state.request_peer_reconnect(&peer.peer_id).await;
            }

            let peer_count = state
                .mark_all_peers_disconnected()
                .await
                .context("mark peers disconnected")?;
            info!(peer_count, "hotkey reconnect requested");
        }
        HotkeyAction::SwitchAll => {
            info!("hotkey switch_all is not implemented yet");
        }
    }

    Ok(())
}

#[cfg(windows)]
fn lock_machine() -> Result<()> {
    let ok = unsafe { LockWorkStation() };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("LockWorkStation");
    }
    Ok(())
}

#[cfg(not(windows))]
fn lock_machine() -> Result<()> {
    anyhow::bail!("lock_machine is only supported on Windows");
}

#[cfg(windows)]
fn is_virtual_key_down(vk: u16) -> bool {
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    (state as u16 & 0x8000) != 0
}

fn parse_key_combo(spec: &str) -> Result<Option<KeyCombo>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("disabled") {
        return Ok(None);
    }

    let mut require_ctrl = false;
    let mut require_alt = false;
    let mut require_shift = false;
    let mut require_win = false;
    let mut key: Option<u16> = None;

    for token in trimmed.split('+') {
        let token = token.trim();
        if token.is_empty() {
            anyhow::bail!("hotkey token must not be empty");
        }

        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                require_ctrl = true;
            }
            "alt" => {
                require_alt = true;
            }
            "shift" => {
                require_shift = true;
            }
            "win" | "windows" | "meta" => {
                require_win = true;
            }
            _ => {
                if key.is_some() {
                    anyhow::bail!("hotkey must include only one non-modifier key token");
                }
                key = Some(parse_primary_key_token(token)?);
            }
        }
    }

    let Some(key) = key else {
        anyhow::bail!("hotkey must include a non-modifier key token");
    };

    Ok(Some(KeyCombo {
        key,
        require_ctrl,
        require_alt,
        require_shift,
        require_win,
    }))
}

fn parse_primary_key_token(token: &str) -> Result<u16> {
    let upper = token.trim().to_ascii_uppercase();

    if upper.len() == 1 {
        let byte = upper.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Ok(u16::from(byte));
        }
    }

    if let Some(number) = upper.strip_prefix('F')
        && let Ok(index) = number.parse::<u8>()
        && (1..=24).contains(&index)
    {
        return Ok(0x70 + u16::from(index - 1));
    }

    match upper.as_str() {
        "SPACE" => Ok(0x20),
        "TAB" => Ok(0x09),
        "ENTER" | "RETURN" => Ok(0x0D),
        "ESC" | "ESCAPE" => Ok(0x1B),
        "BACKSPACE" => Ok(0x08),
        "DELETE" => Ok(0x2E),
        "INSERT" => Ok(0x2D),
        "HOME" => Ok(0x24),
        "END" => Ok(0x23),
        "PGUP" | "PAGEUP" => Ok(0x21),
        "PGDN" | "PAGEDOWN" => Ok(0x22),
        "LEFT" => Ok(0x25),
        "UP" => Ok(0x26),
        "RIGHT" => Ok(0x27),
        "DOWN" => Ok(0x28),
        _ => anyhow::bail!("unsupported hotkey key token '{token}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn temp_state_paths(prefix: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("boundless-{prefix}-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        (root, config_path, security_root)
    }

    #[test]
    fn parse_key_combo_handles_disabled() {
        assert!(parse_key_combo("Disabled").expect("parse").is_none());
        assert!(parse_key_combo(" ").expect("parse").is_none());
    }

    #[test]
    fn parse_key_combo_accepts_modifiers_and_key() {
        let parsed = parse_key_combo("Ctrl+Alt+Shift+R")
            .expect("parse")
            .expect("enabled");
        assert_eq!(parsed.key, u16::from(b'R'));
        assert!(parsed.require_ctrl);
        assert!(parsed.require_alt);
        assert!(parsed.require_shift);
        assert!(!parsed.require_win);
    }

    #[test]
    fn hotkey_engine_fires_on_rising_edge() {
        let mut map = BTreeMap::<String, String>::new();
        map.insert("toggle_easy_mouse".to_string(), "Ctrl+Alt+E".to_string());

        let mut engine = HotkeyEngine::default();
        engine.update_bindings(&map);

        let mut down = HashSet::<u16>::new();
        let poll =
            |engine: &mut HotkeyEngine, down: &HashSet<u16>| engine.poll(|key| down.contains(&key));

        assert!(poll(&mut engine, &down).is_empty());

        down.insert(VK_CONTROL);
        down.insert(VK_ALT);
        down.insert(u16::from(b'E'));
        assert_eq!(
            poll(&mut engine, &down),
            vec![HotkeyAction::ToggleEasyMouse]
        );
        assert!(poll(&mut engine, &down).is_empty());

        down.remove(&u16::from(b'E'));
        assert!(poll(&mut engine, &down).is_empty());
        down.insert(u16::from(b'E'));
        assert_eq!(
            poll(&mut engine, &down),
            vec![HotkeyAction::ToggleEasyMouse]
        );
    }

    #[test]
    fn hotkey_engine_reload_keeps_active_edges_pressed() {
        let mut map = BTreeMap::<String, String>::new();
        map.insert("toggle_easy_mouse".to_string(), "Ctrl+Alt+E".to_string());

        let mut engine = HotkeyEngine::default();
        engine.update_bindings(&map);

        let mut down = HashSet::<u16>::new();
        down.insert(VK_CONTROL);
        down.insert(VK_ALT);
        down.insert(u16::from(b'E'));

        assert_eq!(
            engine.poll(|key| down.contains(&key)),
            vec![HotkeyAction::ToggleEasyMouse]
        );

        engine.update_bindings(&map);
        assert!(
            engine.poll(|key| down.contains(&key)).is_empty(),
            "binding reload must not fire action again while key is still held"
        );
    }

    #[tokio::test]
    async fn hotkey_toggle_easy_mouse_updates_persisted_feature() {
        let (root, config_path, security_root) = temp_state_paths("hotkey-toggle-test");
        let state = AppState::load_or_create_with_paths(config_path.clone(), security_root)
            .expect("load state");

        let before = state
            .feature_map()
            .await
            .get("easy_mouse")
            .copied()
            .unwrap_or(true);
        apply_hotkey_action(&state, HotkeyAction::ToggleEasyMouse)
            .await
            .expect("toggle");
        let after = state
            .feature_map()
            .await
            .get("easy_mouse")
            .copied()
            .unwrap_or(true);
        assert_eq!(after, !before);

        let persisted = std::fs::read_to_string(&config_path).expect("read config");
        assert!(
            persisted.contains(&format!("\"easy_mouse\": {after}")),
            "config file should persist easy_mouse toggle"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn hotkey_reconnect_marks_connected_peers_disconnected() {
        let (root, config_path, security_root) = temp_state_paths("hotkey-reconnect-test");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");

        let (code_one, _) = state.create_pairing_code(120).await;
        let peer_one = state
            .join_peer(
                code_one,
                "127.0.0.1:15100".to_string(),
                Some("peer-one".to_string()),
            )
            .await
            .expect("join peer one");
        let (code_two, _) = state.create_pairing_code(120).await;
        let peer_two = state
            .join_peer(
                code_two,
                "127.0.0.1:15101".to_string(),
                Some("peer-two".to_string()),
            )
            .await
            .expect("join peer two");

        state
            .set_peer_connected(&peer_one, true)
            .await
            .expect("connect one");
        state
            .set_peer_connected(&peer_two, true)
            .await
            .expect("connect two");
        assert!(
            state
                .claim_input_owner(&peer_one, false)
                .await
                .expect("claim owner")
        );

        apply_hotkey_action(&state, HotkeyAction::Reconnect)
            .await
            .expect("reconnect");

        let peers = state.list_peers().await;
        assert!(
            peers.iter().all(|peer| !peer.connected),
            "reconnect action should reset peer connected state"
        );
        assert!(
            state.input_owner().await.is_none(),
            "reconnect action should release input ownership"
        );
        assert!(
            state.peer_reconnect_generation(&peer_one).await > 0,
            "reconnect action should request active session teardown for peer one"
        );
        assert!(
            state.peer_reconnect_generation(&peer_two).await > 0,
            "reconnect action should request active session teardown for peer two"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
