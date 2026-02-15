use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

pub fn init_logging() -> Result<LoggingGuard> {
    let log_dir = log_dir();
    fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "boundlessd.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking)
                .json(),
        )
        .with(fmt::layer().compact())
        .init();

    Ok(LoggingGuard { _file_guard: guard })
}

fn log_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("logs")
}
