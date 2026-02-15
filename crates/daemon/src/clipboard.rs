use std::time::Duration;

use anyhow::Result;
use tokio::time;
use tracing::{debug, info, warn};

use crate::state::{AppState, PendingRemoteClipboardText};

const CLIPBOARD_TICK: Duration = Duration::from_millis(200);

pub fn start(state: AppState) {
    tokio::spawn(async move {
        if let Err(error) = run(state).await {
            warn!(error = ?error, "clipboard runtime stopped");
        }
    });
}

async fn run(state: AppState) -> Result<()> {
    let mut backend = clipboard_backend();
    let mut ticker = time::interval(CLIPBOARD_TICK);

    loop {
        ticker.tick().await;

        drain_remote_queue(&state, backend.as_mut()).await;

        match backend.read_text() {
            Ok(Some(text)) => {
                if let Err(error) = state
                    .queue_local_clipboard_text_for_connected_peers(text)
                    .await
                {
                    warn!(error = ?error, "local clipboard sync failed");
                }
            }
            Ok(None) => {}
            Err(error) => {
                debug!(error = ?error, "clipboard read unavailable");
            }
        }
    }
}

async fn drain_remote_queue(state: &AppState, backend: &mut dyn ClipboardBackend) {
    while let Some(item) = state.dequeue_remote_clipboard_text().await {
        if let Err(error) = apply_remote_text(state, backend, item).await {
            warn!(error = ?error, "failed to apply remote clipboard text");
            break;
        }
    }
}

async fn apply_remote_text(
    state: &AppState,
    backend: &mut dyn ClipboardBackend,
    item: PendingRemoteClipboardText,
) -> Result<()> {
    match backend.write_text(&item.text) {
        Ok(()) => {
            state.mark_remote_clipboard_applied(&item.hash).await;
            info!(
                peer_id = %item.peer_id,
                size_bytes = item.text.len(),
                "applied remote clipboard text to local system clipboard"
            );
            Ok(())
        }
        Err(error) => {
            state.requeue_remote_clipboard_text_front(item).await;
            Err(error)
        }
    }
}

trait ClipboardBackend: Send {
    fn read_text(&mut self) -> Result<Option<String>>;
    fn write_text(&mut self, text: &str) -> Result<()>;
}

fn clipboard_backend() -> Box<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        Box::new(WindowsClipboardBackend)
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopClipboardBackend)
    }
}

#[cfg(windows)]
struct WindowsClipboardBackend;

#[cfg(windows)]
impl ClipboardBackend for WindowsClipboardBackend {
    fn read_text(&mut self) -> Result<Option<String>> {
        Ok(clipboard_win::get_clipboard_string().ok())
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        clipboard_win::set_clipboard_string(text)
            .map_err(|error| anyhow::anyhow!("clipboard write failed: {error}"))
    }
}

#[cfg(not(windows))]
struct NoopClipboardBackend;

#[cfg(not(windows))]
impl ClipboardBackend for NoopClipboardBackend {
    fn read_text(&mut self) -> Result<Option<String>> {
        Ok(None)
    }

    fn write_text(&mut self, _text: &str) -> Result<()> {
        Ok(())
    }
}
