#[cfg(windows)]
use std::{
    ffi::c_void,
    io, mem,
    pin::Pin,
    ptr,
    sync::mpsc::{self as std_mpsc, SyncSender},
    task::{Context as TaskContext, Poll},
    thread::{self, JoinHandle},
};

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;

#[cfg(windows)]
use ipc_api::client_identity::ControlClientIdentity;
#[cfg(windows)]
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    sync::mpsc,
};
#[cfg(windows)]
use tonic::{codegen::tokio_stream::Stream, transport::server::Connected};
#[cfg(windows)]
use windows_sys::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::{GetNamedPipeClientProcessId, GetNamedPipeClientSessionId},
            Power::{
                ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, GetSystemPowerStatus,
                SYSTEM_POWER_STATUS, SetThreadExecutionState,
            },
            Shutdown::LockWorkStation,
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    },
    core::PCWSTR,
};

#[cfg(windows)]
#[derive(Debug)]
pub struct NamedPipeIncoming {
    receiver: mpsc::Receiver<io::Result<NamedPipeIo>>,
}

#[cfg(windows)]
impl Stream for NamedPipeIncoming {
    type Item = io::Result<NamedPipeIo>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_recv(cx)
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct NamedPipeIo {
    inner: NamedPipeServer,
    client_identity: ControlClientIdentity,
}

#[cfg(windows)]
impl Connected for NamedPipeIo {
    type ConnectInfo = ControlClientIdentity;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.client_identity.clone()
    }
}

#[cfg(windows)]
impl AsyncRead for NamedPipeIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl AsyncWrite for NamedPipeIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(windows)]
pub fn named_pipe_incoming(pipe_name: &str) -> io::Result<NamedPipeIncoming> {
    let allowed_user_sid = current_user_sid_string()?;
    named_pipe_incoming_for_allowed_user(pipe_name, &allowed_user_sid)
}

#[cfg(windows)]
pub fn named_pipe_incoming_for_allowed_user(
    pipe_name: &str,
    allowed_user_sid: &str,
) -> io::Result<NamedPipeIncoming> {
    let pipe_path = pipe_path_for_name(pipe_name)?;
    let (sender, receiver) = mpsc::channel(32);
    let security_sddl = control_pipe_sddl_for_allowed_user(allowed_user_sid)?;
    let security_descriptor = PipeSecurityDescriptor::from_sddl(&security_sddl)?;
    let first_server = create_server(&pipe_path, true, &security_descriptor)?;

    tokio::spawn(async move {
        accept_loop(pipe_path, first_server, sender, security_sddl).await;
    });

    Ok(NamedPipeIncoming { receiver })
}

pub fn validate_allowed_user_sid_shape(allowed_user_sid: &str) -> bool {
    let sid = allowed_user_sid.trim();
    if sid != allowed_user_sid || sid.is_empty() {
        return false;
    }

    let mut parts = sid.split('-');
    if parts.next() != Some("S") || parts.next() != Some("1") {
        return false;
    }

    let Some(identifier_authority) = parts.next() else {
        return false;
    };
    if identifier_authority.is_empty()
        || !identifier_authority
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return false;
    }

    let mut sub_authority_count = 0;
    for part in parts {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        sub_authority_count += 1;
    }

    sub_authority_count > 0
}

#[cfg(windows)]
pub fn current_user_sid_string() -> io::Result<String> {
    process_handle_user_sid_string(unsafe { GetCurrentProcess() })
}

#[cfg(windows)]
fn process_handle_user_sid_string(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let _token_guard = HandleGuard(token);

    let mut required_len = 0_u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required_len) };
    if required_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0_u8; required_len as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            required_len,
            &mut required_len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    sid_to_string(token_user.User.Sid)
}

#[cfg(windows)]
pub fn process_id_user_sid_string(process_id: u32) -> io::Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _process_guard = HandleGuard(process);
    process_handle_user_sid_string(process)
}

/// Resolves the connected pipe client's account SID and Windows session id
/// from the pipe handle itself. Fields stay `None` when a query fails, so
/// identity-gated handlers (input broker attach/exchange) fail closed instead
/// of trusting anything the client reports about itself.
#[cfg(windows)]
fn named_pipe_client_identity(server: &NamedPipeServer) -> ControlClientIdentity {
    use std::os::windows::io::AsRawHandle;

    let handle = server.as_raw_handle() as HANDLE;
    let mut identity = ControlClientIdentity::default();

    let mut session_id = 0_u32;
    if unsafe { GetNamedPipeClientSessionId(handle, &mut session_id) } != 0 {
        identity.session_id = Some(session_id);
    } else {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "failed to resolve named-pipe client session id"
        );
    }

    let mut process_id = 0_u32;
    if unsafe { GetNamedPipeClientProcessId(handle, &mut process_id) } != 0 {
        identity.process_id = Some(process_id);
        match process_id_user_sid_string(process_id) {
            Ok(user_sid) => identity.user_sid = Some(user_sid),
            Err(error) => {
                tracing::warn!(%error, "failed to resolve named-pipe client user SID");
            }
        }
    } else {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "failed to resolve named-pipe client process id"
        );
    }

    identity
}

#[cfg(windows)]
pub fn control_pipe_sddl_for_allowed_user(allowed_user_sid: &str) -> io::Result<String> {
    if !validate_allowed_user_sid_shape(allowed_user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "allowed user SID must be a valid SID string",
        ));
    }

    Ok(format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{allowed_user_sid})"
    ))
}

#[cfg(windows)]
fn sid_to_string(sid: *mut c_void) -> io::Result<String> {
    let mut string_sid = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut string_sid) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid_guard = LocalAllocGuard(string_sid.cast::<c_void>());
    wide_ptr_to_string(string_sid as PCWSTR)
}

#[cfg(windows)]
pub fn lock_workstation() -> Result<()> {
    let ok = unsafe { LockWorkStation() };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("LockWorkStation");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn lock_workstation() -> Result<()> {
    anyhow::bail!("lock_machine is only supported on Windows");
}

#[cfg(windows)]
#[derive(Debug)]
pub struct AntiIdlePowerWorker {
    sender: SyncSender<PowerWorkerCommand>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(windows)]
#[derive(Debug)]
enum PowerWorkerCommand {
    Apply(u32),
    Shutdown,
}

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct AntiIdlePowerWorker;

#[cfg(windows)]
impl AntiIdlePowerWorker {
    pub fn apply(&mut self, flags: u32) -> Result<()> {
        self.sender
            .send(PowerWorkerCommand::Apply(flags))
            .context("send anti-idle worker command")
    }
}

#[cfg(not(windows))]
impl AntiIdlePowerWorker {
    pub fn apply(&mut self, _flags: u32) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for AntiIdlePowerWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(PowerWorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
pub fn anti_idle_power_supported() -> bool {
    true
}

#[cfg(not(windows))]
pub fn anti_idle_power_supported() -> bool {
    false
}

#[cfg(windows)]
pub fn anti_idle_execution_state_flags(active: bool, display_required: bool) -> u32 {
    if !active {
        return ES_CONTINUOUS;
    }

    let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
    if display_required {
        flags |= ES_DISPLAY_REQUIRED;
    }
    flags
}

#[cfg(not(windows))]
pub fn anti_idle_execution_state_flags(_active: bool, _display_required: bool) -> u32 {
    0
}

#[cfg(windows)]
pub fn anti_idle_system_on_ac_power() -> Result<bool> {
    let mut status = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut status as *mut SYSTEM_POWER_STATUS) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("GetSystemPowerStatus");
    }

    Ok(status.ACLineStatus == 1)
}

#[cfg(not(windows))]
pub fn anti_idle_system_on_ac_power() -> Result<bool> {
    Ok(true)
}

#[cfg(windows)]
pub fn spawn_anti_idle_power_worker() -> Result<AntiIdlePowerWorker> {
    let (sender, receiver) = std_mpsc::sync_channel::<PowerWorkerCommand>(8);
    let thread = thread::spawn(move || {
        let mut last_flags = u32::MAX;
        while let Ok(command) = receiver.recv() {
            match command {
                PowerWorkerCommand::Apply(flags) => {
                    if flags == last_flags {
                        continue;
                    }
                    let result = unsafe { SetThreadExecutionState(flags) };
                    if result == 0 {
                        tracing::warn!(
                            flags,
                            error = %std::io::Error::last_os_error(),
                            "SetThreadExecutionState failed"
                        );
                        continue;
                    }
                    last_flags = flags;
                }
                PowerWorkerCommand::Shutdown => {
                    let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                    break;
                }
            }
        }
    });

    Ok(AntiIdlePowerWorker {
        sender,
        thread: Some(thread),
    })
}

#[cfg(not(windows))]
pub fn spawn_anti_idle_power_worker() -> Result<AntiIdlePowerWorker> {
    Ok(AntiIdlePowerWorker)
}

pub fn validate_pipe_name(pipe_name: &str) -> Result<()> {
    let trimmed = pipe_name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("pipe name must not be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        anyhow::bail!("pipe name must not contain path separators");
    }
    Ok(())
}

#[cfg(windows)]
async fn accept_loop(
    pipe_path: String,
    mut server: NamedPipeServer,
    sender: mpsc::Sender<io::Result<NamedPipeIo>>,
    security_sddl: String,
) {
    loop {
        if let Err(error) = server.connect().await {
            let _ = sender.send(Err(error)).await;
            break;
        }

        let next_server_result = (|| {
            let security_descriptor = PipeSecurityDescriptor::from_sddl(&security_sddl)?;
            create_server(&pipe_path, false, &security_descriptor)
        })();
        let next_server = match next_server_result {
            Ok(next) => next,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                break;
            }
        };

        let io = NamedPipeIo {
            client_identity: named_pipe_client_identity(&server),
            inner: server,
        };
        if sender.send(Ok(io)).await.is_err() {
            break;
        }

        server = next_server;
    }
}

#[cfg(windows)]
fn create_server(
    pipe_path: &str,
    first_instance: bool,
    security_descriptor: &PipeSecurityDescriptor,
) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    if first_instance {
        options.first_pipe_instance(true);
    }
    let mut attributes = security_descriptor.attributes();
    unsafe {
        options.create_with_security_attributes_raw(
            pipe_path,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

#[cfg(windows)]
fn pipe_path_for_name(pipe_name: &str) -> io::Result<String> {
    validate_pipe_name(pipe_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let trimmed = pipe_name.trim();

    Ok(format!(r"\\.\pipe\{trimmed}"))
}

#[cfg(windows)]
struct PipeSecurityDescriptor {
    security_descriptor: PSECURITY_DESCRIPTOR,
}

#[cfg(windows)]
impl PipeSecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let mut security_descriptor = std::ptr::null_mut();
        let sddl_wide = sddl
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_wide.as_ptr(),
                SDDL_REVISION_1,
                &mut security_descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            security_descriptor,
        })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.security_descriptor,
            bInheritHandle: 0,
        }
    }
}

#[cfg(windows)]
impl Drop for PipeSecurityDescriptor {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                let _ = LocalFree(self.security_descriptor);
            }
        }
    }
}

#[cfg(windows)]
struct HandleGuard(HANDLE);

#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
struct LocalAllocGuard(*mut c_void);

#[cfg(windows)]
impl Drop for LocalAllocGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn wide_ptr_to_string(value: PCWSTR) -> io::Result<String> {
    if value.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows API returned a null string pointer",
        ));
    }

    let mut len = 0_usize;
    unsafe {
        while *value.add(len) != 0 {
            len += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(value, len))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anti_idle_flags_clear_to_continuous_when_inactive() {
        #[cfg(windows)]
        assert_eq!(anti_idle_execution_state_flags(false, false), ES_CONTINUOUS);

        #[cfg(not(windows))]
        assert_eq!(anti_idle_execution_state_flags(false, false), 0);
    }

    #[test]
    fn anti_idle_flags_include_display_when_requested() {
        #[cfg(windows)]
        {
            let flags = anti_idle_execution_state_flags(true, true);
            assert_eq!(flags & ES_SYSTEM_REQUIRED, ES_SYSTEM_REQUIRED);
            assert_eq!(flags & ES_DISPLAY_REQUIRED, ES_DISPLAY_REQUIRED);
            assert_eq!(flags & ES_CONTINUOUS, ES_CONTINUOUS);
        }

        #[cfg(not(windows))]
        assert_eq!(anti_idle_execution_state_flags(true, true), 0);
    }

    #[test]
    fn allowed_user_sid_shape_accepts_numeric_sid() {
        assert!(validate_allowed_user_sid_shape("S-1-5-21-1-2-3-1001"));
        assert!(validate_allowed_user_sid_shape("S-1-5-18"));
    }

    #[test]
    fn allowed_user_sid_shape_rejects_malformed_values() {
        for sid in [
            "",
            " S-1-5-21-1",
            "S-1-5-21-1 ",
            "S-1-not-a-sid",
            "S-1-5-21-1);(A;;GA;;;WD",
            "S-1-5--21",
            "S-1-5-21-",
            "S-1-5-21-abc",
            "S-2-5-21-1",
            "S-1-5",
        ] {
            assert!(
                !validate_allowed_user_sid_shape(sid),
                "expected invalid SID shape to be rejected: {sid}"
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn control_pipe_sddl_allows_only_system_admins_and_expected_user() {
        let sddl = control_pipe_sddl_for_allowed_user("S-1-5-21-1-2-3-1001").expect("sddl");
        assert_eq!(
            sddl,
            "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    #[cfg(windows)]
    fn control_pipe_sddl_rejects_sddl_injection() {
        let err =
            control_pipe_sddl_for_allowed_user("S-1-5-21-1);(A;;GA;;;WD").expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    #[cfg(windows)]
    fn control_pipe_sddl_rejects_non_numeric_sid_shape() {
        let err = control_pipe_sddl_for_allowed_user("S-1-not-a-sid").expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
