use super::*;
use anyhow::bail;

const ALLOWED_ANTI_IDLE_WINDOW_SECS: &[u32] = &[60, 300, 600, 900, 1_800];

impl AppState {
    pub async fn anti_idle_config(&self) -> AntiIdleConfig {
        self.config.read().await.anti_idle.clone()
    }

    pub(crate) async fn anti_idle_runtime_state(&self) -> AntiIdleRuntimeState {
        *self.anti_idle.runtime.read().await
    }

    pub async fn set_anti_idle_config_values(
        &self,
        enabled: bool,
        recent_activity_window_secs: u32,
        allow_on_battery: bool,
        keep_display_on: bool,
    ) -> Result<()> {
        validate_recent_activity_window_secs(recent_activity_window_secs)?;

        let mut config = self.config.write().await;
        config.anti_idle.enabled = enabled;
        config.anti_idle.recent_activity_window_secs = recent_activity_window_secs;
        config.anti_idle.allow_on_battery = allow_on_battery;
        config.anti_idle.keep_display_on = keep_display_on;
        save_config_at(&self.config_path, &config)?;
        drop(config);

        self.notify_anti_idle_wake("anti_idle_config_changed");
        Ok(())
    }

    pub async fn note_real_local_input_activity(&self) {
        *self.anti_idle.last_real_local_input_at.write().await = Some(Instant::now());
        self.notify_anti_idle_wake("real_local_input");
    }

    pub async fn anti_idle_outbound_pulse(&self) -> Option<AntiIdleOutboundPulse> {
        let config = self.anti_idle_config().await;
        if !config.enabled || !platform_windows::runtime::anti_idle_power_supported() {
            return None;
        }

        let now = Instant::now();
        let Some(last_activity) = *self.anti_idle.last_real_local_input_at.read().await else {
            return None;
        };
        if now.duration_since(last_activity)
            > Duration::from_secs(u64::from(config.recent_activity_window_secs))
        {
            return None;
        }
        if !config.allow_on_battery
            && !platform_windows::runtime::anti_idle_system_on_ac_power().unwrap_or(true)
        {
            return None;
        }

        Some(AntiIdleOutboundPulse {
            keep_display_on: config.keep_display_on,
            interval: Duration::from_secs(u64::from(config.pulse_interval_secs)),
        })
    }

    pub async fn note_remote_anti_idle_pulse(&self, peer_id: &str, keep_display_on: bool) {
        let config = self.anti_idle_config().await;
        let lease = Duration::from_secs(u64::from(config.pulse_interval_secs).saturating_mul(3));
        self.anti_idle
            .remote_activity_until_by_peer
            .write()
            .await
            .insert(
                peer_id.to_string(),
                anti_idle_state::RemoteAntiIdleLease {
                    until: Instant::now() + lease,
                    keep_display_on,
                },
            );
        self.record_transport_event(TransportEventRecord {
            timestamp: Utc::now(),
            direction: "incoming".to_string(),
            kind: "anti_idle_pulse_received".to_string(),
            peer_id: peer_id.to_string(),
            detail: format!(
                "keep_display_on={keep_display_on} lease_secs={}",
                lease.as_secs()
            ),
            size_bytes: 0,
        });
        self.notify_anti_idle_wake("anti_idle_pulse_received");
    }

    pub async fn clear_remote_anti_idle_peer(&self, peer_id: &str) -> bool {
        let removed = self
            .anti_idle
            .remote_activity_until_by_peer
            .write()
            .await
            .remove(peer_id)
            .is_some();
        if removed {
            self.notify_anti_idle_wake("anti_idle_peer_cleared");
        }
        removed
    }

    pub(crate) async fn reconcile_anti_idle_runtime(&self) -> AntiIdleRuntimeState {
        let config = self.anti_idle_config().await;
        let now = Instant::now();
        let supported = platform_windows::runtime::anti_idle_power_supported();

        let local_recent_input_active = self
            .anti_idle
            .last_real_local_input_at
            .read()
            .await
            .is_some_and(|last| {
                now.duration_since(last)
                    <= Duration::from_secs(u64::from(config.recent_activity_window_secs))
            });

        let (remote_recent_input_active, remote_display_required) = {
            let mut leases = self.anti_idle.remote_activity_until_by_peer.write().await;
            leases.retain(|_, lease| lease.until > now);
            let remote_recent_input_active = !leases.is_empty();
            let remote_display_required = leases.values().any(|lease| lease.keep_display_on);
            (remote_recent_input_active, remote_display_required)
        };

        let pending_reason = if local_recent_input_active {
            AntiIdleAssertionReason::LocalRecentInput
        } else if remote_recent_input_active {
            AntiIdleAssertionReason::RemoteRecentInput
        } else {
            AntiIdleAssertionReason::None
        };

        let on_ac_power = if supported && !config.allow_on_battery {
            platform_windows::runtime::anti_idle_system_on_ac_power().unwrap_or(true)
        } else {
            true
        };
        let battery_suppressed = supported
            && config.enabled
            && pending_reason != AntiIdleAssertionReason::None
            && !config.allow_on_battery
            && !on_ac_power;
        let active = supported
            && config.enabled
            && pending_reason != AntiIdleAssertionReason::None
            && !battery_suppressed;
        let display_required = active
            && ((local_recent_input_active && config.keep_display_on) || remote_display_required);
        let reason = if active {
            pending_reason
        } else {
            AntiIdleAssertionReason::None
        };
        let desired_execution_state_flags =
            platform_windows::runtime::anti_idle_execution_state_flags(active, display_required);
        let next = AntiIdleRuntimeState {
            supported,
            enabled: config.enabled,
            active,
            display_required,
            battery_suppressed,
            reason,
            desired_execution_state_flags,
        };

        let mut runtime = self.anti_idle.runtime.write().await;
        let previous = *runtime;
        if previous != next {
            self.record_transport_event(TransportEventRecord {
                timestamp: Utc::now(),
                direction: "local".to_string(),
                kind: "anti_idle_state_changed".to_string(),
                peer_id: "self".to_string(),
                detail: format!(
                    "supported={} enabled={} active={} display_required={} battery_suppressed={} reason={}",
                    next.supported,
                    next.enabled,
                    next.active,
                    next.display_required,
                    next.battery_suppressed,
                    next.reason.as_str()
                ),
                size_bytes: 0,
            });
            if !previous.active && next.active {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "anti_idle_assertion_acquired".to_string(),
                    peer_id: "self".to_string(),
                    detail: format!(
                        "display_required={} reason={}",
                        next.display_required,
                        next.reason.as_str()
                    ),
                    size_bytes: u64::from(next.desired_execution_state_flags),
                });
            }
            if previous.active && !next.active {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "anti_idle_assertion_released".to_string(),
                    peer_id: "self".to_string(),
                    detail: format!("previous_reason={}", previous.reason.as_str()),
                    size_bytes: u64::from(previous.desired_execution_state_flags),
                });
            }
            if !previous.battery_suppressed && next.battery_suppressed {
                self.record_transport_event(TransportEventRecord {
                    timestamp: Utc::now(),
                    direction: "local".to_string(),
                    kind: "anti_idle_skipped_on_battery".to_string(),
                    peer_id: "self".to_string(),
                    detail: format!("reason={}", pending_reason.as_str()),
                    size_bytes: 0,
                });
            }
            *runtime = next;
        }

        next
    }

    pub(crate) fn notify_anti_idle_wake(&self, source: &str) {
        if self.anti_idle_wake.trigger() {
            self.record_runtime_wake("anti_idle", source);
            self.anti_idle_wake.notify_one();
        }
    }

    pub(crate) fn anti_idle_wake_signal(&self) -> Arc<RuntimeWakeSignal> {
        self.anti_idle_wake.clone()
    }
}

fn validate_recent_activity_window_secs(value: u32) -> Result<()> {
    if ALLOWED_ANTI_IDLE_WINDOW_SECS.contains(&value) {
        return Ok(());
    }

    bail!(
        "recent activity window must be one of {} seconds",
        ALLOWED_ANTI_IDLE_WINDOW_SECS
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> (AppState, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("boundless-anti-idle-test-{}", uuid::Uuid::new_v4()));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        (state, root)
    }

    async fn test_state_with_peer() -> (AppState, String, std::path::PathBuf) {
        let (state, root) = test_state().await;
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join peer");
        (state, peer_id, root)
    }

    #[tokio::test]
    async fn anti_idle_setting_rejects_invalid_window_values() {
        let (state, root) = test_state().await;
        let err = state
            .set_anti_idle_config_values(true, 120, false, false)
            .await
            .expect_err("invalid window must be rejected");
        assert!(err.to_string().contains("recent activity window"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn real_local_input_opens_outbound_pulse_window() {
        let (state, root) = test_state().await;

        assert!(state.anti_idle_outbound_pulse().await.is_none());
        state.note_real_local_input_activity().await;

        let pulse = state
            .anti_idle_outbound_pulse()
            .await
            .expect("recent local input should produce pulse");
        assert_eq!(pulse.interval, Duration::from_secs(30));
        assert!(!pulse.keep_display_on);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remote_pulse_activates_remote_reason_and_disconnect_clears_it() {
        let (state, peer_id, root) = test_state_with_peer().await;
        state
            .set_peer_connected(&peer_id, true)
            .await
            .expect("connect peer");

        state.note_remote_anti_idle_pulse(&peer_id, true).await;
        let runtime = state.reconcile_anti_idle_runtime().await;
        assert_eq!(runtime.reason, AntiIdleAssertionReason::RemoteRecentInput);
        assert!(runtime.active || runtime.battery_suppressed || !runtime.supported);
        assert_eq!(runtime.display_required, runtime.active);

        state
            .set_peer_connected(&peer_id, false)
            .await
            .expect("disconnect peer");
        let runtime = state.reconcile_anti_idle_runtime().await;
        assert_eq!(runtime.reason, AntiIdleAssertionReason::None);
        assert!(!runtime.active);

        let _ = std::fs::remove_dir_all(root);
    }
}
