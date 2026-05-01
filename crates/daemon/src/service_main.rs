#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    service_entry::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("boundless-service is only supported on Windows");
}

#[cfg(windows)]
mod service_entry {
    use std::{ffi::OsString, io, time::Duration};

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
        host::{HostOverrides, run_with},
        logging, shared_control_plane_app,
    };
    use platform_windows::runtime::named_pipe_incoming;

    const SERVICE_NAME: &str = "BoundlessService";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            tracing::error!(%error, "boundless service failed");
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let _logging = logging::init_logging().ok();
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

        let runtime = tokio::runtime::Runtime::new().map_err(windows_service::Error::Winapi)?;
        let result = runtime.block_on(async move {
            status_handle.set_service_status(service_status(ServiceState::Running))?;
            let result = run_daemon(shutdown_rx).await;
            status_handle.set_service_status(service_status(ServiceState::StopPending))?;
            result
        });

        if let Err(error) = result {
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

    async fn run_daemon(mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        run_with(
            HostOverrides {
                bind: None,
                api_transport: Some(ApiTransport::NamedPipe),
                api_pipe_name: None,
                network_port: None,
            },
            |runtime| async move {
                let control_plane = adapter_ipc_grpc::ControlPlaneApi::new(
                    shared_control_plane_app(runtime.state.clone()),
                )
                .into_server();

                let incoming =
                    named_pipe_incoming(&runtime.snapshot.api_pipe_name).with_context(|| {
                        format!("initialize named pipe {}", runtime.snapshot.api_pipe_name)
                    })?;

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
            },
        )
        .await
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
}
