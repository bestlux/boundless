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
        fs::{self, OpenOptions},
        io::{self, Write},
        path::PathBuf,
        time::Duration,
    };

    use anyhow::{Context, Result};
    use tokio::sync::watch;
    use tonic::transport::Server;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    use boundless_daemon::{
        config::ApiTransport,
        host::{
            HostOverrides, HostRuntimeOptions, prepare_runtime_with_options, start_runtime_tasks,
        },
        input::InputRuntimeMode,
        logging, shared_control_plane_app,
    };
    use platform_windows::runtime::{
        named_pipe_incoming_for_allowed_user, validate_allowed_user_sid_shape,
    };

    const SERVICE_NAME: &str = "BoundlessService";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn service_main(arguments: Vec<OsString>) {
        if let Err(error) = run_service(startup_arguments(arguments)) {
            append_service_startup_diagnostic("service_main_failed", &format!("{error:#}"));
            tracing::error!(%error, "boundless service failed");
        }
    }

    fn run_service(arguments: Vec<OsString>) -> windows_service::Result<()> {
        let _logging = logging::init_logging().ok();
        append_service_startup_diagnostic("starting", "boundless service entrypoint reached");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(true);
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
        status_handle.set_service_status(service_status(ServiceState::StartPending))?;
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
            let result = run_daemon(shutdown_rx, allowed_user_sid, || {
                status_handle.set_service_status(service_status(ServiceState::Running))
            })
            .await;
            status_handle.set_service_status(service_status(ServiceState::StopPending))?;
            result
        });

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

        Ok(())
    }

    async fn run_daemon(
        mut shutdown_rx: watch::Receiver<bool>,
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

        Server::builder()
            .add_service(control_plane)
            .serve_with_incoming_shutdown(incoming, async move {
                while !*shutdown_rx.borrow() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .context("gRPC named-pipe service failure")?;

        Ok(())
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
            wait_hint: Duration::from_secs(10),
            process_id: None,
        }
    }

    fn append_service_startup_diagnostic(stage: &str, detail: &str) {
        let log_dir = service_diagnostic_log_dir();
        if fs::create_dir_all(&log_dir).is_err() {
            return;
        }
        let path = log_dir.join("boundless-service-startup.log");
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = writeln!(
            file,
            "{} stage={} pid={} detail={}",
            chrono::Utc::now().to_rfc3339(),
            stage,
            std::process::id(),
            detail.replace(['\r', '\n'], " ")
        );
    }

    fn service_diagnostic_log_dir() -> PathBuf {
        let root = env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        root.join("Boundless").join("logs")
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
    }
}
