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
}
