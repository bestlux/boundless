use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};

use windows_service::{
    Error as WindowsServiceError,
    service::{ServiceAccess, ServiceState},
    service_manager::{ServiceManager, ServiceManagerAccess},
};
use windows_sys::Win32::{
    Foundation::{ERROR_CANCELLED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{GetExitCodeProcess, WaitForSingleObject},
    UI::{
        Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
    },
};

const ERROR_ACCESS_DENIED_CODE: i32 = 5;
const ERROR_SERVICE_ALREADY_RUNNING_CODE: i32 = 1056;
const ERROR_SERVICE_DOES_NOT_EXIST_CODE: i32 = 1060;
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(15);
const ELEVATED_HELPER_TIMEOUT_MS: u32 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceRecoveryOffer {
    state: String,
    message: String,
    action_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BoundlessServiceState {
    Missing,
    Running,
    StartPending,
    Stopped,
    Installed { state: String },
    QueryFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceStartRequest {
    Requested,
    AlreadyInProgress,
    NeedsElevation,
}

fn windows_service_error_code(error: &WindowsServiceError) -> Option<i32> {
    match error {
        WindowsServiceError::Winapi(error) => error.raw_os_error(),
        _ => None,
    }
}

fn describe_windows_service_error(context: &str, error: &WindowsServiceError) -> String {
    match error {
        WindowsServiceError::Winapi(error) => format!("{context}: {error}"),
        _ => format!("{context}: {error}"),
    }
}

fn query_boundless_service_state() -> BoundlessServiceState {
    let manager = match ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    ) {
        Ok(manager) => manager,
        Err(error) => {
            return BoundlessServiceState::QueryFailed(describe_windows_service_error(
                "open Windows Service Control Manager",
                &error,
            ));
        }
    };

    let service = match manager.open_service(BOUNDLESS_SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => service,
        Err(error)
            if windows_service_error_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST_CODE) =>
        {
            return BoundlessServiceState::Missing;
        }
        Err(error) => {
            return BoundlessServiceState::QueryFailed(describe_windows_service_error(
                "open BoundlessService for status query",
                &error,
            ));
        }
    };

    match service.query_status() {
        Ok(status) => map_service_state(status.current_state),
        Err(error) => BoundlessServiceState::QueryFailed(describe_windows_service_error(
            "query BoundlessService status",
            &error,
        )),
    }
}

fn map_service_state(state: ServiceState) -> BoundlessServiceState {
    match state {
        ServiceState::Running => BoundlessServiceState::Running,
        ServiceState::StartPending => BoundlessServiceState::StartPending,
        ServiceState::Stopped => BoundlessServiceState::Stopped,
        other => BoundlessServiceState::Installed {
            state: format!("{other:?}"),
        },
    }
}

fn boundless_service_recovery_offer(endpoint: &str) -> Option<ServiceRecoveryOffer> {
    if !is_named_pipe_endpoint(endpoint) {
        return None;
    }

    service_recovery_offer_for_state(&query_boundless_service_state())
}

fn service_recovery_offer_for_state(
    state: &BoundlessServiceState,
) -> Option<ServiceRecoveryOffer> {
    match state {
        BoundlessServiceState::Stopped => Some(ServiceRecoveryOffer {
            state: "Stopped".to_string(),
            message: "BoundlessService is installed but stopped. Start the service to reconnect the dashboard; Windows may ask for permission.".to_string(),
            action_label: "Start service".to_string(),
        }),
        BoundlessServiceState::StartPending => Some(ServiceRecoveryOffer {
            state: "StartPending".to_string(),
            message: "BoundlessService is still starting. Wait for the service and its control pipe without launching another daemon.".to_string(),
            action_label: "Finish startup".to_string(),
        }),
        _ => None,
    }
}

fn request_boundless_service_start() -> Result<ServiceStartRequest> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| {
            anyhow::anyhow!(describe_windows_service_error(
                "open Windows Service Control Manager",
                &error,
            ))
        })?;
    let service = match manager.open_service(
        BOUNDLESS_SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(error) if windows_service_error_code(&error) == Some(ERROR_ACCESS_DENIED_CODE) => {
            return classify_start_access_denied();
        }
        Err(error) => {
            bail!(describe_windows_service_error(
                "open BoundlessService for startup",
                &error,
            ));
        }
    };

    let status = service.query_status().map_err(|error| {
        anyhow::anyhow!(describe_windows_service_error(
            "query BoundlessService before startup",
            &error,
        ))
    })?;
    match status.current_state {
        ServiceState::Running | ServiceState::StartPending => {
            return Ok(ServiceStartRequest::AlreadyInProgress);
        }
        ServiceState::Stopped => {}
        other => {
            bail!(
                "BoundlessService cannot be started while its SCM state is {other:?}; wait for that transition to finish, then retry"
            );
        }
    }

    match service.start::<&str>(&[]) {
        Ok(()) => Ok(ServiceStartRequest::Requested),
        Err(error)
            if windows_service_error_code(&error) == Some(ERROR_SERVICE_ALREADY_RUNNING_CODE) =>
        {
            Ok(ServiceStartRequest::AlreadyInProgress)
        }
        Err(error) if windows_service_error_code(&error) == Some(ERROR_ACCESS_DENIED_CODE) => {
            classify_start_access_denied()
        }
        Err(error) => bail!(describe_windows_service_error(
            "request BoundlessService startup",
            &error,
        )),
    }
}

fn classify_start_access_denied() -> Result<ServiceStartRequest> {
    match query_boundless_service_state() {
        BoundlessServiceState::Running | BoundlessServiceState::StartPending => {
            Ok(ServiceStartRequest::AlreadyInProgress)
        }
        BoundlessServiceState::Stopped => Ok(ServiceStartRequest::NeedsElevation),
        BoundlessServiceState::Missing => {
            bail!("BoundlessService was removed before startup could be requested")
        }
        BoundlessServiceState::Installed { state } => {
            bail!("BoundlessService entered SCM state {state} before startup could be requested")
        }
        BoundlessServiceState::QueryFailed(error) => {
            bail!("SCM denied startup access and service status could not be rechecked: {error}")
        }
    }
}

fn start_boundless_service_elevated_entrypoint() -> Result<()> {
    match request_boundless_service_start()? {
        ServiceStartRequest::Requested | ServiceStartRequest::AlreadyInProgress => {
            wait_for_service_running_blocking(SERVICE_START_TIMEOUT)
        }
        ServiceStartRequest::NeedsElevation => {
            bail!("elevated Boundless service-start helper still lacks SCM start permission")
        }
    }
}

fn wait_for_service_running_blocking(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match query_boundless_service_state() {
            BoundlessServiceState::Running => return Ok(()),
            BoundlessServiceState::StartPending => {}
            BoundlessServiceState::Stopped => {
                bail!(
                    "BoundlessService returned to Stopped before startup completed; check the Windows Application log or repair Boundless"
                );
            }
            BoundlessServiceState::Missing => {
                bail!("BoundlessService was removed while startup was in progress")
            }
            BoundlessServiceState::Installed { state } => {
                bail!("BoundlessService entered unexpected SCM state {state} during startup")
            }
            BoundlessServiceState::QueryFailed(error) => {
                bail!("could not verify BoundlessService startup: {error}")
            }
        }

        if Instant::now() >= deadline {
            bail!(
                "BoundlessService did not reach Running within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn launch_elevated_service_start_helper() -> Result<()> {
    let executable = std::env::current_exe().context("resolve current Boundless tray executable")?;
    let verb = wide_null("runas");
    let executable = os_wide_null(executable.as_os_str());
    let parameters = wide_null("--start-service-elevated");
    let mut execute_info = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
    execute_info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    execute_info.fMask = SEE_MASK_NOCLOSEPROCESS;
    execute_info.lpVerb = verb.as_ptr();
    execute_info.lpFile = executable.as_ptr();
    execute_info.lpParameters = parameters.as_ptr();
    execute_info.nShow = SW_HIDE;

    if unsafe { ShellExecuteExW(&mut execute_info) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            bail!("Windows permission prompt was canceled; BoundlessService remains stopped")
        }
        return Err(error).context("request elevated BoundlessService startup");
    }
    if execute_info.hProcess.is_null() {
        bail!("Windows accepted the service-start request but returned no helper process handle")
    }
    let process = OwnedProcessHandle(execute_info.hProcess);

    match unsafe { WaitForSingleObject(process.0, ELEVATED_HELPER_TIMEOUT_MS) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            bail!(
                "elevated Boundless service-start helper did not finish within {} seconds",
                ELEVATED_HELPER_TIMEOUT_MS / 1_000
            );
        }
        _ => return Err(std::io::Error::last_os_error()).context("wait for elevated service-start helper"),
    }

    let mut exit_code = u32::MAX;
    if unsafe { GetExitCodeProcess(process.0, &mut exit_code) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("read elevated service-start helper exit code");
    }
    if exit_code != 0 {
        bail!(
            "elevated Boundless service-start helper failed with exit code {exit_code}; check BoundlessService in Windows Services or repair Boundless"
        );
    }
    Ok(())
}

async fn recover_boundless_service(endpoint: &str) -> Result<String> {
    match query_boundless_service_state() {
        BoundlessServiceState::Missing => {
            bail!("BoundlessService is not installed; repair or reinstall Boundless")
        }
        BoundlessServiceState::Running | BoundlessServiceState::StartPending => {}
        BoundlessServiceState::Stopped => {
            if request_boundless_service_start()? == ServiceStartRequest::NeedsElevation {
                launch_elevated_service_start_helper()?;
            }
        }
        BoundlessServiceState::Installed { state } => {
            bail!(
                "BoundlessService is in SCM state {state}; wait for that transition to finish, then retry"
            )
        }
        BoundlessServiceState::QueryFailed(error) => {
            bail!("could not query BoundlessService before startup: {error}")
        }
    }

    wait_for_boundless_service_and_pipe(endpoint, SERVICE_START_TIMEOUT).await?;
    Ok(format!(
        "{BOUNDLESS_SERVICE_NAME} is running and the Boundless control pipe is ready"
    ))
}

fn recover_boundless_service_blocking(endpoint: &str) -> Result<String> {
    block_on_result(recover_boundless_service(endpoint))
}

async fn wait_for_boundless_service_and_pipe(endpoint: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_pipe_error = String::new();

    loop {
        match query_boundless_service_state() {
            BoundlessServiceState::Running => match channel(endpoint).await {
                Ok(_) => return Ok(()),
                Err(error) => last_pipe_error = error.to_string(),
            },
            BoundlessServiceState::StartPending => {}
            BoundlessServiceState::Stopped => {
                bail!(
                    "BoundlessService returned to Stopped before its control pipe became ready; check the Windows Application log or repair Boundless"
                )
            }
            BoundlessServiceState::Missing => {
                bail!("BoundlessService was removed while startup was in progress")
            }
            BoundlessServiceState::Installed { state } => {
                bail!("BoundlessService entered unexpected SCM state {state} during startup")
            }
            BoundlessServiceState::QueryFailed(error) => {
                bail!("could not verify BoundlessService startup: {error}")
            }
        }

        if Instant::now() >= deadline {
            let pipe_detail = if last_pipe_error.is_empty() {
                "the service never reached Running".to_string()
            } else {
                format!("last control-pipe error: {last_pipe_error}")
            };
            bail!(
                "BoundlessService did not become ready at {endpoint} within {} seconds ({pipe_detail})",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn os_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

struct OwnedProcessHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
            self.0 = ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod service_recovery_tests {
    use super::*;

    #[test]
    fn service_states_map_to_recovery_boundaries() {
        assert_eq!(
            map_service_state(ServiceState::Running),
            BoundlessServiceState::Running
        );
        assert_eq!(
            map_service_state(ServiceState::StartPending),
            BoundlessServiceState::StartPending
        );
        assert_eq!(
            map_service_state(ServiceState::Stopped),
            BoundlessServiceState::Stopped
        );
        assert_eq!(
            map_service_state(ServiceState::StopPending),
            BoundlessServiceState::Installed {
                state: "StopPending".to_string()
            }
        );
    }

    #[test]
    fn recovery_offers_are_explicit_and_do_not_name_a_per_user_daemon() {
        let stopped = service_recovery_offer_for_state(&BoundlessServiceState::Stopped)
            .expect("stopped service offer");
        assert_eq!(stopped.action_label, "Start service");
        assert!(!stopped.message.contains("boundlessd"));

        let pending = service_recovery_offer_for_state(&BoundlessServiceState::StartPending)
            .expect("start-pending service offer");
        assert_eq!(pending.action_label, "Finish startup");
        assert!(!pending.message.contains("boundlessd"));

        assert!(service_recovery_offer_for_state(&BoundlessServiceState::Running).is_none());
        assert!(service_recovery_offer_for_state(&BoundlessServiceState::Missing).is_none());
    }
}
