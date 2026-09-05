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
            RemoteDesktop::WTSGetActiveConsoleSessionId,
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
    service_user: Option<String>,
    administrative_client: bool,
}

#[cfg(windows)]
impl NamedPipeIo {
    fn authorize(&self) -> io::Result<()> {
        if let Some(allowed_sid) = &self.service_user
            && !self.administrative_client
            && !console_client_authorized(
                &self.client_identity,
                allowed_sid,
                active_console_session_id(),
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "configured desktop user is not in the active console session",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn console_client_authorized(
    identity: &ControlClientIdentity,
    allowed_sid: &str,
    console: Option<u32>,
) -> bool {
    identity.user_sid.as_deref() == Some(allowed_sid)
        && identity
            .session_id
            .is_some_and(|session| session != 0 && Some(session) == console)
}

#[cfg(windows)]
pub fn active_console_session_id() -> Option<u32> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != u32::MAX && session != 0).then_some(session)
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
        if let Err(error) = self.authorize() {
            return Poll::Ready(Err(error));
        }
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
        if let Err(error) = self.authorize() {
            return Poll::Ready(Err(error));
        }
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
    named_pipe_incoming_with_policy(pipe_name, allowed_user_sid, false)
}

/// Service-only control endpoint: recheck console-session access on subsequent
/// reads and writes, not just when an HTTP/2 connection was first accepted.
#[cfg(windows)]
pub fn named_pipe_incoming_for_service_user(
    pipe_name: &str,
    allowed_user_sid: &str,
) -> io::Result<NamedPipeIncoming> {
    named_pipe_incoming_with_policy(pipe_name, allowed_user_sid, true)
}

#[cfg(windows)]
fn named_pipe_incoming_with_policy(
    pipe_name: &str,
    allowed_user_sid: &str,
    console_only: bool,
) -> io::Result<NamedPipeIncoming> {
    let pipe_path = pipe_path_for_name(pipe_name)?;
    let (sender, receiver) = mpsc::channel(32);
    let privileged_server =
        console_only || process_handle_is_administrative(unsafe { GetCurrentProcess() })?;
    let security_sddl = control_pipe_sddl_for_server(allowed_user_sid, privileged_server)?;
    let security_descriptor = PipeSecurityDescriptor::from_sddl(&security_sddl)?;
    let first_server = create_server(&pipe_path, true, &security_descriptor)?;

    let service_user = console_only.then(|| allowed_user_sid.to_string());
    tokio::spawn(async move {
        accept_loop(pipe_path, first_server, sender, security_sddl, service_user).await;
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
pub(crate) fn process_handle_user_sid_string(process: HANDLE) -> io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let _token_guard = HandleGuard(token);

    token_user_sid_string(token)
}

#[cfg(windows)]
pub(crate) fn token_user_sid_string(token: HANDLE) -> io::Result<String> {
    let mut required_len = 0_u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required_len) };
    if required_len == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0usize; (required_len as usize).div_ceil(mem::size_of::<usize>())];
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
fn named_pipe_client_identity(server: &NamedPipeServer) -> (ControlClientIdentity, bool) {
    use std::os::windows::io::AsRawHandle;

    let handle = server.as_raw_handle() as HANDLE;
    let mut identity = ControlClientIdentity::default();
    let mut administrative = false;

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
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        if !process.is_null() {
            let _process_guard = HandleGuard(process);
            identity.user_sid = process_handle_user_sid_string(process).ok();
            identity.process_creation_time = process_creation_time(process);
            administrative = process_handle_is_administrative(process).unwrap_or(false);
        }
    } else {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "failed to resolve named-pipe client process id"
        );
    }

    (identity, administrative)
}

#[cfg(windows)]
fn process_creation_time(process: HANDLE) -> Option<u64> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(windows)]
fn process_handle_is_administrative(process: HANDLE) -> io::Result<bool> {
    use windows_sys::Win32::{
        Security::{IsWellKnownSid, TOKEN_GROUPS, TokenGroups, WinBuiltinAdministratorsSid},
        System::SystemServices::SE_GROUP_ENABLED,
    };
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _guard = HandleGuard(token);
    if token_user_sid_string(token)? == "S-1-5-18" {
        return Ok(true);
    }
    let mut required = 0;
    unsafe { GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut required) };
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    // usize storage satisfies TOKEN_GROUPS/SID_AND_ATTRIBUTES alignment.
    let mut buffer = vec![0usize; (required as usize).div_ceil(mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            TokenGroups,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let groups = unsafe { &*buffer.as_ptr().cast::<TOKEN_GROUPS>() };
    let entries =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    Ok(entries.iter().any(|group| {
        group.Attributes & SE_GROUP_ENABLED as u32 != 0
            && unsafe { IsWellKnownSid(group.Sid, WinBuiltinAdministratorsSid) } != 0
    }))
}

#[cfg(windows)]
pub fn control_pipe_sddl_for_allowed_user(allowed_user_sid: &str) -> io::Result<String> {
    control_pipe_sddl_for_server(allowed_user_sid, true)
}

#[cfg(windows)]
fn control_pipe_sddl_for_server(
    allowed_user_sid: &str,
    privileged_server: bool,
) -> io::Result<String> {
    if !validate_allowed_user_sid_shape(allowed_user_sid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "allowed user SID must be a valid SID string",
        ));
    }

    // An unelevated per-user host and its clients have the same SID and
    // authority. That host needs create-instance rights to accept/reconnect.
    // SYSTEM/elevated hosts can create instances using their privileged ACE.
    let user_access = if privileged_server { "0x12019b" } else { "GA" };
    Ok(format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;{user_access};;;{allowed_user_sid})"
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
    service_user: Option<String>,
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

        let (client_identity, administrative_client) = named_pipe_client_identity(&server);
        let io = NamedPipeIo {
            client_identity,
            inner: server,
            service_user: service_user.clone(),
            administrative_client,
        };
        if io.authorize().is_err() {
            server = next_server;
            continue;
        }
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
    options.reject_remote_clients(true);
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
    #[cfg(windows)]
    fn console_user_policy_rejects_missing_foreign_and_changed_sessions() {
        let identity = ControlClientIdentity {
            user_sid: Some("S-1-5-21-1-2-3-1001".to_string()),
            session_id: Some(7),
            ..Default::default()
        };
        let sid = identity.user_sid.as_deref().unwrap();
        assert!(console_client_authorized(&identity, sid, Some(7)));
        assert!(!console_client_authorized(&identity, sid, None));
        assert!(!console_client_authorized(&identity, sid, Some(8)));
        assert!(!console_client_authorized(
            &identity,
            "S-1-5-21-1-2-3-1002",
            Some(7)
        ));
        let session_zero = ControlClientIdentity {
            session_id: Some(0),
            ..identity
        };
        assert!(!console_client_authorized(
            &session_zero,
            "S-1-5-21-1-2-3-1001",
            Some(0)
        ));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn pipe_acl_separates_privileged_clients_from_unelevated_per_user_hosts() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Security::{
            CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE,
            ImpersonateLoggedOnUser, RevertToSelf, SID_AND_ATTRIBUTES, TOKEN_DUPLICATE,
            WinBuiltinAdministratorsSid,
        };

        let path = format!(
            r"\\.\pipe\boundless-authority-fixture-{}",
            uuid::Uuid::new_v4()
        );
        let user_sid = current_user_sid_string().expect("user SID");
        let sddl = control_pipe_sddl_for_allowed_user(&user_sid).expect("fixture ACL");
        let descriptor = PipeSecurityDescriptor::from_sddl(&sddl).expect("fixture descriptor");
        let server = create_server(&path, true, &descriptor).expect("first fixture instance");
        tokio::task::spawn_blocking(move || {
            let mut process_token = ptr::null_mut();
            assert_ne!(
                unsafe {
                    OpenProcessToken(
                        GetCurrentProcess(),
                        TOKEN_DUPLICATE | TOKEN_QUERY,
                        &mut process_token,
                    )
                },
                0
            );
            let process_token = HandleGuard(process_token);
            let mut admin_sid = [0u64; 12];
            let mut sid_size = mem::size_of_val(&admin_sid) as u32;
            assert_ne!(
                unsafe {
                    CreateWellKnownSid(
                        WinBuiltinAdministratorsSid,
                        ptr::null_mut(),
                        admin_sid.as_mut_ptr().cast(),
                        &mut sid_size,
                    )
                },
                0
            );
            let deny_admin = SID_AND_ATTRIBUTES {
                Sid: admin_sid.as_mut_ptr().cast(),
                Attributes: 0,
            };
            let mut restricted = ptr::null_mut();
            assert_ne!(
                unsafe {
                    CreateRestrictedToken(
                        process_token.0,
                        DISABLE_MAX_PRIVILEGE,
                        1,
                        &deny_admin,
                        0,
                        ptr::null(),
                        0,
                        ptr::null(),
                        &mut restricted,
                    )
                },
                0
            );
            let restricted = HandleGuard(restricted);
            assert_ne!(unsafe { ImpersonateLoggedOnUser(restricted.0) }, 0);
            struct Revert;
            impl Drop for Revert {
                fn drop(&mut self) {
                    if unsafe { RevertToSelf() } == 0 {
                        std::process::abort();
                    }
                }
            }
            let _revert = Revert;
            let descriptor = PipeSecurityDescriptor::from_sddl(&sddl).expect("fixture descriptor");
            let error = create_server(&path, false, &descriptor)
                .expect_err("ordinary client cannot host another pipe instance");
            assert_eq!(error.raw_os_error(), Some(5));
            let client = std::fs::OpenOptions::new()
                .access_mode(0x0012_019b)
                .custom_flags(0x0010_0000 | 0x0001_0000)
                .open(&path)
                .expect("ordinary user can open the existing pipe with narrow data rights");
            drop(client);

            let per_user_path = format!("{path}-per-user");
            let per_user_acl =
                control_pipe_sddl_for_server(&user_sid, false).expect("per-user ACL");
            let descriptor =
                PipeSecurityDescriptor::from_sddl(&per_user_acl).expect("per-user descriptor");
            let first = create_server(&per_user_path, true, &descriptor)
                .expect("unelevated first instance");
            let client = std::fs::OpenOptions::new()
                .access_mode(0x0012_019b)
                .open(&per_user_path)
                .expect("first per-user client");
            let next = create_server(&per_user_path, false, &descriptor)
                .expect("unelevated host can create its accept loop successor");
            drop(client);
            drop(first);
            let client = std::fs::OpenOptions::new()
                .access_mode(0x0012_019b)
                .open(&per_user_path)
                .expect("per-user client reconnects to successor");
            drop(client);
            drop(next);
        })
        .await
        .expect("pipe fixture worker");
        drop(server);
    }

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
            "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;S-1-5-21-1-2-3-1001)"
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
