use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    ptr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use crate::{
    input::{TrackedWindowsInput, WindowsNumLockState, num_lock_state_from_dedicated_message_lane},
    process_identity::{
        ImageTrustState, WindowsProcessIdentity, authenticode_trust, current_process_identity,
        expected_boundless_image, process_identity, validate_injector_pair,
    },
    runtime::named_pipe_incoming_for_allowed_user,
};
use anyhow::{Context, Result, bail};
use clap::Parser;
use control_plane_client::channel_to_named_pipe_server;
pub use ipc_api::boundless::v1::{
    InputInjectorReason, InputInjectorSignatureTrust, InputInjectorState,
};
use ipc_api::{
    INPUT_INJECTOR_MAX_EVENTS, INPUT_INJECTOR_PROTOCOL_REVISION,
    boundless::v1::{
        BrokerInputEvent, InputInjectorApplyReply, InputInjectorApplyRequest,
        InputInjectorAttachReply, InputInjectorAttachRequest, InputInjectorControlReply,
        InputInjectorHeartbeatRequest, InputInjectorReleaseAndShutdownRequest,
        input_injector_service_client::InputInjectorServiceClient,
        input_injector_service_server::{InputInjectorService, InputInjectorServiceServer},
    },
    broker_events::{broker_events_from_input_events, input_events_from_broker_events},
    client_identity::ControlClientIdentity,
};
use tokio::sync::watch;
use tonic::{
    Request, Response, Status,
    transport::{Channel, Server},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, GetLastError,
        HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::Threading::{
        CreateMutexW, GetProcessId, MUTEX_MODIFY_STATE, OpenMutexW, OpenProcess,
        PROCESS_SYNCHRONIZE, WaitForSingleObject,
    },
    UI::{
        Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

const PIPE_PREFIX: &str = "boundless-elevated-input-v1";
const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(3);
pub const INPUT_INJECTOR_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_ATTEMPTS: usize = 4;
const INJECTOR_MAX_MESSAGE_BYTES: usize = 128 * 1024;

pub fn state_name(state: InputInjectorState) -> &'static str {
    match state {
        InputInjectorState::Unspecified => "unspecified",
        InputInjectorState::Off => "off",
        InputInjectorState::Prompting => "prompting",
        InputInjectorState::ReadyPendingIdle => "ready_pending_idle",
        InputInjectorState::Active => "active",
        InputInjectorState::Stopping => "stopping",
        InputInjectorState::Unavailable => "unavailable",
    }
}

pub fn reason_name(reason: InputInjectorReason) -> &'static str {
    match reason {
        InputInjectorReason::Unspecified => "unspecified",
        InputInjectorReason::None => "none",
        InputInjectorReason::UserCancelled => "user_cancelled",
        InputInjectorReason::NotInstalled => "not_installed",
        InputInjectorReason::WrongPath => "wrong_path",
        InputInjectorReason::IdentityRejected => "identity_rejected",
        InputInjectorReason::SignatureInvalid => "signature_invalid",
        InputInjectorReason::Duplicate => "duplicate",
        InputInjectorReason::ProtocolMismatch => "protocol_mismatch",
        InputInjectorReason::IpcUnavailable => "ipc_unavailable",
        InputInjectorReason::HeartbeatExpired => "heartbeat_expired",
        InputInjectorReason::ParentExited => "parent_exited",
        InputInjectorReason::InjectFailed => "inject_failed",
        InputInjectorReason::ShutdownIncomplete => "shutdown_incomplete",
        InputInjectorReason::DeliveryUncertain => "delivery_uncertain",
    }
}

pub fn signature_trust_name(trust: InputInjectorSignatureTrust) -> &'static str {
    match trust {
        InputInjectorSignatureTrust::Unspecified => "unspecified",
        InputInjectorSignatureTrust::Valid => "valid",
        InputInjectorSignatureTrust::UnsignedDogfood => "unsigned_dogfood",
        InputInjectorSignatureTrust::Invalid => "invalid",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectorStatus {
    pub state: InputInjectorState,
    pub reason: InputInjectorReason,
    pub signature_trust: InputInjectorSignatureTrust,
    pub helper_version: String,
}

impl InjectorStatus {
    pub fn telemetry_fields(&self) -> (&'static str, &'static str, &'static str) {
        (
            state_name(self.state),
            reason_name(self.reason),
            signature_trust_name(self.signature_trust),
        )
    }
}

#[derive(Debug)]
pub struct InjectorApplyOutcome {
    pub operation_id: u64,
    pub committed_event_count: usize,
    pub remaining_events: Vec<core_input::InputEvent>,
    pub replayed: bool,
    pub reason: InputInjectorReason,
    pub destination_num_lock_on: bool,
}

#[derive(Debug)]
pub struct ExplicitInjectorLaunch {
    process: OwnedProcessHandle,
    process_id: u32,
    endpoint: String,
    launch_nonce: String,
    signature_trust: InputInjectorSignatureTrust,
}

impl ExplicitInjectorLaunch {
    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn signature_trust(&self) -> InputInjectorSignatureTrust {
        self.signature_trust
    }
}

/// Explicitly launches the installed helper. Calling this function is the UAC
/// consent boundary; callers must invoke it only from a user action.
pub fn launch_explicit() -> Result<ExplicitInjectorLaunch> {
    let tray = current_process_identity().context("resolve injector launch origin")?;
    if !tray.is_medium_unelevated() || tray.session_id == 0 {
        bail!("administrator-app control must be requested by the unelevated interactive tray");
    }
    let helper_path = expected_boundless_image("boundless-input-injector.exe")?;
    if !helper_path.is_file() {
        bail!("installed elevated input injector was not found");
    }
    let helper_trust = authenticode_trust(&helper_path);
    if helper_trust == ImageTrustState::Invalid {
        bail!("installed elevated input injector has an invalid Authenticode state");
    }
    let launch_nonce = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let pipe_name = format!("{PIPE_PREFIX}-{}-{launch_nonce}", tray.session_id);
    let endpoint = format!("npipe://./pipe/{pipe_name}");
    let parameters = format!(
        "--pipe-name {pipe_name} --launch-nonce {launch_nonce} --origin-pid {} --origin-sid {} --origin-session {}",
        tray.process_id, tray.user_sid, tray.session_id
    );

    let verb = wide_null("runas");
    let helper = os_wide_null(helper_path.as_os_str());
    let parameters = wide_null(&parameters);
    let mut execute = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
    execute.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    execute.fMask = SEE_MASK_NOCLOSEPROCESS;
    execute.lpVerb = verb.as_ptr();
    execute.lpFile = helper.as_ptr();
    execute.lpParameters = parameters.as_ptr();
    execute.nShow = SW_HIDE;
    if unsafe { ShellExecuteExW(&mut execute) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            bail!("Windows permission prompt was canceled");
        }
        return Err(error).context("launch elevated input injector");
    }
    if execute.hProcess.is_null() {
        bail!("Windows returned no process handle for elevated input injector");
    }
    let process = OwnedProcessHandle(execute.hProcess);
    let process_id = unsafe { GetProcessId(process.0) };
    if process_id == 0 {
        return Err(std::io::Error::last_os_error())
            .context("resolve elevated injector process id");
    }

    Ok(ExplicitInjectorLaunch {
        process,
        process_id,
        endpoint,
        launch_nonce,
        signature_trust: signature_trust(helper_trust),
    })
}

/// Returns true only when no elevated injector currently owns this user's
/// interactive-session lane. Direct SendInput callers must check this at the
/// final injection boundary so a fast tray restart cannot overlap a stale
/// helper that is still releasing held input.
pub fn direct_input_lane_available() -> Result<bool> {
    let identity = current_process_identity().context("resolve direct input lane identity")?;
    Ok(direct_input_lane_available_for_identity(
        &identity.user_sid,
        identity.session_id,
    ))
}

pub struct InjectorClient {
    launch: ExplicitInjectorLaunch,
    client: InputInjectorServiceClient<Channel>,
    attachment_token: String,
    next_operation_id: u64,
    pending_apply: Option<InputInjectorApplyRequest>,
    status: InjectorStatus,
}

impl InjectorClient {
    pub async fn connect(launch: ExplicitInjectorLaunch) -> Result<Self> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let channel = loop {
            match channel_to_named_pipe_server(&launch.endpoint, launch.process_id).await {
                Ok(channel) => break channel,
                Err(error) => {
                    if launch.process.has_exited()? {
                        bail!(
                            "elevated input injector exited before its pipe became ready: {error:#}"
                        );
                    }
                    if Instant::now() >= deadline {
                        return Err(error).context("wait for elevated input injector pipe");
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let mut client = InputInjectorServiceClient::new(channel)
            .max_decoding_message_size(INJECTOR_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(INJECTOR_MAX_MESSAGE_BYTES);
        let attach = client
            .attach(InputInjectorAttachRequest {
                protocol_revision: INPUT_INJECTOR_PROTOCOL_REVISION,
                launch_nonce: launch.launch_nonce.clone(),
            })
            .await?
            .into_inner();
        if !attach.accepted {
            bail!(
                "elevated input injector rejected attachment: {}",
                reason_from_i32(attach.reason).as_str_name()
            );
        }
        if attach.protocol_revision != INPUT_INJECTOR_PROTOCOL_REVISION {
            bail!("elevated input injector protocol revision mismatch");
        }
        if attach.attachment_token.is_empty() {
            bail!("elevated input injector omitted its attachment token");
        }
        let signature_trust = signature_from_i32(attach.signature_trust);
        if signature_trust == InputInjectorSignatureTrust::Invalid {
            bail!("elevated input injector reported invalid signature trust");
        }
        let status = InjectorStatus {
            state: InputInjectorState::Active,
            reason: InputInjectorReason::None,
            signature_trust,
            helper_version: attach.helper_version,
        };
        Ok(Self {
            launch,
            client,
            attachment_token: attach.attachment_token,
            next_operation_id: 1,
            pending_apply: None,
            status,
        })
    }

    pub fn status(&self) -> &InjectorStatus {
        &self.status
    }

    pub async fn apply(
        &mut self,
        events: &[core_input::InputEvent],
        destination_num_lock_on: bool,
    ) -> Result<InjectorApplyOutcome> {
        let encoded = broker_events_from_input_events(events);
        let request = if let Some(pending) = self.pending_apply.as_ref() {
            if pending.events != encoded
                || pending.destination_num_lock_on != destination_num_lock_on
            {
                bail!("an uncertain injector operation must be retried before newer input");
            }
            pending.clone()
        } else {
            let request = InputInjectorApplyRequest {
                attachment_token: self.attachment_token.clone(),
                operation_id: self.next_operation_id,
                events: encoded,
                destination_num_lock_on,
            };
            self.pending_apply = Some(request.clone());
            request
        };
        let request_event_count = request.events.len();

        let reply = match self.client.apply(request.clone()).await {
            Ok(reply) => reply.into_inner(),
            Err(first_error) => {
                self.reconnect().await.with_context(|| {
                    format!("reconnect after uncertain injector apply: {first_error}")
                })?;
                self.client.apply(request).await?.into_inner()
            }
        };
        if !reply.accepted || reply.operation_id != self.next_operation_id {
            bail!("elevated input injector rejected or mismatched operation receipt");
        }
        let (remaining_events, undecodable) =
            input_events_from_broker_events(&reply.remaining_events);
        if undecodable != 0 {
            bail!("elevated input injector returned undecodable remaining input");
        }
        let committed_event_count = reply.committed_event_count as usize;
        if committed_event_count > request_event_count
            || committed_event_count.saturating_add(remaining_events.len()) != request_event_count
        {
            bail!("elevated input injector returned an invalid committed prefix receipt");
        }
        self.pending_apply = None;
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        let reason = reason_from_i32(reply.reason);
        self.status.reason = reason;
        Ok(InjectorApplyOutcome {
            operation_id: reply.operation_id,
            committed_event_count,
            remaining_events,
            replayed: reply.replayed,
            reason,
            destination_num_lock_on: reply.destination_num_lock_on,
        })
    }

    pub async fn heartbeat(&mut self) -> Result<()> {
        let reply = self
            .client
            .heartbeat(InputInjectorHeartbeatRequest {
                attachment_token: self.attachment_token.clone(),
            })
            .await?
            .into_inner();
        if !reply.accepted {
            bail!("elevated input injector rejected heartbeat");
        }
        Ok(())
    }

    pub async fn release_and_shutdown(mut self) -> Result<InjectorStatus> {
        self.status.state = InputInjectorState::Stopping;
        let reply = self
            .client
            .release_and_shutdown(InputInjectorReleaseAndShutdownRequest {
                attachment_token: self.attachment_token.clone(),
            })
            .await?
            .into_inner();
        self.status.state = InputInjectorState::Off;
        self.status.reason = reason_from_i32(reply.reason);
        if !reply.accepted || reply.remaining_held_event_count != 0 {
            bail!("elevated input injector shutdown left held input unresolved");
        }
        Ok(self.status)
    }

    async fn reconnect(&mut self) -> Result<()> {
        let channel =
            channel_to_named_pipe_server(&self.launch.endpoint, self.launch.process_id).await?;
        self.client = InputInjectorServiceClient::new(channel)
            .max_decoding_message_size(INJECTOR_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(INJECTOR_MAX_MESSAGE_BYTES);
        Ok(())
    }
}

#[derive(Debug, Parser)]
#[command(name = "boundless-input-injector", version)]
struct HelperArgs {
    #[arg(long)]
    pipe_name: String,
    #[arg(long)]
    launch_nonce: String,
    #[arg(long)]
    origin_pid: u32,
    #[arg(long)]
    origin_sid: String,
    #[arg(long)]
    origin_session: u32,
}

pub async fn run_helper() -> Result<()> {
    let args = HelperArgs::parse();
    validate_launch_shape(&args)?;
    let helper = current_process_identity().context("resolve elevated injector identity")?;
    let tray = process_identity(args.origin_pid).context("resolve injector origin identity")?;
    validate_injector_pair(&tray, &helper, args.origin_pid)?;
    if tray.user_sid != args.origin_sid
        || helper.user_sid != args.origin_sid
        || tray.session_id != args.origin_session
        || helper.session_id != args.origin_session
    {
        bail!("launch identity arguments did not match actual process identities");
    }
    let _single_instance = InjectorMutex::acquire(&args.origin_sid, args.origin_session)?;
    let parent = open_parent_watch(args.origin_pid)?;
    let signature_trust = combined_signature_trust(&tray, &helper);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let _shutdown_window =
        crate::cooperative_shutdown::CooperativeShutdownWindow::start(shutdown_tx.clone())
            .context("start elevated injector cooperative shutdown window")?;
    let runtime = Arc::new(InjectorRuntime {
        origin: tray,
        helper,
        launch_nonce: args.launch_nonce,
        signature_trust,
        state: Mutex::new(InjectorRuntimeState {
            created_at: Instant::now(),
            last_activity: Instant::now(),
            attachment_token: None,
            last_apply: None,
            injector: TrackedWindowsInput::new(WindowsNumLockState::new(
                num_lock_state_from_dedicated_message_lane()
                    .context("seed elevated injector Num Lock authority")?,
            )),
        }),
        shutdown_tx,
    });
    let incoming = named_pipe_incoming_for_allowed_user(&args.pipe_name, &args.origin_sid)
        .context("create elevated injector named pipe")?;
    let watchdog_runtime = runtime.clone();
    let service = InjectorServiceImpl {
        runtime: runtime.clone(),
    };
    Server::builder()
        .add_service(
            InputInjectorServiceServer::new(service)
                .max_decoding_message_size(INJECTOR_MAX_MESSAGE_BYTES)
                .max_encoding_message_size(INJECTOR_MAX_MESSAGE_BYTES),
        )
        .serve_with_incoming_shutdown(incoming, watchdog(parent, watchdog_runtime, shutdown_rx))
        .await
        .context("serve elevated injector pipe")?;
    let mut state = runtime.lock();
    let _ = cleanup_injector(&mut state.injector, CLEANUP_ATTEMPTS);
    Ok(())
}

fn validate_launch_shape(args: &HelperArgs) -> Result<()> {
    if args.origin_pid == 0 || args.origin_session == 0 {
        bail!("elevated injector requires a live interactive origin");
    }
    if args.launch_nonce.len() != 64
        || !args
            .launch_nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("elevated injector launch nonce must be 256-bit hexadecimal");
    }
    if !args.pipe_name.starts_with(PIPE_PREFIX)
        || args.pipe_name.len() > 180
        || !args
            .pipe_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("elevated injector pipe name was invalid");
    }
    Ok(())
}

struct InjectorRuntime {
    origin: WindowsProcessIdentity,
    helper: WindowsProcessIdentity,
    launch_nonce: String,
    signature_trust: InputInjectorSignatureTrust,
    state: Mutex<InjectorRuntimeState>,
    shutdown_tx: watch::Sender<bool>,
}

impl InjectorRuntime {
    fn lock(&self) -> MutexGuard<'_, InjectorRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn verify_client<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let identity = request
            .extensions()
            .get::<ControlClientIdentity>()
            .ok_or_else(|| Status::permission_denied("verified named-pipe identity required"))?;
        if identity.process_id != Some(self.origin.process_id)
            || identity.user_sid.as_deref() != Some(self.origin.user_sid.as_str())
            || identity.session_id != Some(self.origin.session_id)
        {
            return Err(Status::permission_denied(
                "injector client identity rejected",
            ));
        }
        Ok(())
    }

    fn verify_token(&self, token: &str, state: &mut InjectorRuntimeState) -> Result<(), Status> {
        if state.attachment_token.as_deref() != Some(token) {
            return Err(Status::permission_denied(
                "injector attachment token rejected",
            ));
        }
        state.last_activity = Instant::now();
        Ok(())
    }
}

struct InjectorRuntimeState {
    created_at: Instant,
    last_activity: Instant,
    attachment_token: Option<String>,
    last_apply: Option<CachedApply>,
    injector: TrackedWindowsInput,
}

#[derive(Clone)]
struct CachedApply {
    request_events: Vec<BrokerInputEvent>,
    request_destination_num_lock_on: bool,
    reply: InputInjectorApplyReply,
}

/// Returns true only for a byte-for-byte-equivalent replay of the last
/// operation. New operation ids must be contiguous, preventing a reconnect
/// from silently skipping an uncertain input prefix.
fn validate_operation_sequence(
    last_apply: Option<&CachedApply>,
    operation_id: u64,
    request_events: &[BrokerInputEvent],
    request_destination_num_lock_on: bool,
) -> Result<bool, Status> {
    let Some(cached) = last_apply else {
        if operation_id != 1 {
            return Err(Status::failed_precondition(
                "first injector operation id must be one",
            ));
        }
        return Ok(false);
    };
    if operation_id == cached.reply.operation_id {
        if request_events != cached.request_events
            || request_destination_num_lock_on != cached.request_destination_num_lock_on
        {
            return Err(Status::failed_precondition(
                "injector operation replay changed payload",
            ));
        }
        return Ok(true);
    }
    if operation_id != cached.reply.operation_id.saturating_add(1) {
        return Err(Status::failed_precondition(
            "injector operation id was not contiguous",
        ));
    }
    Ok(false)
}

#[derive(Clone)]
struct InjectorServiceImpl {
    runtime: Arc<InjectorRuntime>,
}

#[tonic::async_trait]
impl InputInjectorService for InjectorServiceImpl {
    async fn attach(
        &self,
        request: Request<InputInjectorAttachRequest>,
    ) -> Result<Response<InputInjectorAttachReply>, Status> {
        self.runtime.verify_client(&request)?;
        let request = request.into_inner();
        if request.protocol_revision != INPUT_INJECTOR_PROTOCOL_REVISION {
            return Ok(Response::new(InputInjectorAttachReply {
                accepted: false,
                reason: InputInjectorReason::ProtocolMismatch as i32,
                protocol_revision: INPUT_INJECTOR_PROTOCOL_REVISION,
                ..Default::default()
            }));
        }
        if request.launch_nonce != self.runtime.launch_nonce {
            return Err(Status::permission_denied(
                "injector launch handshake rejected",
            ));
        }
        // Re-query the actual connecting image at attachment time. Command-line
        // identity values never authorize the privileged channel.
        let client = process_identity(self.runtime.origin.process_id)
            .map_err(|_| Status::permission_denied("injector client process unavailable"))?;
        validate_injector_pair(
            &client,
            &self.runtime.helper,
            self.runtime.origin.process_id,
        )
        .map_err(|_| Status::permission_denied("injector client process validation failed"))?;

        let mut state = self.runtime.lock();
        if state.attachment_token.is_some() {
            return Ok(Response::new(InputInjectorAttachReply {
                accepted: false,
                reason: InputInjectorReason::Duplicate as i32,
                protocol_revision: INPUT_INJECTOR_PROTOCOL_REVISION,
                ..Default::default()
            }));
        }
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        state.attachment_token = Some(token.clone());
        state.last_activity = Instant::now();
        Ok(Response::new(InputInjectorAttachReply {
            accepted: true,
            reason: InputInjectorReason::None as i32,
            protocol_revision: INPUT_INJECTOR_PROTOCOL_REVISION,
            attachment_token: token,
            helper_version: env!("CARGO_PKG_VERSION").to_string(),
            signature_trust: self.runtime.signature_trust as i32,
        }))
    }

    async fn apply(
        &self,
        request: Request<InputInjectorApplyRequest>,
    ) -> Result<Response<InputInjectorApplyReply>, Status> {
        self.runtime.verify_client(&request)?;
        let request = request.into_inner();
        if request.operation_id == 0 || request.events.is_empty() {
            return Err(Status::invalid_argument(
                "injector operation must contain input",
            ));
        }
        if request.events.len() > INPUT_INJECTOR_MAX_EVENTS {
            return Err(Status::resource_exhausted("injector event cap exceeded"));
        }
        let (events, undecodable) = input_events_from_broker_events(&request.events);
        if undecodable != 0 {
            return Err(Status::invalid_argument(
                "injector request contained invalid input",
            ));
        }
        let mut state = self.runtime.lock();
        self.runtime
            .verify_token(&request.attachment_token, &mut state)?;
        if validate_operation_sequence(
            state.last_apply.as_ref(),
            request.operation_id,
            &request.events,
            request.destination_num_lock_on,
        )? {
            let mut reply = state
                .last_apply
                .as_ref()
                .expect("replay validation requires a cached operation")
                .reply
                .clone();
            reply.replayed = true;
            return Ok(Response::new(reply));
        }

        let _ = state
            .injector
            .synchronize_num_lock_if_native_idle(request.destination_num_lock_on);
        let outcome = state.injector.send_events(&events);
        let reply = InputInjectorApplyReply {
            accepted: true,
            reason: if outcome.error.is_none() {
                InputInjectorReason::None as i32
            } else {
                InputInjectorReason::InjectFailed as i32
            },
            operation_id: request.operation_id,
            committed_event_count: outcome.committed_event_count as u32,
            remaining_events: broker_events_from_input_events(&outcome.remaining_events),
            replayed: false,
            destination_num_lock_on: state.injector.num_lock_is_on(),
        };
        state.last_apply = Some(CachedApply {
            request_events: request.events,
            request_destination_num_lock_on: request.destination_num_lock_on,
            reply: reply.clone(),
        });
        Ok(Response::new(reply))
    }

    async fn heartbeat(
        &self,
        request: Request<InputInjectorHeartbeatRequest>,
    ) -> Result<Response<InputInjectorControlReply>, Status> {
        self.runtime.verify_client(&request)?;
        let request = request.into_inner();
        let mut state = self.runtime.lock();
        self.runtime
            .verify_token(&request.attachment_token, &mut state)?;
        Ok(Response::new(control_reply(
            true,
            InputInjectorReason::None,
            0,
        )))
    }

    async fn release_and_shutdown(
        &self,
        request: Request<InputInjectorReleaseAndShutdownRequest>,
    ) -> Result<Response<InputInjectorControlReply>, Status> {
        self.runtime.verify_client(&request)?;
        let request = request.into_inner();
        let (clean, remaining) = {
            let mut state = self.runtime.lock();
            self.runtime
                .verify_token(&request.attachment_token, &mut state)?;
            let clean = cleanup_injector(&mut state.injector, CLEANUP_ATTEMPTS);
            let remaining = state.injector.held_down_events().len() as u32;
            (clean, remaining)
        };
        self.runtime.shutdown_tx.send_replace(true);
        Ok(Response::new(control_reply(
            clean,
            if clean {
                InputInjectorReason::None
            } else {
                InputInjectorReason::ShutdownIncomplete
            },
            remaining,
        )))
    }
}

fn control_reply(
    accepted: bool,
    reason: InputInjectorReason,
    remaining_held_event_count: u32,
) -> InputInjectorControlReply {
    InputInjectorControlReply {
        accepted,
        reason: reason as i32,
        remaining_held_event_count,
    }
}

fn cleanup_injector(injector: &mut TrackedWindowsInput, max_attempts: usize) -> bool {
    for _ in 0..max_attempts {
        if injector.is_idle() {
            return true;
        }
        let _ = injector.release_all();
    }
    injector.is_idle()
}

async fn watchdog(
    parent: ParentWatchHandle,
    runtime: Arc<InjectorRuntime>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let parent_exited = parent.has_exited().unwrap_or(true);
                let expired = {
                    let state = runtime.lock();
                    let timeout = if state.attachment_token.is_some() { HEARTBEAT_TIMEOUT } else { ATTACH_TIMEOUT };
                    let observed = if state.attachment_token.is_some() { state.last_activity } else { state.created_at };
                    observed.elapsed() >= timeout
                };
                if parent_exited || expired {
                    let mut state = runtime.lock();
                    let _ = cleanup_injector(&mut state.injector, CLEANUP_ATTEMPTS);
                    runtime.shutdown_tx.send_replace(true);
                    break;
                }
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

fn combined_signature_trust(
    tray: &WindowsProcessIdentity,
    helper: &WindowsProcessIdentity,
) -> InputInjectorSignatureTrust {
    if tray.image_trust == ImageTrustState::Invalid
        || helper.image_trust == ImageTrustState::Invalid
    {
        InputInjectorSignatureTrust::Invalid
    } else if tray.image_trust == ImageTrustState::Valid
        && helper.image_trust == ImageTrustState::Valid
    {
        InputInjectorSignatureTrust::Valid
    } else {
        InputInjectorSignatureTrust::UnsignedDogfood
    }
}

fn signature_trust(trust: ImageTrustState) -> InputInjectorSignatureTrust {
    match trust {
        ImageTrustState::Valid => InputInjectorSignatureTrust::Valid,
        ImageTrustState::UnsignedDogfood => InputInjectorSignatureTrust::UnsignedDogfood,
        ImageTrustState::Invalid => InputInjectorSignatureTrust::Invalid,
    }
}

fn reason_from_i32(value: i32) -> InputInjectorReason {
    InputInjectorReason::try_from(value).unwrap_or(InputInjectorReason::Unspecified)
}

fn signature_from_i32(value: i32) -> InputInjectorSignatureTrust {
    InputInjectorSignatureTrust::try_from(value).unwrap_or(InputInjectorSignatureTrust::Unspecified)
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn os_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[derive(Debug)]
struct OwnedProcessHandle(HANDLE);
impl OwnedProcessHandle {
    fn has_exited(&self) -> Result<bool> {
        match unsafe { WaitForSingleObject(self.0, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error()).context("query injector process state"),
        }
    }
}
impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct ParentWatchHandle(usize);
impl ParentWatchHandle {
    fn has_exited(&self) -> Result<bool> {
        let handle = self.0 as HANDLE;
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error()).context("query injector origin state"),
        }
    }
}
impl Drop for ParentWatchHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                CloseHandle(self.0 as HANDLE);
            }
        }
    }
}

fn open_parent_watch(process_id: u32) -> Result<ParentWatchHandle> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("open injector origin watchdog");
    }
    Ok(ParentWatchHandle(handle as usize))
}

struct InjectorMutex(HANDLE);
impl InjectorMutex {
    fn acquire(user_sid: &str, session_id: u32) -> Result<Self> {
        let name = wide_null(&injector_mutex_name(user_sid, session_id));
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("create injector owner mutex");
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            bail!("another elevated input injector already owns this user session");
        }
        Ok(Self(handle))
    }
}

fn injector_mutex_name(user_sid: &str, session_id: u32) -> String {
    format!("Local\\Boundless.ElevatedInputInjector.v1.{user_sid}.{session_id}")
}

fn direct_input_lane_available_for_identity(user_sid: &str, session_id: u32) -> bool {
    let name = wide_null(&injector_mutex_name(user_sid, session_id));
    let handle = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        return unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND;
    }
    unsafe {
        CloseHandle(handle);
    }
    false
}
impl Drop for InjectorMutex {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_shape_rejects_short_nonce_and_path_like_pipe() {
        let base = HelperArgs {
            pipe_name: format!("{PIPE_PREFIX}-1-{}", "a".repeat(64)),
            launch_nonce: "a".repeat(64),
            origin_pid: 42,
            origin_sid: "S-1-5-21-1".to_string(),
            origin_session: 1,
        };
        validate_launch_shape(&base).expect("valid launch shape");
        let mut short = base;
        short.launch_nonce = "abc".to_string();
        assert!(validate_launch_shape(&short).is_err());
        short.launch_nonce = "b".repeat(64);
        short.pipe_name = format!("{PIPE_PREFIX}\\escape");
        assert!(validate_launch_shape(&short).is_err());
    }

    #[test]
    fn signature_evidence_never_collapses_invalid_into_dogfood() {
        let identity = |trust| WindowsProcessIdentity {
            process_id: 1,
            user_sid: "S-1-5-21-1".to_string(),
            session_id: 1,
            integrity_rid: 8192,
            elevated: false,
            image_path: "unused".into(),
            image_trust: trust,
        };
        assert_eq!(
            combined_signature_trust(
                &identity(ImageTrustState::UnsignedDogfood),
                &identity(ImageTrustState::UnsignedDogfood)
            ),
            InputInjectorSignatureTrust::UnsignedDogfood
        );
        assert_eq!(
            combined_signature_trust(
                &identity(ImageTrustState::Valid),
                &identity(ImageTrustState::Invalid)
            ),
            InputInjectorSignatureTrust::Invalid
        );
    }

    #[test]
    fn direct_lane_waits_for_stale_helper_mutex_release() {
        let sid = format!("S-1-5-21-test-{}", uuid::Uuid::new_v4().simple());
        let session_id = 4242;
        for _ in 0..32 {
            assert!(direct_input_lane_available_for_identity(&sid, session_id));
        }
        let helper = InjectorMutex::acquire(&sid, session_id).expect("acquire helper lane");
        assert!(!direct_input_lane_available_for_identity(&sid, session_id));
        drop(helper);
        assert!(direct_input_lane_available_for_identity(&sid, session_id));
    }

    #[test]
    fn operation_sequence_accepts_only_exact_replay_or_contiguous_next() {
        let first =
            broker_events_from_input_events(&[core_input::InputEvent::MouseMove { dx: 4, dy: -3 }]);
        let changed =
            broker_events_from_input_events(&[core_input::InputEvent::MouseMove { dx: 5, dy: -3 }]);
        assert!(!validate_operation_sequence(None, 1, &first, false).expect("first operation"));
        assert_eq!(
            validate_operation_sequence(None, 2, &first, false)
                .expect_err("first operation cannot skip")
                .code(),
            tonic::Code::FailedPrecondition
        );

        let cached = CachedApply {
            request_events: first.clone(),
            request_destination_num_lock_on: false,
            reply: InputInjectorApplyReply {
                operation_id: 7,
                ..Default::default()
            },
        };
        assert!(
            validate_operation_sequence(Some(&cached), 7, &first, false).expect("exact replay")
        );
        assert!(
            !validate_operation_sequence(Some(&cached), 8, &changed, true).expect("next operation")
        );
        assert_eq!(
            validate_operation_sequence(Some(&cached), 7, &changed, false)
                .expect_err("replay payload must match")
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            validate_operation_sequence(Some(&cached), 7, &first, true)
                .expect_err("replay destination Num Lock must match")
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            validate_operation_sequence(Some(&cached), 9, &first, false)
                .expect_err("operation cannot skip")
                .code(),
            tonic::Code::FailedPrecondition
        );
    }
}
