const ELEVATED_INPUT_APPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const ELEVATED_INPUT_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const ELEVATED_INPUT_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const ELEVATED_INPUT_SHUTDOWN_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ElevatedInputControllerStatus {
    state: platform_windows::elevated_input::InputInjectorState,
    reason: platform_windows::elevated_input::InputInjectorReason,
    signature_trust: platform_windows::elevated_input::InputInjectorSignatureTrust,
    helper_version: String,
    direct_fallback_safe: bool,
    direct_recovery_cleanup_required: bool,
}

impl Default for ElevatedInputControllerStatus {
    fn default() -> Self {
        Self {
            state: platform_windows::elevated_input::InputInjectorState::Off,
            reason: platform_windows::elevated_input::InputInjectorReason::None,
            signature_trust:
                platform_windows::elevated_input::InputInjectorSignatureTrust::Unspecified,
            helper_version: String::new(),
            direct_fallback_safe: true,
            direct_recovery_cleanup_required: false,
        }
    }
}

impl ElevatedInputControllerStatus {
    fn telemetry_fields(&self) -> (&'static str, &'static str, &'static str) {
        (
            platform_windows::elevated_input::state_name(self.state),
            platform_windows::elevated_input::reason_name(self.reason),
            platform_windows::elevated_input::signature_trust_name(self.signature_trust),
        )
    }
}

#[derive(Debug)]
enum ElevatedInputApplyResult {
    NotActive,
    Applied {
        committed_event_count: usize,
        remaining_events: Vec<core_input::InputEvent>,
        reason: platform_windows::elevated_input::InputInjectorReason,
        destination_num_lock_on: bool,
    },
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElevatedInputActivationResult {
    NotReady,
    Activated,
    Uncertain,
}

enum ElevatedInputControllerCommand {
    Enable,
    Activate(std::sync::mpsc::Sender<bool>),
    Apply {
        events: Vec<core_input::InputEvent>,
        destination_num_lock_on: bool,
        reply: std::sync::mpsc::Sender<ElevatedInputApplyResult>,
    },
    Disable(Option<std::sync::mpsc::Sender<bool>>),
    Shutdown(std::sync::mpsc::Sender<()>),
}

#[derive(Clone)]
pub(super) struct ElevatedInputController {
    inner: std::sync::Arc<ElevatedInputControllerInner>,
}

impl std::fmt::Debug for ElevatedInputController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ElevatedInputController")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

struct ElevatedInputControllerInner {
    commands: std::sync::mpsc::Sender<ElevatedInputControllerCommand>,
    status: std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    worker_done: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl ElevatedInputController {
    fn start() -> anyhow::Result<Self> {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let status = std::sync::Arc::new(std::sync::Mutex::new(
            ElevatedInputControllerStatus::default(),
        ));
        let worker_status = status.clone();
        let worker = std::thread::Builder::new()
            .name("boundless-elevated-input-controller".to_string())
            .spawn(move || {
                elevated_input_controller_worker(command_rx, worker_status);
                let _ = done_tx.send(());
            })
            .context("spawn elevated input controller")?;
        Ok(Self {
            inner: std::sync::Arc::new(ElevatedInputControllerInner {
                commands: command_tx,
                status,
                worker: std::sync::Mutex::new(Some(worker)),
                worker_done: std::sync::Mutex::new(Some(done_rx)),
            }),
        })
    }

    fn status(&self) -> ElevatedInputControllerStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn request_enable(&self) -> bool {
        {
            let mut status = self
                .inner
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !status.direct_fallback_safe
                || status.direct_recovery_cleanup_required
                || !matches!(
                    status.state,
                    platform_windows::elevated_input::InputInjectorState::Off
                        | platform_windows::elevated_input::InputInjectorState::Unavailable
                )
            {
                return false;
            }
            status.state = platform_windows::elevated_input::InputInjectorState::Prompting;
            status.reason = platform_windows::elevated_input::InputInjectorReason::None;
            status.signature_trust =
                platform_windows::elevated_input::InputInjectorSignatureTrust::Unspecified;
            status.helper_version.clear();
        }
        if self
            .inner
            .commands
            .send(ElevatedInputControllerCommand::Enable)
            .is_ok()
        {
            true
        } else {
            update_elevated_input_status(&self.inner.status, |status| {
                status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
                status.reason =
                    platform_windows::elevated_input::InputInjectorReason::IpcUnavailable;
            });
            false
        }
    }

    fn activate_if_ready(&self) -> ElevatedInputActivationResult {
        if self.status().state
            != platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle
        {
            return ElevatedInputActivationResult::NotReady;
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self
            .inner
            .commands
            .send(ElevatedInputControllerCommand::Activate(reply_tx))
            .is_err()
        {
            self.mark_delivery_uncertain();
            return ElevatedInputActivationResult::Uncertain;
        }
        match reply_rx.recv_timeout(ELEVATED_INPUT_REPLY_TIMEOUT) {
            Ok(true) => ElevatedInputActivationResult::Activated,
            Ok(false) => ElevatedInputActivationResult::NotReady,
            Err(_) => {
                // The worker may activate after this wait expires. Block the
                // ordinary lane and serialize a cleanup command behind that
                // activation instead of treating the frame as safe to send
                // directly.
                self.mark_delivery_uncertain();
                let _ = self
                    .inner
                    .commands
                    .send(ElevatedInputControllerCommand::Disable(None));
                ElevatedInputActivationResult::Uncertain
            }
        }
    }

    fn apply(
        &self,
        events: &[core_input::InputEvent],
        destination_num_lock_on: bool,
    ) -> ElevatedInputApplyResult {
        if self.status().state != platform_windows::elevated_input::InputInjectorState::Active {
            return ElevatedInputApplyResult::NotActive;
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self
            .inner
            .commands
            .send(ElevatedInputControllerCommand::Apply {
                events: events.to_vec(),
                destination_num_lock_on,
                reply: reply_tx,
            })
            .is_err()
        {
            self.mark_delivery_uncertain();
            return ElevatedInputApplyResult::Uncertain;
        }
        match reply_rx.recv_timeout(ELEVATED_INPUT_REPLY_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.mark_delivery_uncertain();
                let _ = self
                    .inner
                    .commands
                    .send(ElevatedInputControllerCommand::Disable(None));
                ElevatedInputApplyResult::Uncertain
            }
        }
    }

    fn request_disable(&self) -> bool {
        self.disable(false)
    }

    fn disable_and_wait(&self) -> bool {
        self.disable(true)
    }

    fn disable(&self, wait: bool) -> bool {
        let current = self.status();
        if current.state == platform_windows::elevated_input::InputInjectorState::Off {
            return true;
        }
        update_elevated_input_status(&self.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Stopping;
        });
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let reply = wait.then_some(reply_tx);
        if self
            .inner
            .commands
            .send(ElevatedInputControllerCommand::Disable(reply))
            .is_err()
        {
            self.mark_shutdown_incomplete();
            return false;
        }
        !wait
            || reply_rx
                .recv_timeout(ELEVATED_INPUT_CONTROL_TIMEOUT + ELEVATED_INPUT_REPLY_TIMEOUT)
                .unwrap_or(false)
    }

    fn direct_fallback_safe(&self) -> bool {
        self.status().direct_fallback_safe
    }

    fn direct_recovery_cleanup_required(&self) -> bool {
        self.status().direct_recovery_cleanup_required
    }

    fn complete_direct_recovery_cleanup(&self) {
        update_elevated_input_status(&self.inner.status, |status| {
            if status.direct_recovery_cleanup_required {
                status.state = platform_windows::elevated_input::InputInjectorState::Off;
                status.reason = platform_windows::elevated_input::InputInjectorReason::ParentExited;
                status.direct_fallback_safe = true;
                status.direct_recovery_cleanup_required = false;
            }
        });
    }

    fn mark_delivery_uncertain(&self) {
        update_elevated_input_status(&self.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
            status.reason =
                platform_windows::elevated_input::InputInjectorReason::DeliveryUncertain;
            status.direct_fallback_safe = false;
        });
    }

    fn mark_shutdown_incomplete(&self) {
        update_elevated_input_status(&self.inner.status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
            status.reason =
                platform_windows::elevated_input::InputInjectorReason::ShutdownIncomplete;
        });
    }
}

impl Drop for ElevatedInputControllerInner {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + ELEVATED_INPUT_SHUTDOWN_JOIN_TIMEOUT;
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let _ = self
            .commands
            .send(ElevatedInputControllerCommand::Shutdown(reply_tx));
        let _ = reply_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
        let done = self
            .worker_done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_none_or(|receiver| {
                receiver
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .is_ok()
            });
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && done
        {
            let _ = worker.join();
        }
    }
}

fn elevated_input_controller_worker(
    commands: std::sync::mpsc::Receiver<ElevatedInputControllerCommand>,
    status: std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            update_elevated_input_status(&status, |status| {
                status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
                status.reason =
                    platform_windows::elevated_input::InputInjectorReason::IpcUnavailable;
            });
            return;
        }
    };
    let mut client: Option<platform_windows::elevated_input::InjectorClient> = None;
    let mut last_heartbeat = std::time::Instant::now();
    loop {
        let heartbeat_due = platform_windows::elevated_input::INPUT_INJECTOR_HEARTBEAT_INTERVAL
            .saturating_sub(last_heartbeat.elapsed());
        match commands.recv_timeout(heartbeat_due) {
            Ok(ElevatedInputControllerCommand::Enable) => {
                if client.is_some() {
                    continue;
                }
                let launch = match platform_windows::elevated_input::launch_explicit() {
                    Ok(launch) => launch,
                    Err(error) => {
                        let reason = classify_injector_launch_error(&error);
                        update_elevated_input_status(&status, |status| {
                            status.state = if reason
                                == platform_windows::elevated_input::InputInjectorReason::UserCancelled
                            {
                                platform_windows::elevated_input::InputInjectorState::Off
                            } else {
                                platform_windows::elevated_input::InputInjectorState::Unavailable
                            };
                            status.reason = reason;
                            status.direct_fallback_safe = true;
                            status.direct_recovery_cleanup_required = false;
                        });
                        continue;
                    }
                };
                let launch_trust = launch.signature_trust();
                match runtime.block_on(platform_windows::elevated_input::InjectorClient::connect(
                    launch,
                )) {
                    Ok(connected) => {
                        let connected_status = connected.status().clone();
                        update_elevated_input_status(&status, |status| {
                            status.state = platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle;
                            status.reason = connected_status.reason;
                            status.signature_trust = connected_status.signature_trust;
                            status.helper_version = connected_status.helper_version;
                            status.direct_fallback_safe = true;
                            status.direct_recovery_cleanup_required = false;
                        });
                        client = Some(connected);
                        last_heartbeat = std::time::Instant::now();
                    }
                    Err(error) => {
                        let reason = classify_injector_launch_error(&error);
                        update_elevated_input_status(&status, |status| {
                            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
                            status.reason = reason;
                            status.signature_trust = launch_trust;
                            // Attachment never succeeded, so this helper could
                            // not have accepted input.
                            status.direct_fallback_safe = true;
                            status.direct_recovery_cleanup_required = false;
                        });
                    }
                }
            }
            Ok(ElevatedInputControllerCommand::Activate(reply)) => {
                let activated = client.is_some()
                    && read_elevated_input_status(&status).state
                        == platform_windows::elevated_input::InputInjectorState::ReadyPendingIdle;
                if activated {
                    update_elevated_input_status(&status, |status| {
                        status.state = platform_windows::elevated_input::InputInjectorState::Active;
                        status.reason = platform_windows::elevated_input::InputInjectorReason::None;
                        status.direct_fallback_safe = false;
                        status.direct_recovery_cleanup_required = false;
                    });
                }
                let _ = reply.send(activated);
            }
            Ok(ElevatedInputControllerCommand::Apply {
                events,
                destination_num_lock_on,
                reply,
            }) => {
                let Some(active) = client.as_mut() else {
                    let _ = reply.send(ElevatedInputApplyResult::NotActive);
                    continue;
                };
                if read_elevated_input_status(&status).state
                    != platform_windows::elevated_input::InputInjectorState::Active
                {
                    let _ = reply.send(ElevatedInputApplyResult::NotActive);
                    continue;
                }
                let apply = runtime.block_on(tokio::time::timeout(
                    ELEVATED_INPUT_APPLY_TIMEOUT,
                    active.apply(&events, destination_num_lock_on),
                ));
                match apply {
                    Ok(Ok(outcome)) => {
                        update_elevated_input_status(&status, |status| {
                            status.reason = outcome.reason;
                        });
                        let result = ElevatedInputApplyResult::Applied {
                            committed_event_count: outcome.committed_event_count,
                            remaining_events: outcome.remaining_events,
                            reason: outcome.reason,
                            destination_num_lock_on: outcome.destination_num_lock_on,
                        };
                        let _ = reply.send(result);
                    }
                    Ok(Err(_)) | Err(_) => {
                        update_elevated_input_status(&status, |status| {
                            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
                            status.reason = platform_windows::elevated_input::InputInjectorReason::DeliveryUncertain;
                            status.direct_fallback_safe = false;
                        });
                        let _ = reply.send(ElevatedInputApplyResult::Uncertain);
                    }
                }
            }
            Ok(ElevatedInputControllerCommand::Disable(reply)) => {
                let stopped = stop_elevated_input_client(&runtime, &status, &mut client);
                if let Some(reply) = reply {
                    let _ = reply.send(stopped);
                }
            }
            Ok(ElevatedInputControllerCommand::Shutdown(reply)) => {
                let _ = stop_elevated_input_client(&runtime, &status, &mut client);
                let _ = reply.send(());
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(active) = client.as_mut() {
                    let heartbeat = runtime.block_on(tokio::time::timeout(
                        ELEVATED_INPUT_CONTROL_TIMEOUT,
                        active.heartbeat(),
                    ));
                    if !matches!(heartbeat, Ok(Ok(()))) {
                        update_elevated_input_status(&status, |status| {
                            status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
                            status.reason = platform_windows::elevated_input::InputInjectorReason::IpcUnavailable;
                        });
                        let _ = stop_elevated_input_client(&runtime, &status, &mut client);
                    }
                }
                last_heartbeat = std::time::Instant::now();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = stop_elevated_input_client(&runtime, &status, &mut client);
                break;
            }
        }
    }
}

fn stop_elevated_input_client(
    runtime: &tokio::runtime::Runtime,
    status: &std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
    client: &mut Option<platform_windows::elevated_input::InjectorClient>,
) -> bool {
    let current = read_elevated_input_status(status);
    let fallback_was_safe = current.direct_fallback_safe;
    let recovery_was_required = current.direct_recovery_cleanup_required;
    let Some(active) = client.take() else {
        let helper_lane_absent = platform_windows::elevated_input::direct_input_lane_available()
            .unwrap_or(false);
        if recovery_was_required || !fallback_was_safe {
            mark_direct_recovery_required_after_helper_exit(status, helper_lane_absent);
            // This second, cleanup-owned stop may proceed only after the
            // cross-process helper marker has disappeared. The broker still
            // owns the held-input ledger and must inject its release snapshot
            // before it marks ordinary input safe again.
            return helper_lane_absent;
        }
        update_elevated_input_status(status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Off;
            status.direct_fallback_safe = true;
            status.direct_recovery_cleanup_required = false;
        });
        return true;
    };
    update_elevated_input_status(status, |status| {
        status.state = platform_windows::elevated_input::InputInjectorState::Stopping;
    });
    let stopped = runtime.block_on(tokio::time::timeout(
        ELEVATED_INPUT_CONTROL_TIMEOUT,
        active.release_and_shutdown(),
    ));
    match stopped {
        Ok(Ok(stopped_status)) => {
            update_elevated_input_status(status, |status| {
                status.state = platform_windows::elevated_input::InputInjectorState::Off;
                status.reason = stopped_status.reason;
                status.signature_trust = stopped_status.signature_trust;
                status.helper_version = stopped_status.helper_version;
                status.direct_fallback_safe = true;
                status.direct_recovery_cleanup_required = false;
            });
            true
        }
        Ok(Err(_)) | Err(_) => {
            let helper_lane_absent =
                platform_windows::elevated_input::direct_input_lane_available().unwrap_or(false);
            // A failed stop of a present client can leave helper-injected keys
            // held even after the process and its mutex disappear. Never make
            // the direct lane safe in this same call: the broker must first
            // reset authorization and inject its conservative Up snapshot.
            mark_direct_recovery_required_after_helper_exit(status, helper_lane_absent);
            false
        }
    }
}

fn mark_direct_recovery_required_after_helper_exit(
    status: &std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
    helper_lane_absent: bool,
) {
    update_elevated_input_status(status, |status| {
        status.state = platform_windows::elevated_input::InputInjectorState::Unavailable;
        status.reason = if helper_lane_absent {
            platform_windows::elevated_input::InputInjectorReason::ParentExited
        } else {
            platform_windows::elevated_input::InputInjectorReason::ShutdownIncomplete
        };
        status.direct_fallback_safe = false;
        status.direct_recovery_cleanup_required = true;
    });
}

fn read_elevated_input_status(
    status: &std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
) -> ElevatedInputControllerStatus {
    status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn update_elevated_input_status(
    status: &std::sync::Arc<std::sync::Mutex<ElevatedInputControllerStatus>>,
    update: impl FnOnce(&mut ElevatedInputControllerStatus),
) {
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    update(&mut status);
}

fn classify_injector_launch_error(
    error: &anyhow::Error,
) -> platform_windows::elevated_input::InputInjectorReason {
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("permission prompt was canceled") {
        platform_windows::elevated_input::InputInjectorReason::UserCancelled
    } else if message.contains("another elevated input injector")
        || message.contains("duplicate")
        || message.contains("already exists")
    {
        platform_windows::elevated_input::InputInjectorReason::Duplicate
    } else if message.contains("was not found") {
        platform_windows::elevated_input::InputInjectorReason::NotInstalled
    } else if message.contains("invalid authenticode")
        || (message.contains("invalid") && message.contains("signature"))
    {
        platform_windows::elevated_input::InputInjectorReason::SignatureInvalid
    } else if message.contains("administrator-app control")
        || message.contains("identity")
        || message.contains("session")
        || message.contains("permission_denied")
        || message.contains("permission denied")
        || message.contains("client process validation")
        || message.contains("launch handshake rejected")
    {
        platform_windows::elevated_input::InputInjectorReason::IdentityRejected
    } else if message.contains("path") {
        platform_windows::elevated_input::InputInjectorReason::WrongPath
    } else if message.contains("protocol") {
        platform_windows::elevated_input::InputInjectorReason::ProtocolMismatch
    } else {
        platform_windows::elevated_input::InputInjectorReason::IpcUnavailable
    }
}

#[cfg(test)]
mod elevated_input_controller_tests {
    use super::*;

    #[test]
    fn controller_starts_off_without_requesting_elevation() {
        let status = ElevatedInputControllerStatus::default();
        assert_eq!(
            status.state,
            platform_windows::elevated_input::InputInjectorState::Off
        );
        assert!(status.direct_fallback_safe);
    }

    #[test]
    fn launch_errors_map_to_bounded_reasons() {
        let cases = [
            ("Windows permission prompt was canceled", platform_windows::elevated_input::InputInjectorReason::UserCancelled),
            ("another elevated input injector already exists", platform_windows::elevated_input::InputInjectorReason::Duplicate),
            ("elevated input injector rejected attachment: DUPLICATE", platform_windows::elevated_input::InputInjectorReason::Duplicate),
            ("installed elevated input injector was not found", platform_windows::elevated_input::InputInjectorReason::NotInstalled),
            ("invalid Authenticode state", platform_windows::elevated_input::InputInjectorReason::SignatureInvalid),
            ("elevated input injector reported invalid signature trust", platform_windows::elevated_input::InputInjectorReason::SignatureInvalid),
            ("administrator-app control requires the interactive session", platform_windows::elevated_input::InputInjectorReason::IdentityRejected),
            ("status: PermissionDenied, message: injector client process validation failed", platform_windows::elevated_input::InputInjectorReason::IdentityRejected),
            ("injector launch handshake rejected", platform_windows::elevated_input::InputInjectorReason::IdentityRejected),
            ("protocol revision mismatch", platform_windows::elevated_input::InputInjectorReason::ProtocolMismatch),
        ];
        for (message, expected) in cases {
            assert_eq!(classify_injector_launch_error(&anyhow::anyhow!(message)), expected);
        }
        assert_eq!(
            classify_injector_launch_error(&anyhow::anyhow!("opaque transport error")),
            platform_windows::elevated_input::InputInjectorReason::IpcUnavailable
        );
    }

    #[test]
    fn confirmed_helper_exit_requires_broker_cleanup_before_direct_fallback() {
        let status = std::sync::Arc::new(std::sync::Mutex::new(
            ElevatedInputControllerStatus {
                state: platform_windows::elevated_input::InputInjectorState::Unavailable,
                reason: platform_windows::elevated_input::InputInjectorReason::DeliveryUncertain,
                direct_fallback_safe: false,
                ..ElevatedInputControllerStatus::default()
            },
        ));
        mark_direct_recovery_required_after_helper_exit(&status, true);
        let pending = read_elevated_input_status(&status);
        assert_eq!(
            pending.state,
            platform_windows::elevated_input::InputInjectorState::Unavailable
        );
        assert_eq!(
            pending.reason,
            platform_windows::elevated_input::InputInjectorReason::ParentExited
        );
        assert!(!pending.direct_fallback_safe);
        assert!(pending.direct_recovery_cleanup_required);

        update_elevated_input_status(&status, |status| {
            status.state = platform_windows::elevated_input::InputInjectorState::Off;
            status.reason = platform_windows::elevated_input::InputInjectorReason::ParentExited;
            status.direct_fallback_safe = true;
            status.direct_recovery_cleanup_required = false;
        });
        let recovered = read_elevated_input_status(&status);
        assert_eq!(
            recovered.state,
            platform_windows::elevated_input::InputInjectorState::Off
        );
        assert_eq!(
            recovered.reason,
            platform_windows::elevated_input::InputInjectorReason::ParentExited
        );
        assert!(recovered.direct_fallback_safe);
        assert!(!recovered.direct_recovery_cleanup_required);
    }
}
