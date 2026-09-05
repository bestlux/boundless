#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("boundless-service {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    service_entry::run()
}

#[cfg(not(windows))]
fn main() {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("boundless-service {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    eprintln!("boundless-service is only supported on Windows");
}

#[cfg(windows)]
mod service_entry {
    use std::{
        env,
        ffi::OsString,
        io, panic,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use anyhow::{Context, Result};
    use tokio::{sync::watch, time};
    use tonic::transport::Server;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
        service_dispatcher,
    };

    use boundless_daemon::{
        config::ApiTransport,
        host::{
            HostOverrides, HostRuntimeOptions, prepare_runtime_with_options,
            runtime_task_health_json, shutdown_runtime, start_runtime_tasks,
        },
        input::InputRuntimeMode,
        logging::{self, append_service_startup_diagnostic},
        shared_control_plane_app,
    };
    use platform_windows::runtime::{
        named_pipe_incoming_for_allowed_user, validate_allowed_user_sid_shape,
    };

    const SERVICE_NAME: &str = "BoundlessService";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    const SERVICE_START_WAIT_HINT: Duration = Duration::from_secs(10);
    const SERVICE_STOP_WAIT_HINT: Duration = Duration::from_secs(5);
    const SERVICE_CONTROL_PLANE_STOP_GRACE: Duration = Duration::from_secs(2);
    const SERVICE_RUNTIME_TASK_STOP_GRACE: Duration = Duration::from_secs(1);
    const TOKIO_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn service_main(arguments: Vec<OsString>) {
        let _startup_logging = logging::init_service_startup_logging();
        install_service_panic_hook();
        if let Err(error) = run_service(startup_arguments(arguments)) {
            append_service_startup_diagnostic("service_main_failed", &format!("{error:#}"));
            tracing::error!(%error, "boundless service failed");
        }
    }

    fn run_service(arguments: Vec<OsString>) -> windows_service::Result<()> {
        let _logging = logging::init_logging().ok();
        append_service_startup_diagnostic("starting", "boundless service entrypoint reached");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let status_handle_slot: Arc<Mutex<Option<ServiceStatusHandle>>> =
            Arc::new(Mutex::new(None));
        let status_handle_for_handler = status_handle_slot.clone();

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop => {
                    request_service_shutdown(
                        &shutdown_tx,
                        &status_handle_for_handler,
                        "control_stop_requested",
                        "SCM stop control received",
                    );
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Shutdown => {
                    request_service_shutdown(
                        &shutdown_tx,
                        &status_handle_for_handler,
                        "control_shutdown_requested",
                        "SCM shutdown control received",
                    );
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
        {
            let mut slot = status_handle_slot
                .lock()
                .expect("service status handle slot");
            *slot = Some(status_handle);
        }
        if *shutdown_rx.borrow() {
            status_handle.set_service_status(service_status(ServiceState::StopPending))?;
        } else {
            status_handle.set_service_status(service_status(ServiceState::StartPending))?;
        }
        let allowed_user_sid = match parse_allowed_user_sid(arguments) {
            Ok(sid) => {
                append_service_startup_diagnostic("allowed_user_sid", &sid);
                sid
            }
            Err(error) => {
                append_service_startup_diagnostic(
                    "invalid_allowed_user_sid",
                    &format!("{error:#}"),
                );
                let _ = status_handle
                    .set_service_status(service_status_with_exit(ServiceState::Stopped, 1));
                return Err(windows_service::Error::Winapi(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    error.to_string(),
                )));
            }
        };

        let runtime = tokio::runtime::Runtime::new().map_err(windows_service::Error::Winapi)?;
        let result = runtime.block_on(async move {
            let shutdown_status_rx = shutdown_rx.clone();
            let result = run_daemon(shutdown_rx, allowed_user_sid, || {
                status_handle.set_service_status(service_status(ServiceState::Running))
            })
            .await;
            if *shutdown_status_rx.borrow() {
                append_service_startup_diagnostic(
                    "stop_pending",
                    "service runtime returned after shutdown request",
                );
                status_handle.set_service_status(service_status(ServiceState::StopPending))?;
            }
            result
        });
        runtime.shutdown_timeout(TOKIO_RUNTIME_SHUTDOWN_TIMEOUT);

        if let Err(error) = result {
            append_service_startup_diagnostic("runtime_error", &format!("{error:#}"));
            tracing::error!(%error, "boundless service runtime stopped with error");
            let _ = status_handle
                .set_service_status(service_status_with_exit(ServiceState::Stopped, 1));
            return Err(windows_service::Error::Winapi(io::Error::other(format!(
                "boundless service runtime failed: {error:#}"
            ))));
        }

        let stopped_status = service_status(ServiceState::Stopped);
        let _ = status_handle.set_service_status(stopped_status);
        append_service_startup_diagnostic("stopped", "service stopped cleanly");

        Ok(())
    }

    fn request_service_shutdown(
        shutdown_tx: &watch::Sender<bool>,
        status_handle_slot: &Arc<Mutex<Option<ServiceStatusHandle>>>,
        stage: &str,
        detail: &str,
    ) {
        report_stop_pending(status_handle_slot);
        let _ = shutdown_tx.send(true);
        append_service_startup_diagnostic(stage, detail);
    }

    fn report_stop_pending(status_handle_slot: &Arc<Mutex<Option<ServiceStatusHandle>>>) {
        let status_handle = {
            let slot = status_handle_slot
                .lock()
                .expect("service status handle slot");
            *slot
        };
        let Some(status_handle) = status_handle else {
            append_service_startup_diagnostic(
                "stop_pending_skipped",
                "service status handle not registered yet",
            );
            return;
        };
        match status_handle.set_service_status(service_status(ServiceState::StopPending)) {
            Ok(()) => append_service_startup_diagnostic(
                "stop_pending",
                "service control handler reported StopPending",
            ),
            Err(error) => append_service_startup_diagnostic(
                "stop_pending_failed",
                &format!("failed to report StopPending: {error:#}"),
            ),
        }
    }

    async fn run_daemon(
        shutdown_rx: watch::Receiver<bool>,
        allowed_user_sid: String,
        mark_running: impl FnOnce() -> windows_service::Result<()>,
    ) -> Result<()> {
        let options = HostRuntimeOptions {
            input_runtime_mode: InputRuntimeMode::ServiceSessionUnsupported,
        };
        append_service_startup_diagnostic("prepare_runtime", "loading service control-plane state");
        let runtime = prepare_runtime_with_options(
            HostOverrides {
                bind: None,
                api_transport: Some(ApiTransport::NamedPipe),
                api_pipe_name: None,
                network_port: None,
            },
            options,
        )
        .await
        .context("prepare service control-plane runtime")?;
        // Broker attach/exchange authorization compares the verified pipe
        // client identity against this SID; without it every attach fails
        // closed even though admins keep pipe access for diagnostics.
        runtime
            .state
            .set_input_broker_allowed_user_sid(&allowed_user_sid);

        let control_plane =
            adapter_ipc_grpc::ControlPlaneApi::new(shared_control_plane_app(runtime.state.clone()))
                .into_server();

        append_service_startup_diagnostic(
            "bind_named_pipe",
            &format!(
                "pipe={} allowed_user_sid={allowed_user_sid}",
                runtime.snapshot.api_pipe_name
            ),
        );
        let incoming = named_pipe_incoming_for_allowed_user(
            &runtime.snapshot.api_pipe_name,
            &allowed_user_sid,
        )
        .with_context(|| format!("initialize named pipe {}", runtime.snapshot.api_pipe_name))?;
        mark_running().context("report service running after named-pipe bind")?;
        append_service_startup_diagnostic(
            "running",
            "service reported Running after control-pipe bind",
        );

        start_runtime_tasks(&runtime, options).await;
        append_service_startup_diagnostic(
            "runtime_tasks_started",
            "background runtime tasks started",
        );
        append_service_startup_diagnostic(
            "runtime_tasks_health_started",
            &runtime_task_health_json(&runtime),
        );

        let shutdown_monitor = shutdown_rx.clone();
        let serve = Server::builder()
            .add_service(control_plane)
            .serve_with_incoming_shutdown(incoming, wait_for_shutdown_request(shutdown_rx));
        tokio::pin!(serve);
        let result = tokio::select! {
            result = &mut serve => result.context("gRPC named-pipe service failure"),
            _ = wait_for_shutdown_request(shutdown_monitor) => {
                append_service_startup_diagnostic(
                    "serve_shutdown_requested",
                    "shutdown requested; waiting briefly for gRPC control-plane drain",
                );
                match time::timeout(SERVICE_CONTROL_PLANE_STOP_GRACE, &mut serve).await {
                    Ok(result) => result.context("gRPC named-pipe service failure"),
                    Err(_) => {
                        append_service_startup_diagnostic(
                            "serve_shutdown_timeout",
                            "gRPC control-plane drain exceeded stop grace; aborting service runtime",
                        );
                        Ok(())
                    }
                }
            }
        };
        match &result {
            Ok(()) => append_service_startup_diagnostic("serve_exited", "gRPC service exited"),
            Err(error) => append_service_startup_diagnostic("serve_error", &format!("{error:#}")),
        }
        append_service_startup_diagnostic(
            "runtime_tasks_health_before_shutdown",
            &runtime_task_health_json(&runtime),
        );

        match time::timeout(SERVICE_RUNTIME_TASK_STOP_GRACE, shutdown_runtime(&runtime)).await {
            Ok(()) => append_service_startup_diagnostic(
                "runtime_tasks_stopped",
                "background runtime tasks stopped",
            ),
            Err(_) => append_service_startup_diagnostic(
                "runtime_tasks_shutdown_timeout",
                "runtime task shutdown exceeded stop grace; service process will exit",
            ),
        }
        append_service_startup_diagnostic(
            "runtime_tasks_health_stopped",
            &runtime_task_health_json(&runtime),
        );

        result
    }

    async fn wait_for_shutdown_request(mut shutdown_rx: watch::Receiver<bool>) {
        loop {
            if *shutdown_rx.borrow_and_update() {
                break;
            }
            if shutdown_rx.changed().await.is_err() {
                break;
            }
        }
    }

    fn startup_arguments(service_arguments: Vec<OsString>) -> Vec<OsString> {
        service_arguments
            .into_iter()
            .chain(env::args_os().skip(1))
            .collect()
    }

    fn parse_allowed_user_sid(arguments: Vec<OsString>) -> Result<String> {
        for argument in arguments {
            let Some(value) = argument.to_str() else {
                continue;
            };
            if let Some(sid) = value.strip_prefix("--allowed-user-sid=") {
                if validate_allowed_user_sid_shape(sid) {
                    return Ok(sid.to_string());
                }
                anyhow::bail!("service allowed user SID argument was invalid");
            }
        }

        anyhow::bail!(
            "service missing --allowed-user-sid argument; reinstall the service with current boundlessctl"
        )
    }

    fn service_status(current_state: ServiceState) -> ServiceStatus {
        service_status_with_exit(current_state, 0)
    }

    fn service_status_with_exit(
        current_state: ServiceState,
        service_exit_code: u32,
    ) -> ServiceStatus {
        let controls_accepted = match current_state {
            ServiceState::Running => ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            _ => ServiceControlAccept::empty(),
        };
        let exit_code = if service_exit_code == 0 {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::ServiceSpecific(service_exit_code)
        };

        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state,
            controls_accepted,
            exit_code,
            checkpoint: 0,
            wait_hint: service_wait_hint(current_state),
            process_id: None,
        }
    }

    fn service_wait_hint(current_state: ServiceState) -> Duration {
        match current_state {
            ServiceState::StartPending => SERVICE_START_WAIT_HINT,
            ServiceState::StopPending => SERVICE_STOP_WAIT_HINT,
            _ => Duration::ZERO,
        }
    }

    fn install_service_panic_hook() {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            append_service_startup_diagnostic("panic", &format!("{info}"));
            previous(info);
        }));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_allowed_user_sid_accepts_expected_argument() {
            let sid = parse_allowed_user_sid(vec![
                OsString::from("ignored"),
                OsString::from("--allowed-user-sid=S-1-5-21-1-2-3-1001"),
            ])
            .expect("sid");

            assert_eq!(sid, "S-1-5-21-1-2-3-1001");
        }

        #[test]
        fn parse_allowed_user_sid_rejects_sddl_injection() {
            let err = parse_allowed_user_sid(vec![OsString::from(
                "--allowed-user-sid=S-1-5-21-1);(A;;GA;;;WD",
            )])
            .expect_err("must reject");

            assert!(
                err.to_string()
                    .contains("service allowed user SID argument was invalid")
            );
        }

        #[test]
        fn parse_allowed_user_sid_rejects_non_numeric_shape() {
            let err =
                parse_allowed_user_sid(vec![OsString::from("--allowed-user-sid=S-1-not-a-sid")])
                    .expect_err("must reject");

            assert!(
                err.to_string()
                    .contains("service allowed user SID argument was invalid")
            );
        }

        #[test]
        fn parse_allowed_user_sid_rejects_empty_segment() {
            let err = parse_allowed_user_sid(vec![OsString::from("--allowed-user-sid=S-1-5--21")])
                .expect_err("must reject");

            assert!(
                err.to_string()
                    .contains("service allowed user SID argument was invalid")
            );
        }

        #[test]
        fn parse_allowed_user_sid_rejects_surrounding_whitespace() {
            let err =
                parse_allowed_user_sid(vec![OsString::from("--allowed-user-sid= S-1-5-21-1")])
                    .expect_err("must reject");

            assert!(
                err.to_string()
                    .contains("service allowed user SID argument was invalid")
            );
        }

        #[test]
        fn stop_pending_status_uses_short_wait_hint_and_accepts_no_controls() {
            let status = service_status(ServiceState::StopPending);

            assert_eq!(status.current_state, ServiceState::StopPending);
            assert_eq!(status.controls_accepted, ServiceControlAccept::empty());
            assert_eq!(status.wait_hint, SERVICE_STOP_WAIT_HINT);
        }
    }
}
