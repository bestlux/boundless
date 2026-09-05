//! Bounded, best-effort disk storage. Only the background logging worker uses this writer.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024;
pub(super) const QUEUE_RECORDS: usize = 256;
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const OVERSIZED_RECORD: &[u8] =
    b"{\"level\":\"WARN\",\"message\":\"log record omitted: exceeds 16384-byte limit\"}\n";

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub segment_bytes: u64,
    pub files: usize,
    pub retention: Duration,
}

impl Policy {
    pub const RUNTIME: Self = Self {
        segment_bytes: 10 * 1024 * 1024,
        files: 10,
        retention: Duration::from_secs(14 * 24 * 60 * 60),
    };
    pub const STARTUP: Self = Self {
        segment_bytes: 1024 * 1024,
        files: 4,
        retention: Self::RUNTIME.retention,
    };
}

#[derive(Default)]
pub(super) struct Counters {
    pub disk_ready: AtomicBool,
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub oversized: AtomicU64,
    pub io_failures: AtomicU64,
}

struct Entry {
    path: PathBuf,
    modified: SystemTime,
    active: bool,
}

pub(super) struct DiskWriter {
    directory: PathBuf,
    base: &'static str,
    policy: Policy,
    file: Option<File>,
    lock: Option<File>,
    written: u64,
    segment_created: SystemTime,
    last_maintenance: Instant,
    retry_at: Option<Instant>,
    counters: Arc<Counters>,
}

impl DiskWriter {
    pub fn new(
        directory: PathBuf,
        base: &'static str,
        policy: Policy,
        counters: Arc<Counters>,
    ) -> Self {
        assert!(matches!(
            base,
            "boundlessd.log" | "boundless-service-startup.log"
        ));
        assert!(policy.files >= 2 && policy.segment_bytes >= MAX_RECORD_BYTES as u64);
        Self {
            directory,
            base,
            policy,
            file: None,
            lock: None,
            written: 0,
            segment_created: SystemTime::now(),
            last_maintenance: Instant::now(),
            retry_at: None,
            counters,
        }
    }

    fn active_path(&self) -> PathBuf {
        self.directory.join(self.base)
    }

    fn archive_path(&self, index: usize) -> PathBuf {
        self.directory.join(format!("{}.{index}", self.base))
    }

    fn initialize(&mut self) -> io::Result<()> {
        for ancestor in self.directory.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(metadata) if is_link(&metadata) => {
                    return Err(io::Error::other(
                        "log directory ancestry must not contain links",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        fs::create_dir_all(&self.directory)?;
        let metadata = fs::symlink_metadata(&self.directory)?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(io::Error::other(
                "log directory must be an ordinary directory",
            ));
        }
        self.directory = self.directory.canonicalize()?;
        let lock_path = self.directory.join(format!("{}.lock", self.base));
        check_regular_if_present(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.try_lock().map_err(io::Error::other)?;
        self.lock = Some(lock);
        // Reserve one slot for a newly-created active file. This also bounds legacy
        // daily files on upgrade without ever reading an oversized file's contents.
        self.prune(self.policy.files - 1, false, SystemTime::now())?;
        self.open_active()?;
        self.last_maintenance = Instant::now();
        self.counters.disk_ready.store(true, Ordering::Release);
        Ok(())
    }

    fn open_active(&mut self) -> io::Result<()> {
        let path = self.active_path();
        check_regular_if_present(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let metadata = file.metadata()?;
        self.written = metadata.len();
        self.segment_created = metadata.modified()?;
        self.file = Some(file);
        Ok(())
    }

    fn prune(&self, keep: usize, protect_active: bool, now: SystemTime) -> io::Result<()> {
        // Retain only a bounded list of survivors; never recurse or collect the
        // full directory. Unknown filenames and other applications are untouched.
        let mut survivors: Vec<Entry> = Vec::with_capacity(self.policy.files + 1);
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !owned_name(self.base, name) {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            ensure_regular(&metadata)?;
            let active = name == self.base && protect_active;
            let modified = metadata.modified()?;
            let expired = now.duration_since(modified).unwrap_or_default() >= self.policy.retention;
            if !active && (metadata.len() > self.policy.segment_bytes || expired) {
                fs::remove_file(path)?;
                continue;
            }
            survivors.push(Entry {
                path,
                modified,
                active,
            });
            if survivors.len() > keep {
                let oldest = survivors
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| !item.active)
                    .min_by(|(_, a), (_, b)| {
                        a.modified
                            .cmp(&b.modified)
                            .then_with(|| a.path.cmp(&b.path))
                    })
                    .map(|(index, _)| index)
                    .ok_or_else(|| io::Error::other("cannot enforce log retention"))?;
                fs::remove_file(survivors.swap_remove(oldest).path)?;
            }
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        remove_regular_if_present(&self.archive_path(self.policy.files - 1))?;
        for index in (1..self.policy.files - 1).rev() {
            let source = self.archive_path(index);
            if check_regular_if_present(&source)? {
                let target = self.archive_path(index + 1);
                check_regular_if_present(&target)?;
                fs::rename(source, target)?;
            }
        }
        let active = self.active_path();
        if check_regular_if_present(&active)? {
            let target = self.archive_path(1);
            check_regular_if_present(&target)?;
            fs::rename(active, target)?;
        }
        self.prune(self.policy.files - 1, false, SystemTime::now())?;
        self.open_active()?;
        self.last_maintenance = Instant::now();
        Ok(())
    }

    fn try_maintenance(&mut self) -> io::Result<()> {
        if self.file.is_none() || self.last_maintenance.elapsed() < MAINTENANCE_INTERVAL {
            return Ok(());
        }
        let now = SystemTime::now();
        if now.duration_since(self.segment_created).unwrap_or_default() >= self.policy.retention {
            self.rotate()?;
        } else {
            self.prune(self.policy.files, true, now)?;
            self.last_maintenance = Instant::now();
        }
        Ok(())
    }

    pub fn maintenance(&mut self) {
        if self.try_maintenance().is_err() {
            self.fail();
        }
    }

    fn fail(&mut self) {
        // Fail closed: do not keep appending when rotation/cleanup cannot uphold
        // the budget. Report only the first failure, from this background thread.
        self.file = None;
        self.lock = None;
        self.counters.disk_ready.store(false, Ordering::Release);
        self.retry_at = Some(Instant::now() + FAILURE_COOLDOWN);
        if self.counters.io_failures.fetch_add(1, Ordering::Relaxed) == 0 {
            report_failure(self.base);
        }
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.file.is_none() {
            self.initialize()?;
        }
        self.try_maintenance()?;
        if self.written + bytes.len() as u64 > self.policy.segment_bytes {
            self.rotate()?;
        }
        self.file
            .as_mut()
            .expect("active log opened")
            .write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

pub(super) fn report_failure(base: &str) {
    let _ = writeln!(
        io::stderr().lock(),
        "boundless_logging=unavailable stream={base}; disk diagnostics are being dropped; daemon operation continues"
    );
}

impl Write for DiskWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(bytes.len());
        }
        let record = if bytes.len() > MAX_RECORD_BYTES {
            self.counters.oversized.fetch_add(1, Ordering::Relaxed);
            OVERSIZED_RECORD
        } else {
            bytes
        };
        if self.append(record).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.fail();
        } else {
            self.counters.written.fetch_add(1, Ordering::Relaxed);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // File::write_all already reaches the OS. No synchronous durability wait
        // is required for best-effort diagnostics or application shutdown.
        Ok(())
    }
}

fn owned_name(base: &str, name: &str) -> bool {
    if name == base {
        return true;
    }
    let Some(suffix) = name
        .strip_prefix(base)
        .and_then(|tail| tail.strip_prefix('.'))
    else {
        return false;
    };
    if let Ok(index) = suffix.parse::<u32>() {
        return index > 0 && index.to_string() == suffix;
    }
    suffix.len() == 10 && chrono::NaiveDate::parse_from_str(suffix, "%Y-%m-%d").is_ok()
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn ensure_regular(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() || is_link(metadata) {
        return Err(io::Error::other("owned log path must be an ordinary file"));
    }
    Ok(())
}

fn check_regular_if_present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_regular(&metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_regular_if_present(path: &Path) -> io::Result<()> {
    if check_regular_if_present(path)? {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("log-budget-tests")
                .join(uuid::Uuid::new_v4().to_string());
            fs::create_dir_all(&root).expect("create isolated fixture");
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn small_policy() -> Policy {
        Policy {
            segment_bytes: MAX_RECORD_BYTES as u64,
            files: 4,
            retention: Policy::RUNTIME.retention,
        }
    }

    fn writer(root: &Path, base: &'static str, policy: Policy) -> DiskWriter {
        DiskWriter::new(
            root.to_path_buf(),
            base,
            policy,
            Arc::new(Counters::default()),
        )
    }

    fn disk_usage(root: &Path, base: &str, policy: Policy) -> (usize, u64) {
        let mut files = 0;
        let mut bytes = 0;
        for entry in fs::read_dir(root).expect("read fixture") {
            let entry = entry.expect("entry");
            if !owned_name(base, &entry.file_name().to_string_lossy()) {
                continue;
            }
            // Windows directory-entry size can lag while the writer still holds
            // the active file. Query an opened handle to measure actual bytes.
            let metadata = File::open(entry.path())
                .expect("open retained log")
                .metadata()
                .expect("metadata");
            assert!(
                metadata.len() <= policy.segment_bytes,
                "segment exceeds budget"
            );
            files += 1;
            bytes += metadata.len();
        }
        assert!(
            files <= policy.files,
            "too many retained log files: {files}"
        );
        assert!(
            bytes <= policy.segment_bytes * policy.files as u64,
            "total exceeds budget"
        );
        (files, bytes)
    }

    #[test]
    fn sustained_writes_keep_runtime_and_startup_streams_within_independent_budgets() {
        for base in ["boundlessd.log", "boundless-service-startup.log"] {
            let fixture = Fixture::new();
            let policy = small_policy();
            let mut sink = writer(&fixture.0, base, policy);
            for sequence in 0..512 {
                let mut record = format!("record={sequence:04} ").into_bytes();
                record.resize(1023, b'x');
                record.push(b'\n');
                sink.write_all(&record).expect("write");
                disk_usage(&fixture.0, base, policy);
            }
            assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
            assert!(
                fs::read_to_string(fixture.0.join(base))
                    .expect("active file")
                    .contains("record=0511")
            );
        }
    }

    #[test]
    fn restart_resumes_existing_segments_without_escaping_budget() {
        let fixture = Fixture::new();
        let policy = small_policy();
        for _ in 0..12 {
            let mut sink = writer(&fixture.0, "boundlessd.log", policy);
            for _ in 0..11 {
                sink.write_all(&[b'x'; 8192]).expect("write");
            }
            assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
            disk_usage(&fixture.0, "boundlessd.log", policy);
        }
    }

    #[test]
    fn service_startup_default_policy_caps_actual_files_at_four_mib() {
        let fixture = Fixture::new();
        let policy = Policy::STARTUP;
        let mut sink = writer(&fixture.0, "boundless-service-startup.log", policy);
        for _ in 0..1024 {
            sink.write_all(&[b'x'; 8192]).expect("write 8 MiB");
        }
        let (files, bytes) = disk_usage(&fixture.0, "boundless-service-startup.log", policy);
        assert_eq!(files, 4);
        assert_eq!(bytes, 4 * 1024 * 1024);
        assert_eq!(sink.counters.written.load(Ordering::Relaxed), 1024);
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn oversized_service_startup_log_is_bounded_on_first_start_and_restart() {
        let fixture = Fixture::new();
        let policy = Policy::STARTUP;
        let path = fixture.0.join("boundless-service-startup.log");
        for _ in 0..2 {
            File::create(&path)
                .expect("legacy startup log")
                .set_len(policy.segment_bytes * 8)
                .expect("oversized legacy file");
            let mut sink = writer(&fixture.0, "boundless-service-startup.log", policy);
            sink.write_all(b"bounded startup\n")
                .expect("startup record");
            assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
            assert_eq!(
                fs::read(&path).expect("active startup"),
                b"bounded startup\n"
            );
            disk_usage(&fixture.0, "boundless-service-startup.log", policy);
        }
    }

    #[cfg(unix)]
    #[test]
    fn log_and_directory_symlinks_cannot_redirect_writes_or_pruning() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::write(outside.join("sentinel"), b"preserve").expect("sentinel");
        let logs = fixture.0.join("logs");
        fs::create_dir(&logs).expect("logs");
        symlink(outside.join("sentinel"), logs.join("boundlessd.log")).expect("file symlink");
        let mut sink = writer(&logs, "boundlessd.log", small_policy());
        sink.write_all(b"must not append").expect("best effort");
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 1);
        assert!(!sink.counters.disk_ready.load(Ordering::Acquire));
        assert_eq!(
            fs::read(outside.join("sentinel")).expect("sentinel"),
            b"preserve"
        );
        let linked_parent = fixture.0.join("linked-parent");
        symlink(&outside, &linked_parent).expect("directory symlink");
        let mut sink = writer(
            &linked_parent.join("nested-logs"),
            "boundlessd.log",
            small_policy(),
        );
        sink.write_all(b"must not create").expect("best effort");
        assert!(!outside.join("nested-logs").exists());
    }

    #[test]
    fn startup_prunes_oversized_legacy_active_and_daily_files_but_preserves_unknown_names() {
        let fixture = Fixture::new();
        let policy = small_policy();
        for name in ["boundlessd.log", "boundlessd.log.2026-09-01"] {
            File::create(fixture.0.join(name))
                .expect("create legacy")
                .set_len(policy.segment_bytes * 8)
                .expect("oversized legacy");
        }
        for index in 1..20 {
            fs::write(fixture.0.join(format!("boundlessd.log.{index}")), b"old\n")
                .expect("old archive");
        }
        for name in [
            "important.txt",
            "boundlessd.log.2026-99-01",
            "boundlessd.log.1.bak",
            "boundlessd.log.01",
        ] {
            fs::write(fixture.0.join(name), b"preserve").expect("unowned sentinel");
        }
        let mut sink = writer(&fixture.0, "boundlessd.log", policy);
        sink.write_all(b"new bounded log\n").expect("write");
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
        disk_usage(&fixture.0, "boundlessd.log", policy);
        assert!(!fixture.0.join("boundlessd.log.2026-09-01").exists());
        for name in [
            "important.txt",
            "boundlessd.log.2026-99-01",
            "boundlessd.log.1.bak",
            "boundlessd.log.01",
        ] {
            assert_eq!(
                fs::read(fixture.0.join(name)).expect("sentinel remains"),
                b"preserve"
            );
        }
    }

    #[test]
    fn age_retention_prunes_known_files_on_startup_and_idle_maintenance() {
        let fixture = Fixture::new();
        let policy = small_policy();
        let old = SystemTime::now() - policy.retention - Duration::from_secs(60);
        let expired = fixture.0.join("boundlessd.log.2026-08-01");
        let file = File::create(&expired).expect("create expired");
        file.set_modified(old).expect("set old timestamp");
        drop(file);
        let mut sink = writer(&fixture.0, "boundlessd.log", policy);
        sink.write_all(b"fresh\n").expect("write");
        assert!(!expired.exists());
        fs::write(&expired, b"expired\n").expect("recreate expired");
        File::options()
            .write(true)
            .open(&expired)
            .expect("open")
            .set_modified(old)
            .expect("age");
        sink.segment_created = old;
        sink.last_maintenance = Instant::now() - MAINTENANCE_INTERVAL;
        sink.maintenance();
        assert!(!expired.exists());
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
        disk_usage(&fixture.0, "boundlessd.log", policy);
    }

    #[test]
    fn rotation_failure_disables_writes_and_retries_only_after_cooldown() {
        let fixture = Fixture::new();
        let policy = small_policy();
        let mut sink = writer(&fixture.0, "boundlessd.log", policy);
        sink.write_all(&[b'x'; MAX_RECORD_BYTES])
            .expect("fill active segment");
        let obstruction = fixture.0.join("boundlessd.log.1");
        fs::create_dir(&obstruction).expect("obstruct rotation with a directory");
        sink.write_all(b"cannot rotate\n")
            .expect("logging remains best effort");
        for _ in 0..10_000 {
            sink.write_all(b"dropped during cooldown\n").expect("drop");
        }
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 1);
        assert_eq!(
            fs::metadata(fixture.0.join("boundlessd.log"))
                .expect("active unchanged")
                .len(),
            policy.segment_bytes
        );
        fs::remove_dir(&obstruction).expect("remove empty fixture obstruction");
        sink.retry_at = Some(Instant::now());
        sink.write_all(b"recovered\n").expect("retry");
        assert!(sink.counters.disk_ready.load(Ordering::Acquire));
        assert!(
            fs::read_to_string(fixture.0.join("boundlessd.log"))
                .expect("active")
                .contains("recovered")
        );
        disk_usage(&fixture.0, "boundlessd.log", policy);
    }

    #[test]
    fn unavailable_directory_and_concurrent_writer_fail_closed() {
        let fixture = Fixture::new();
        let blocked = fixture.0.join("not-a-directory");
        fs::write(&blocked, b"preserve").expect("obstruction");
        let mut unavailable = writer(&blocked, "boundlessd.log", small_policy());
        for _ in 0..100 {
            unavailable.write_all(b"drop\n").expect("best effort");
        }
        assert_eq!(unavailable.counters.io_failures.load(Ordering::Relaxed), 1);
        assert_eq!(fs::read(blocked).expect("preserved"), b"preserve");
        let mut first = writer(&fixture.0, "boundlessd.log", small_policy());
        first.write_all(b"owner\n").expect("first writer");
        let mut second = writer(&fixture.0, "boundlessd.log", small_policy());
        second.write_all(b"competitor\n").expect("best effort");
        assert_eq!(second.counters.io_failures.load(Ordering::Relaxed), 1);
        assert_eq!(
            fs::read(fixture.0.join("boundlessd.log")).expect("owned file"),
            b"owner\n"
        );
    }

    #[test]
    fn oversized_record_is_replaced_without_oversized_allocation_or_file() {
        let fixture = Fixture::new();
        let mut sink = writer(&fixture.0, "boundlessd.log", small_policy());
        sink.write_all(&vec![b'x'; MAX_RECORD_BYTES * 10])
            .expect("oversized record");
        assert_eq!(
            fs::read(fixture.0.join("boundlessd.log")).expect("record"),
            OVERSIZED_RECORD
        );
        assert_eq!(sink.counters.oversized.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[ignore = "intentional filesystem throughput and storage-budget benchmark"]
    fn disk_log_budget_benchmark() {
        let fixture = Fixture::new();
        let policy = Policy::RUNTIME;
        let mut sink = writer(&fixture.0, "boundlessd.log", policy);
        let record = [b'x'; 8192];
        let records: u64 = 256 * 1024 * 1024 / record.len() as u64;
        let started = Instant::now();
        let mut peak_retained_bytes = 0;
        for sequence in 0..records {
            sink.write_all(&record).expect("benchmark write");
            if (sequence + 1) % 128 == 0 {
                peak_retained_bytes =
                    peak_retained_bytes.max(disk_usage(&fixture.0, "boundlessd.log", policy).1);
            }
        }
        let elapsed = started.elapsed();
        let (retained_files, retained_bytes) = disk_usage(&fixture.0, "boundlessd.log", policy);
        peak_retained_bytes = peak_retained_bytes.max(retained_bytes);
        assert_eq!(sink.counters.io_failures.load(Ordering::Relaxed), 0);
        let bytes_processed = records * record.len() as u64;
        println!(
            "boundless_log_budget_benchmark={}",
            serde_json::json!({
                "records": records, "bytes_processed": bytes_processed, "elapsed_ms": elapsed.as_millis(),
                "throughput_mib_per_sec": bytes_processed as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64(),
                "retained_bytes": retained_bytes, "retained_files": retained_files, "peak_retained_bytes": peak_retained_bytes,
                "cap_total_bytes": policy.segment_bytes * policy.files as u64, "cap_segment_bytes": policy.segment_bytes,
                "cap_files": policy.files, "retention_days": policy.retention.as_secs() / (24 * 60 * 60)
                , "durability": "os_write_cache", "sample_interval_records": 128
            })
        );
    }
}
