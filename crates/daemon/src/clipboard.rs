use std::time::Duration;

use anyhow::Result;
use tokio::time;
use tracing::{debug, info, warn};

use core_clipboard::ClipboardPayload;
#[cfg(windows)]
use platform_windows::clipboard_backend::WindowsClipboardBackend;

use crate::{
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::{AppState, PendingRemoteClipboardPayload},
};

const CLIPBOARD_TICK: Duration = Duration::from_millis(200);
const MAX_REMOTE_CLIPBOARD_APPLY_RETRIES: u8 = 3;

enum RemoteApplyOutcome {
    Applied,
    Requeued,
    Dropped,
}

trait ClipboardRuntimeState {
    async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload>;
    async fn requeue_remote_clipboard_payload_front(&self, item: PendingRemoteClipboardPayload);
    async fn mark_remote_clipboard_applied(
        &self,
        source_peer_id: &str,
        payload: &ClipboardPayload,
        hash: &str,
    );
    async fn queue_local_clipboard_payload_for_connected_peers(
        &self,
        payload: ClipboardPayload,
    ) -> Result<bool>;
}

impl ClipboardRuntimeState for AppState {
    async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload> {
        AppState::dequeue_remote_clipboard_payload(self).await
    }

    async fn requeue_remote_clipboard_payload_front(&self, item: PendingRemoteClipboardPayload) {
        AppState::requeue_remote_clipboard_payload_front(self, item).await;
    }

    async fn mark_remote_clipboard_applied(
        &self,
        source_peer_id: &str,
        payload: &ClipboardPayload,
        hash: &str,
    ) {
        AppState::mark_remote_clipboard_applied(self, source_peer_id, payload, hash).await;
    }

    async fn queue_local_clipboard_payload_for_connected_peers(
        &self,
        payload: ClipboardPayload,
    ) -> Result<bool> {
        match payload {
            ClipboardPayload::Text(text) => {
                self.queue_local_clipboard_text_for_connected_peers(text)
                    .await
            }
            ClipboardPayload::Image(image_bmp) => {
                self.queue_local_clipboard_image_for_connected_peers(image_bmp)
                    .await
            }
        }
    }
}

pub fn start(state: AppState) {
    let task_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "clipboard.runtime",
            RuntimeTaskOwner::Clipboard,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        async move {
            if let Err(error) = run(task_state).await {
                warn!(error = ?error, "clipboard runtime stopped");
            }
        },
    );
}

async fn run(state: AppState) -> Result<()> {
    let mut backend = clipboard_backend();
    let mut ticker = time::interval(CLIPBOARD_TICK);
    let mut last_sequence: Option<u64> = None;

    loop {
        ticker.tick().await;
        tick_clipboard_runtime(&state, backend.as_mut(), &mut last_sequence).await;
    }
}

async fn tick_clipboard_runtime<S: ClipboardRuntimeState>(
    state: &S,
    backend: &mut dyn ClipboardBackend,
    last_sequence: &mut Option<u64>,
) {
    drain_remote_queue(state, backend).await;
    poll_local_clipboard(state, backend, last_sequence).await;
}

async fn drain_remote_queue<S: ClipboardRuntimeState>(
    state: &S,
    backend: &mut dyn ClipboardBackend,
) {
    while let Some(item) = state.dequeue_remote_clipboard_payload().await {
        match apply_remote_payload(state, backend, item).await {
            RemoteApplyOutcome::Applied | RemoteApplyOutcome::Dropped => {}
            RemoteApplyOutcome::Requeued => break,
        }
    }
}

async fn poll_local_clipboard<S: ClipboardRuntimeState>(
    state: &S,
    backend: &mut dyn ClipboardBackend,
    last_sequence: &mut Option<u64>,
) {
    if let Some(sequence) = backend.sequence_number() {
        if *last_sequence == Some(sequence) {
            return;
        }
        *last_sequence = Some(sequence);
    }

    match backend.read_payload() {
        Ok(Some(payload)) => {
            if let Err(error) = state
                .queue_local_clipboard_payload_for_connected_peers(payload)
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

async fn apply_remote_payload<S: ClipboardRuntimeState>(
    state: &S,
    backend: &mut dyn ClipboardBackend,
    mut item: PendingRemoteClipboardPayload,
) -> RemoteApplyOutcome {
    let payload_kind = clipboard_payload_kind(&item.payload);
    let payload_size = clipboard_payload_size(&item.payload);

    match backend.write_payload(&item.payload) {
        Ok(()) => {
            state
                .mark_remote_clipboard_applied(&item.peer_id, &item.payload, &item.hash)
                .await;
            info!(
                peer_id = %item.peer_id,
                payload_kind,
                size_bytes = payload_size,
                "applied remote clipboard payload to local system clipboard"
            );
            RemoteApplyOutcome::Applied
        }
        Err(error) => {
            item.retry_count = item.retry_count.saturating_add(1);
            if item.retry_count > MAX_REMOTE_CLIPBOARD_APPLY_RETRIES {
                warn!(
                    peer_id = %item.peer_id,
                    payload_kind,
                    size_bytes = payload_size,
                    retry_count = item.retry_count,
                    error = ?error,
                    "dropping remote clipboard payload after bounded retries"
                );
                return RemoteApplyOutcome::Dropped;
            }
            warn!(
                peer_id = %item.peer_id,
                payload_kind,
                size_bytes = payload_size,
                retry_count = item.retry_count,
                error = ?error,
                "failed to apply remote clipboard payload; requeueing for retry"
            );
            state.requeue_remote_clipboard_payload_front(item).await;
            RemoteApplyOutcome::Requeued
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
        Box::new(WindowsClipboardBackendAdapter(WindowsClipboardBackend))
    }

    #[cfg(not(windows))]
    {
        Box::new(NoopClipboardBackend)
    }
}

#[cfg(not(windows))]
struct NoopClipboardBackend;

#[cfg(windows)]
struct WindowsClipboardBackendAdapter(WindowsClipboardBackend);

#[cfg(windows)]
impl ClipboardBackend for WindowsClipboardBackendAdapter {
    fn sequence_number(&mut self) -> Option<u64> {
        self.0.sequence_number()
    }

    fn read_payload(&mut self) -> Result<Option<ClipboardPayload>> {
        self.0.read_payload()
    }

    fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
        self.0.write_payload(payload)
    }
}

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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use anyhow::anyhow;

    use super::*;

    #[derive(Default)]
    struct FakeRuntimeState {
        log: Arc<Mutex<Vec<String>>>,
        remote_queue: Mutex<VecDeque<PendingRemoteClipboardPayload>>,
        remote_applied_hashes: Mutex<Vec<String>>,
        local_payloads: Mutex<Vec<ClipboardPayload>>,
    }

    impl FakeRuntimeState {
        fn with_log(log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                log,
                ..Self::default()
            }
        }

        fn push_remote(&self, item: PendingRemoteClipboardPayload) {
            self.remote_queue
                .lock()
                .expect("remote queue")
                .push_back(item);
        }

        fn remote_queue_snapshot(&self) -> Vec<PendingRemoteClipboardPayload> {
            self.remote_queue
                .lock()
                .expect("remote queue")
                .iter()
                .cloned()
                .collect()
        }

        fn remote_applied_hashes(&self) -> Vec<String> {
            self.remote_applied_hashes
                .lock()
                .expect("applied hashes")
                .clone()
        }
    }

    impl ClipboardRuntimeState for FakeRuntimeState {
        async fn dequeue_remote_clipboard_payload(&self) -> Option<PendingRemoteClipboardPayload> {
            self.log
                .lock()
                .expect("log")
                .push("state.dequeue_remote".to_string());
            self.remote_queue.lock().expect("remote queue").pop_front()
        }

        async fn requeue_remote_clipboard_payload_front(
            &self,
            item: PendingRemoteClipboardPayload,
        ) {
            self.log
                .lock()
                .expect("log")
                .push("state.requeue_remote".to_string());
            self.remote_queue
                .lock()
                .expect("remote queue")
                .push_front(item);
        }

        async fn mark_remote_clipboard_applied(
            &self,
            _source_peer_id: &str,
            _payload: &ClipboardPayload,
            hash: &str,
        ) {
            self.log
                .lock()
                .expect("log")
                .push(format!("state.mark_remote:{hash}"));
            self.remote_applied_hashes
                .lock()
                .expect("applied hashes")
                .push(hash.to_string());
        }

        async fn queue_local_clipboard_payload_for_connected_peers(
            &self,
            payload: ClipboardPayload,
        ) -> Result<bool> {
            self.log.lock().expect("log").push(format!(
                "state.queue_local:{}",
                clipboard_payload_kind(&payload)
            ));
            self.local_payloads
                .lock()
                .expect("local payloads")
                .push(payload);
            Ok(true)
        }
    }

    struct FakeClipboardBackend {
        log: Arc<Mutex<Vec<String>>>,
        sequence_values: VecDeque<Option<u64>>,
        sequence_fallback: Option<u64>,
        read_results: VecDeque<Result<Option<ClipboardPayload>>>,
        write_results: VecDeque<Result<()>>,
        writes: Vec<ClipboardPayload>,
        read_calls: usize,
    }

    impl FakeClipboardBackend {
        fn new(log: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                log,
                sequence_values: VecDeque::new(),
                sequence_fallback: None,
                read_results: VecDeque::new(),
                write_results: VecDeque::new(),
                writes: Vec::new(),
                read_calls: 0,
            }
        }

        fn with_sequence_values<I>(mut self, values: I) -> Self
        where
            I: IntoIterator<Item = Option<u64>>,
        {
            self.sequence_values.extend(values);
            self
        }

        fn with_read_results<I>(mut self, values: I) -> Self
        where
            I: IntoIterator<Item = Result<Option<ClipboardPayload>>>,
        {
            self.read_results.extend(values);
            self
        }

        fn with_write_results<I>(mut self, values: I) -> Self
        where
            I: IntoIterator<Item = Result<()>>,
        {
            self.write_results.extend(values);
            self
        }
    }

    impl ClipboardBackend for FakeClipboardBackend {
        fn sequence_number(&mut self) -> Option<u64> {
            self.log
                .lock()
                .expect("log")
                .push("backend.sequence".to_string());
            self.sequence_values
                .pop_front()
                .unwrap_or(self.sequence_fallback)
        }

        fn read_payload(&mut self) -> Result<Option<ClipboardPayload>> {
            self.read_calls += 1;
            self.log
                .lock()
                .expect("log")
                .push("backend.read".to_string());
            self.read_results.pop_front().unwrap_or(Ok(None))
        }

        fn write_payload(&mut self, payload: &ClipboardPayload) -> Result<()> {
            self.log
                .lock()
                .expect("log")
                .push(format!("backend.write:{}", clipboard_payload_kind(payload)));
            self.writes.push(payload.clone());
            self.write_results.pop_front().unwrap_or(Ok(()))
        }
    }

    #[tokio::test]
    async fn clipboard_tick_requeues_remote_apply_failures_and_retries_next_tick() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = FakeRuntimeState::with_log(log.clone());
        let remote = PendingRemoteClipboardPayload {
            peer_id: "peer-a".to_string(),
            payload: ClipboardPayload::Text("remote".to_string()),
            hash: "hash-1".to_string(),
            retry_count: 0,
        };
        state.push_remote(remote.clone());
        let mut backend = FakeClipboardBackend::new(log)
            .with_write_results([Err(anyhow!("clipboard busy")), Ok(())]);
        let mut last_sequence = None;

        tick_clipboard_runtime(&state, &mut backend, &mut last_sequence).await;

        assert_eq!(backend.writes, vec![remote.payload.clone()]);
        let queued = state.remote_queue_snapshot();
        assert_eq!(queued.len(), 1, "failed remote apply should remain queued");
        assert_eq!(
            queued[0].peer_id, remote.peer_id,
            "failed remote apply should be requeued to the front"
        );
        assert_eq!(
            queued[0].hash, remote.hash,
            "failed remote apply should preserve the pending item"
        );
        assert!(
            state.remote_applied_hashes().is_empty(),
            "failed remote apply must not be marked applied"
        );

        tick_clipboard_runtime(&state, &mut backend, &mut last_sequence).await;

        assert_eq!(
            backend.writes,
            vec![remote.payload.clone(), remote.payload.clone()]
        );
        assert!(
            state.remote_queue_snapshot().is_empty(),
            "successful retry should drain the remote queue"
        );
        assert_eq!(state.remote_applied_hashes(), vec![remote.hash]);
    }

    #[tokio::test]
    async fn clipboard_tick_processes_remote_then_local_poll() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = FakeRuntimeState::with_log(log.clone());
        state.push_remote(PendingRemoteClipboardPayload {
            peer_id: "peer-a".to_string(),
            payload: ClipboardPayload::Text("remote".to_string()),
            hash: "hash-remote".to_string(),
            retry_count: 0,
        });
        let mut backend = FakeClipboardBackend::new(log.clone())
            .with_sequence_values([Some(10)])
            .with_read_results([Ok(Some(ClipboardPayload::Text("local".to_string())))]);
        let mut last_sequence = Some(8);

        tick_clipboard_runtime(&state, &mut backend, &mut last_sequence).await;

        let log = log.lock().expect("log").clone();
        let remote_write = log
            .iter()
            .position(|entry| entry == "backend.write:text")
            .expect("remote write logged");
        let local_queue = log
            .iter()
            .position(|entry| entry == "state.queue_local:text")
            .expect("local queue logged");

        assert!(
            remote_write < local_queue,
            "remote queue should be processed before local polling"
        );
    }

    #[tokio::test]
    async fn clipboard_tick_drops_remote_after_bounded_retries_and_allows_next_item() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = FakeRuntimeState::with_log(log);
        let stuck = PendingRemoteClipboardPayload {
            peer_id: "peer-a".to_string(),
            payload: ClipboardPayload::Text("stuck".to_string()),
            hash: "hash-stuck".to_string(),
            retry_count: 0,
        };
        let next = PendingRemoteClipboardPayload {
            peer_id: "peer-b".to_string(),
            payload: ClipboardPayload::Text("next".to_string()),
            hash: "hash-next".to_string(),
            retry_count: 0,
        };
        state.push_remote(stuck.clone());
        state.push_remote(next.clone());

        let mut backend = FakeClipboardBackend::new(Arc::new(Mutex::new(Vec::new())))
            .with_write_results([
                Err(anyhow!("clipboard busy")),
                Err(anyhow!("clipboard busy")),
                Err(anyhow!("clipboard busy")),
                Err(anyhow!("clipboard busy")),
                Ok(()),
            ]);
        let mut last_sequence = None;

        for _ in 0..MAX_REMOTE_CLIPBOARD_APPLY_RETRIES {
            tick_clipboard_runtime(&state, &mut backend, &mut last_sequence).await;
        }

        let queued_before_drop = state.remote_queue_snapshot();
        assert_eq!(
            queued_before_drop.len(),
            2,
            "bounded retries should keep the stuck item queued until the retry budget is exhausted"
        );
        assert_eq!(
            queued_before_drop[0].retry_count, MAX_REMOTE_CLIPBOARD_APPLY_RETRIES,
            "retry count should advance each failed tick"
        );

        tick_clipboard_runtime(&state, &mut backend, &mut last_sequence).await;

        assert!(
            state.remote_queue_snapshot().is_empty(),
            "dropping the exhausted item should let the next queued remote payload apply in the same tick"
        );
        assert_eq!(
            state.remote_applied_hashes(),
            vec![next.hash],
            "only the later payload should remain after the exhausted head item is dropped"
        );
    }
}
