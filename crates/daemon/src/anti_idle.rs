use std::time::Duration;

use anyhow::Result;
use tokio::time;
use tracing::warn;

use crate::{
    runtime_tasks::{RuntimeTaskOwner, RuntimeTaskShutdown, RuntimeTaskSpec},
    state::AppState,
};

const ANTI_IDLE_SAFETY_TICK: Duration = Duration::from_secs(1);

pub fn start(state: AppState) {
    let task_state = state.clone();
    state.spawn_runtime_task(
        RuntimeTaskSpec::new(
            "anti_idle.runtime",
            RuntimeTaskOwner::AntiIdle,
            RuntimeTaskShutdown::AbortOnDaemonShutdown,
        ),
        async move {
            if let Err(error) = run(task_state).await {
                warn!(error = ?error, "anti-idle runtime stopped");
            }
        },
    );
}

async fn run(state: AppState) -> Result<()> {
    let mut worker = platform_windows::runtime::spawn_anti_idle_power_worker()?;
    let wake = state.anti_idle_wake_signal();
    let mut ticker = time::interval(ANTI_IDLE_SAFETY_TICK);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut last_flags = u32::MAX;
    loop {
        let wake_notified = wake.notified();
        tokio::pin!(wake_notified);

        if !wake.take_pending() {
            tokio::select! {
                _ = &mut wake_notified => {
                    let _ = wake.take_pending();
                }
                _ = ticker.tick() => {}
            }
        }

        let runtime = state.reconcile_anti_idle_runtime().await;
        if runtime.desired_execution_state_flags != last_flags {
            worker.apply(runtime.desired_execution_state_flags)?;
            last_flags = runtime.desired_execution_state_flags;
        }
    }
}
