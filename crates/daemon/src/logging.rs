mod bounded;

use std::{
    fmt as stdfmt,
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

use anyhow::Result;
use bounded::{Counters, DiskWriter, MAX_RECORD_BYTES, OVERSIZED_RECORD, Policy, QUEUE_RECORDS};
use tracing_subscriber::{
    EnvFilter, fmt, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Requests a bounded drain on drop. A blocked filesystem never delays shutdown
/// for more than one second and never blocks log producers.
pub struct LoggingGuard {
    shutdown: Arc<AtomicBool>,
    finished: Receiver<()>,
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.finished.recv_timeout(Duration::from_secs(1));
    }
}

#[derive(Clone)]
struct RecordSink {
    sender: Option<SyncSender<Vec<u8>>>,
    counters: Arc<Counters>,
}

impl RecordSink {
    fn start(directory: PathBuf, base: &'static str, policy: Policy) -> (Self, LoggingGuard) {
        let counters = Arc::new(Counters::default());
        let mut writer = DiskWriter::new(directory, base, policy, counters.clone());
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE_RECORDS);
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("{base}-writer"))
            .spawn(move || {
                loop {
                    if worker_shutdown.load(Ordering::Acquire) {
                        for _ in 0..QUEUE_RECORDS {
                            let Ok(record) = receiver.try_recv() else {
                                break;
                            };
                            let _ = writer.write_all(&record);
                        }
                        break;
                    }
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(record) => {
                            let _ = writer.write_all(&record);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => writer.maintenance(),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                drop(writer);
                let _ = finished_tx.try_send(());
            });
        let sender = match spawn {
            Ok(_) => Some(sender),
            Err(_) => {
                counters.io_failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        };
        (
            Self { sender, counters },
            LoggingGuard { shutdown, finished },
        )
    }

    fn record(&self) -> RecordWriter {
        RecordWriter {
            sink: self.clone(),
            bytes: Vec::new(),
            oversized: false,
        }
    }

    fn submit(&self, bytes: Vec<u8>) {
        // The record limit is enforced before queue allocation: 256 x 16 KiB
        // gives at most 4 MiB queued payload plus one worker record.
        debug_assert!(bytes.len() <= MAX_RECORD_BYTES);
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.try_send(bytes).is_err())
        {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct RecordWriter {
    sink: RecordSink,
    bytes: Vec<u8>,
    oversized: bool,
}

impl Write for RecordWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.oversized {
            if bytes.len() > MAX_RECORD_BYTES - self.bytes.len() {
                self.oversized = true;
                self.bytes.clear();
            } else {
                self.bytes.extend_from_slice(bytes);
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RecordWriter {
    fn drop(&mut self) {
        if self.oversized {
            self.sink.counters.oversized.fetch_add(1, Ordering::Relaxed);
            self.sink.submit(OVERSIZED_RECORD.to_vec());
        } else if !self.bytes.is_empty() {
            self.sink.submit(std::mem::take(&mut self.bytes));
        }
    }
}

impl<'a> MakeWriter<'a> for RecordSink {
    type Writer = RecordWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.record()
    }
}

/// Per directory/security context, runtime logs retain at most 100 MiB (10 x
/// 10 MiB), with 14-day retention. There is no shared budget across user profiles.
pub fn init_logging() -> Result<LoggingGuard> {
    let (sink, guard) = RecordSink::start(log_dir(), "boundlessd.log", Policy::RUNTIME);
    let _ = RUNTIME_COUNTERS.set(sink.counters.clone());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(sink)
                .json(),
        )
        .try_init()?;
    // The former synchronous stdout mirror could block runtime tasks or create
    // an unlimited redirected log. Diagnostics now use only the bounded sink.
    Ok(guard)
}

static STARTUP_SINK: OnceLock<RecordSink> = OnceLock::new();
static RUNTIME_COUNTERS: OnceLock<Arc<Counters>> = OnceLock::new();

/// Redacted counters and budgets for the existing control-plane diagnostics
/// bundle. This never inspects the filesystem or waits for a logging worker.
/// Counters reset per process; rejected queue records and storage failures count
/// as dropped, while oversized replacements count separately from written records.
pub fn logging_health() -> serde_json::Value {
    fn stream(counters: Option<&Arc<Counters>>, policy: Policy) -> serde_json::Value {
        serde_json::json!({
            "initialized": counters.is_some(),
            "file_logging_ready": counters.is_some_and(|c| c.disk_ready.load(Ordering::Acquire)),
            "written_records": counters.map(|c| c.written.load(Ordering::Relaxed)).unwrap_or(0),
            "dropped_records": counters.map(|c| c.dropped.load(Ordering::Relaxed)).unwrap_or(0),
            "oversized_records": counters.map(|c| c.oversized.load(Ordering::Relaxed)).unwrap_or(0),
            "io_failures": counters.map(|c| c.io_failures.load(Ordering::Relaxed)).unwrap_or(0),
            "segment_bytes": policy.segment_bytes,
            "retained_files": policy.files,
            "total_payload_bytes": policy.segment_bytes * policy.files as u64,
            "retention_days": policy.retention.as_secs() / (24 * 60 * 60),
            "record_bytes": MAX_RECORD_BYTES,
            "queue_records": QUEUE_RECORDS,
        })
    }
    serde_json::json!({
        "runtime": stream(RUNTIME_COUNTERS.get(), Policy::RUNTIME),
        "service_startup": stream(STARTUP_SINK.get().map(|sink| &sink.counters), Policy::STARTUP),
    })
}

/// Independent startup diagnostics retain at most 4 MiB (4 x 1 MiB) for 14
/// days. Initialize before the service panic hook and the normal subscriber.
pub fn init_service_startup_logging() -> LoggingGuard {
    let (sink, guard) = RecordSink::start(
        service_log_dir(),
        "boundless-service-startup.log",
        Policy::STARTUP,
    );
    let _ = STARTUP_SINK.set(sink);
    guard
}

pub fn append_service_startup_diagnostic(stage: &str, detail: &str) {
    let Some(sink) = STARTUP_SINK.get() else {
        return;
    };
    let mut record = sink.record();
    let _ = writeln!(
        record,
        "{} stage={} pid={} detail={}",
        chrono::Utc::now().to_rfc3339(),
        Sanitized(stage),
        std::process::id(),
        Sanitized(detail)
    );
}

struct Sanitized<'a>(&'a str);

impl stdfmt::Display for Sanitized<'_> {
    fn fmt(&self, formatter: &mut stdfmt::Formatter<'_>) -> stdfmt::Result {
        for (index, character) in self.0.char_indices() {
            if index >= MAX_RECORD_BYTES / 2 {
                return formatter.write_str(" [truncated]");
            }
            match character {
                '\r' | '\n' => formatter.write_str(" ")?,
                other => stdfmt::Write::write_char(formatter, other)?,
            }
        }
        Ok(())
    }
}

fn log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("logs")
}

fn service_log_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Boundless")
        .join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cap_is_applied_before_queue_and_fragmented_records_remain_bounded() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let counters = Arc::new(Counters::default());
        let sink = RecordSink {
            sender: Some(sender),
            counters: counters.clone(),
        };
        {
            let mut record = sink.record();
            record
                .write_all(&[b'x'; MAX_RECORD_BYTES / 2])
                .expect("first fragment");
            record
                .write_all(&[b'x'; MAX_RECORD_BYTES])
                .expect("oversized fragment");
            assert!(record.bytes.capacity() <= MAX_RECORD_BYTES);
            record
                .write_all(&vec![b'x'; MAX_RECORD_BYTES * 10])
                .expect("discard remainder");
            assert!(record.bytes.capacity() <= MAX_RECORD_BYTES);
        }
        assert_eq!(
            receiver.recv().expect("one bounded record"),
            OVERSIZED_RECORD
        );
        assert_eq!(counters.oversized.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn full_queue_drops_immediately_without_waiting_for_receiver() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let counters = Arc::new(Counters::default());
        let sink = RecordSink {
            sender: Some(sender),
            counters: counters.clone(),
        };
        sink.record().write_all(b"first\n").expect("first");
        for _ in 0..1000 {
            sink.record()
                .write_all(b"drop\n")
                .expect("lossy submission");
        }
        assert_eq!(counters.dropped.load(Ordering::Relaxed), 1000);
        assert_eq!(receiver.recv().expect("retained first record"), b"first\n");
    }

    #[test]
    fn tracing_json_and_service_details_reach_independent_files() {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("log-budget-tests")
            .join(uuid::Uuid::new_v4().to_string());
        let (sink, guard) = RecordSink::start(root.clone(), "boundlessd.log", Policy::RUNTIME);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink)
            .json()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(attempts = 7, "test diagnostic")
        });
        drop(guard);
        let runtime = std::fs::read_to_string(root.join("boundlessd.log")).expect("persisted JSON");
        let value: serde_json::Value =
            serde_json::from_str(runtime.trim()).expect("valid JSON record");
        assert_eq!(value["fields"]["attempts"], 7);
        let (startup, guard) = RecordSink::start(
            root.clone(),
            "boundless-service-startup.log",
            Policy::STARTUP,
        );
        writeln!(
            startup.record(),
            "stage=test detail={}",
            Sanitized("first\r\nsecond")
        )
        .expect("startup diagnostic");
        drop(guard);
        assert_eq!(
            std::fs::read_to_string(root.join("boundless-service-startup.log"))
                .expect("startup file"),
            "stage=test detail=first  second\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("boundlessd.log")).expect("runtime retained"),
            runtime
        );
        std::fs::remove_dir_all(root).expect("remove isolated test logs");
    }
}
