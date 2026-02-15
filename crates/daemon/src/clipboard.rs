use std::time::Duration;

use anyhow::Result;
use tokio::time;
use tracing::{debug, info, warn};

use core_clipboard::ClipboardPayload;

use crate::state::{AppState, PendingRemoteClipboardPayload};

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
    let mut last_sequence: Option<u64> = None;

    loop {
        ticker.tick().await;

        drain_remote_queue(&state, backend.as_mut()).await;

        if let Some(sequence) = backend.sequence_number() {
            if last_sequence == Some(sequence) {
                continue;
            }
            last_sequence = Some(sequence);
        }

        match backend.read_payload() {
            Ok(Some(ClipboardPayload::Text(text))) => {
                if let Err(error) = state
                    .queue_local_clipboard_text_for_connected_peers(text)
                    .await
                {
                    warn!(error = ?error, "local clipboard sync failed");
                }
            }
            Ok(Some(ClipboardPayload::Image(image_bmp))) => {
                if let Err(error) = state
                    .queue_local_clipboard_image_for_connected_peers(image_bmp)
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
    while let Some(item) = state.dequeue_remote_clipboard_payload().await {
        if let Err(error) = apply_remote_payload(state, backend, item).await {
            warn!(error = ?error, "failed to apply remote clipboard payload");
            break;
        }
    }
}

async fn apply_remote_payload(
    state: &AppState,
    backend: &mut dyn ClipboardBackend,
    item: PendingRemoteClipboardPayload,
) -> Result<()> {
    let payload_kind = clipboard_payload_kind(&item.payload);
    let payload_size = clipboard_payload_size(&item.payload);

    match backend.write_payload(&item.payload) {
        Ok(()) => {
            state.mark_remote_clipboard_applied(&item.hash).await;
            info!(
                peer_id = %item.peer_id,
                payload_kind,
                size_bytes = payload_size,
                "applied remote clipboard payload to local system clipboard"
            );
            Ok(())
        }
        Err(error) => {
            state.requeue_remote_clipboard_payload_front(item).await;
            Err(error)
        }
    }
}

trait ClipboardBackend: Send {
    fn sequence_number(&mut self) -> Option<u64>;
    fn read_payload(&mut self) -> Result<Option<ClipboardPayload>>;
    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()>;
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
    fn sequence_number(&mut self) -> Option<u64> {
        clipboard_win::seq_num().map(|num| num.get() as u64)
    }

    fn read_payload(&mut self) -> Result<Option<ClipboardPayload>> {
        use clipboard_win::{Format, formats};

        if formats::Bitmap.is_format_avail() {
            let image_bmp: Vec<u8> = clipboard_win::get_clipboard(formats::Bitmap)
                .map_err(|error| anyhow::anyhow!("clipboard image read failed: {error}"))?;
            return Ok(Some(ClipboardPayload::Image(image_bmp)));
        }

        Ok(clipboard_win::get_clipboard_string()
            .ok()
            .map(ClipboardPayload::Text))
    }

    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
        use clipboard_win::formats;

        match payload {
            ClipboardPayload::Text(text) => clipboard_win::set_clipboard_string(text)
                .map_err(|error| anyhow::anyhow!("clipboard text write failed: {error}")),
            ClipboardPayload::Image(image_bmp) => {
                clipboard_win::set_clipboard(formats::Bitmap, image_bmp.as_slice())
                    .map_err(|error| anyhow::anyhow!("clipboard image write failed: {error}"))
            }
        }
    }
}

#[cfg(not(windows))]
struct NoopClipboardBackend;

#[cfg(not(windows))]
impl ClipboardBackend for NoopClipboardBackend {
    fn sequence_number(&mut self) -> Option<u64> {
        None
    }

    fn read_payload(&mut self) -> Result<Option<ClipboardPayload>> {
        Ok(None)
    }

    fn write_payload(&mut self, _payload: &ClipboardPayload) -> Result<()> {
        Ok(())
    }
}

fn clipboard_payload_kind(payload: &ClipboardPayload) -> &'static str {
    match payload {
        ClipboardPayload::Text(_) => "text",
        ClipboardPayload::Image(_) => "image",
    }
}

fn clipboard_payload_size(payload: &ClipboardPayload) -> usize {
    match payload {
        ClipboardPayload::Text(text) => text.len(),
        ClipboardPayload::Image(bytes) => bytes.len(),
    }
}
