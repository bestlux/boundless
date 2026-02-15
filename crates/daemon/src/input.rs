use std::time::Duration;

use anyhow::Result;
use tokio::time;
use tracing::warn;

use core_input::InputEvent;

use crate::state::{AppState, PendingInjectInputFrame};

const INPUT_TICK: Duration = Duration::from_millis(5);

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "input runtime stopped");
        }
    });
}

async fn run(state: AppState) -> Result<()> {
    let mut backend = input_backend();
    let mut ticker = time::interval(INPUT_TICK);

    loop {
        ticker.tick().await;
        drain_pending_inject_frames(&state, backend.as_mut()).await;
    }
}

async fn drain_pending_inject_frames(state: &AppState, backend: &mut dyn InputBackend) {
    while let Some(frame) = state.dequeue_pending_inject_input_frame().await {
        if !state.input_injection_allowed_for_peer(&frame.peer_id).await {
            state
                .record_input_inject_skipped(
                    &frame.peer_id,
                    frame.sequence,
                    frame.events.len(),
                    "owner_or_feature_changed",
                )
                .await;
            continue;
        }

        match apply_frame(backend, &frame) {
            Ok(()) => {
                state
                    .record_input_inject_applied(&frame.peer_id, frame.sequence, frame.events.len())
                    .await;
            }
            Err(error) => {
                let message = format!("{error:#}");
                state
                    .record_input_inject_failed(
                        &frame.peer_id,
                        frame.sequence,
                        frame.events.len(),
                        &message,
                    )
                    .await;
                state.requeue_pending_inject_input_frame_front(frame).await;
                break;
            }
        }
    }
}

fn apply_frame(backend: &mut dyn InputBackend, frame: &PendingInjectInputFrame) -> Result<()> {
    for event in &frame.events {
        backend.apply(event)?;
    }
    Ok(())
}

trait InputBackend: Send {
    fn apply(&mut self, event: &InputEvent) -> Result<()>;
}

fn input_backend() -> Box<dyn InputBackend> {
    #[cfg(windows)]
    {
        Box::new(WindowsInputBackend::default())
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopInputBackend)
    }
}

#[cfg(not(windows))]
struct NoopInputBackend;

#[cfg(not(windows))]
impl InputBackend for NoopInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsInputBackend {
    warned_unimplemented: bool,
}

#[cfg(windows)]
impl InputBackend for WindowsInputBackend {
    fn apply(&mut self, _event: &InputEvent) -> Result<()> {
        if !self.warned_unimplemented {
            self.warned_unimplemented = true;
            warn!("windows input injection backend is still a no-op implementation");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    use super::*;
    #[cfg(not(windows))]
    use crate::state::AppState;
    #[cfg(not(windows))]
    use core_input::{InputFrame, KeyState};

    #[cfg(not(windows))]
    #[test]
    fn noop_backend_accepts_events() {
        let mut backend = NoopInputBackend;
        let frame = PendingInjectInputFrame {
            peer_id: "peer-a".to_string(),
            sequence: 1,
            events: vec![
                InputEvent::MouseMove { dx: 1, dy: -1 },
                InputEvent::Key {
                    scan_code: 30,
                    state: core_input::KeyState::Down,
                },
            ],
        };

        apply_frame(&mut backend, &frame).expect("noop backend should accept events");
    }

    #[cfg(not(windows))]
    struct CountingBackend {
        applied: usize,
    }

    #[cfg(not(windows))]
    impl InputBackend for CountingBackend {
        fn apply(&mut self, _event: &InputEvent) -> Result<()> {
            self.applied += 1;
            Ok(())
        }
    }

    #[cfg(not(windows))]
    async fn state_with_peer_for_input_test() -> (AppState, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "boundless-input-runtime-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");

        let state = AppState::load_or_create_with_paths(config_path, security_root).expect("state");
        let (code, _) = state.create_pairing_code(120).await;
        let peer_id = state
            .join_peer(
                code,
                "127.0.0.1:15100".to_string(),
                Some("peer".to_string()),
            )
            .await
            .expect("join");

        (state, peer_id, root)
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn drain_skips_frame_if_owner_changes_before_inject() {
        let (state, peer_id, root) = state_with_peer_for_input_test().await;
        assert!(
            state
                .claim_input_owner(&peer_id, false)
                .await
                .expect("claim owner")
        );

        state
            .route_incoming_input_frame(
                &peer_id,
                InputFrame {
                    source_peer_id: peer_id.clone(),
                    sequence: 1,
                    timestamp_unix_ms: 1,
                    events: vec![InputEvent::Key {
                        scan_code: 30,
                        state: KeyState::Down,
                    }],
                },
            )
            .await
            .expect("route");

        assert!(state.release_input_owner(&peer_id).await, "release owner");

        let mut backend = CountingBackend { applied: 0 };
        drain_pending_inject_frames(&state, &mut backend).await;
        assert_eq!(backend.applied, 0, "stale owner frame must not be injected");
        assert!(
            state.dequeue_pending_inject_input_frame().await.is_none(),
            "skipped stale frame should be dropped"
        );

        let events = state.transport_events().await;
        assert!(
            events
                .iter()
                .any(|event| event.kind == "input_inject_skipped" && event.peer_id == peer_id),
            "runtime should emit skipped event telemetry"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
