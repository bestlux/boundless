use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeTaskOwner {
    AntiIdle,
    Clipboard,
    Discovery,
    Hotkeys,
    Input,
    Network,
    Pairing,
}

impl RuntimeTaskOwner {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AntiIdle => "anti_idle",
            Self::Clipboard => "clipboard",
            Self::Discovery => "discovery",
            Self::Hotkeys => "hotkeys",
            Self::Input => "input",
            Self::Network => "network",
            Self::Pairing => "pairing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeTaskShutdown {
    AbortOnDaemonShutdown,
}

impl RuntimeTaskShutdown {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AbortOnDaemonShutdown => "abort_on_daemon_shutdown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeTaskStatus {
    Running,
    Finished,
    Cancelling,
    Cancelled,
    Failed,
}

impl RuntimeTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeTaskSpec {
    pub(crate) name: &'static str,
    pub(crate) owner: RuntimeTaskOwner,
    pub(crate) shutdown: RuntimeTaskShutdown,
}

impl RuntimeTaskSpec {
    pub(crate) const fn new(
        name: &'static str,
        owner: RuntimeTaskOwner,
        shutdown: RuntimeTaskShutdown,
    ) -> Self {
        Self {
            name,
            owner,
            shutdown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeTaskSnapshot {
    pub(crate) name: &'static str,
    pub(crate) owner: &'static str,
    pub(crate) shutdown: &'static str,
    pub(crate) status: &'static str,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug)]
struct RuntimeTaskEntry {
    spec: RuntimeTaskSpec,
    status: RuntimeTaskStatus,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct RuntimeTaskRegistryInner {
    tasks: BTreeMap<&'static str, RuntimeTaskEntry>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeTaskRegistry {
    inner: Arc<Mutex<RuntimeTaskRegistryInner>>,
}

impl RuntimeTaskRegistry {
    pub(crate) fn spawn<F>(&self, spec: RuntimeTaskSpec, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task_name = spec.name;
        let mut previous = None;
        {
            let mut inner = self.inner.lock().expect("runtime task registry");
            if let Some(mut entry) = inner.tasks.remove(task_name) {
                entry.status = RuntimeTaskStatus::Cancelling;
                entry.updated_at = Utc::now();
                previous = entry.handle.take();
            }

            let now = Utc::now();
            inner.tasks.insert(
                task_name,
                RuntimeTaskEntry {
                    spec,
                    status: RuntimeTaskStatus::Running,
                    started_at: now,
                    updated_at: now,
                    handle: None,
                },
            );
        }

        if let Some(handle) = previous {
            warn!(
                task = task_name,
                "replacing existing runtime task registration"
            );
            handle.abort();
        }

        let registry = self.clone();
        let handle = tokio::spawn(async move {
            future.await;
            registry.mark_finished(task_name);
        });

        let mut inner = self.inner.lock().expect("runtime task registry");
        if let Some(entry) = inner.tasks.get_mut(task_name) {
            entry.handle = Some(handle);
        } else {
            handle.abort();
        }
    }

    pub(crate) fn snapshots(&self) -> Vec<RuntimeTaskSnapshot> {
        let inner = self.inner.lock().expect("runtime task registry");
        inner
            .tasks
            .values()
            .map(|entry| {
                let status = if entry.status == RuntimeTaskStatus::Running
                    && entry
                        .handle
                        .as_ref()
                        .map(|handle| handle.is_finished())
                        .unwrap_or(false)
                {
                    "finished_unobserved"
                } else {
                    entry.status.as_str()
                };

                RuntimeTaskSnapshot {
                    name: entry.spec.name,
                    owner: entry.spec.owner.as_str(),
                    shutdown: entry.spec.shutdown.as_str(),
                    status,
                    started_at: entry.started_at,
                    updated_at: entry.updated_at,
                }
            })
            .collect()
    }

    pub(crate) async fn shutdown(&self) {
        let handles = {
            let mut inner = self.inner.lock().expect("runtime task registry");
            let now = Utc::now();
            inner
                .tasks
                .iter_mut()
                .filter_map(|(name, entry)| {
                    entry.status = RuntimeTaskStatus::Cancelling;
                    entry.updated_at = now;
                    entry.handle.take().map(|handle| (*name, handle))
                })
                .collect::<Vec<_>>()
        };

        if handles.is_empty() {
            return;
        }

        info!(task_count = handles.len(), "shutting down runtime tasks");
        for (_, handle) in &handles {
            handle.abort();
        }

        for (name, handle) in handles {
            let status = match handle.await {
                Ok(()) => RuntimeTaskStatus::Finished,
                Err(error) if error.is_cancelled() => RuntimeTaskStatus::Cancelled,
                Err(error) => {
                    warn!(task = name, error = ?error, "runtime task join failed");
                    RuntimeTaskStatus::Failed
                }
            };
            self.mark_status(name, status);
        }
    }

    fn mark_finished(&self, task_name: &'static str) {
        self.mark_status(task_name, RuntimeTaskStatus::Finished);
    }

    fn mark_status(&self, task_name: &'static str, status: RuntimeTaskStatus) {
        let mut inner = self.inner.lock().expect("runtime task registry");
        if let Some(entry) = inner.tasks.get_mut(task_name) {
            entry.status = status;
            entry.updated_at = Utc::now();
        }
    }
}

pub(crate) fn task_health_json(snapshots: &[RuntimeTaskSnapshot]) -> Value {
    json!({
        "tasks": snapshots
            .iter()
            .map(|task| {
                json!({
                    "name": task.name,
                    "owner": task.owner,
                    "status": task.status,
                    "shutdown": task.shutdown,
                    "started_at": task.started_at.to_rfc3339(),
                    "updated_at": task.updated_at.to_rfc3339(),
                })
            })
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tokio::sync::oneshot;

    use super::*;

    fn test_spec(name: &'static str) -> RuntimeTaskSpec {
        RuntimeTaskSpec::new(
            name,
            RuntimeTaskOwner::Network,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        )
    }

    #[tokio::test]
    async fn snapshots_report_stable_redacted_metadata() {
        let registry = RuntimeTaskRegistry::default();
        let (_tx, rx) = oneshot::channel::<()>();

        registry.spawn(test_spec("network.supervisor"), async move {
            let _ = rx.await;
        });

        let snapshots = registry.snapshots();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "network.supervisor");
        assert_eq!(snapshots[0].owner, "network");
        assert_eq!(snapshots[0].shutdown, "abort_on_daemon_shutdown");
        assert_eq!(snapshots[0].status, "running");

        let health = task_health_json(&snapshots).to_string();
        assert!(health.contains("network.supervisor"));
        assert!(!health.contains("peer-"));
        assert!(!health.contains("machine"));

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn finished_tasks_report_finished() {
        let registry = RuntimeTaskRegistry::default();
        registry.spawn(test_spec("network.listener"), async {});

        for _ in 0..20 {
            if registry.snapshots()[0].status == "finished" {
                return;
            }
            tokio::task::yield_now().await;
        }

        panic!("task did not report finished");
    }

    #[tokio::test]
    async fn shutdown_aborts_and_joins_running_tasks() {
        let registry = RuntimeTaskRegistry::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let dropped_for_task = dropped.clone();
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (_tx, rx) = oneshot::channel::<()>();

        registry.spawn(test_spec("network.supervisor"), async move {
            struct DropMarker(Arc<AtomicBool>);
            impl Drop for DropMarker {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }

            let _marker = DropMarker(dropped_for_task);
            let _ = started_tx.send(());
            let _ = rx.await;
        });
        started_rx.await.expect("task started");

        registry.shutdown().await;

        assert!(dropped.load(Ordering::SeqCst));
        let snapshots = registry.snapshots();
        assert_eq!(snapshots[0].status, "cancelled");
    }
}
