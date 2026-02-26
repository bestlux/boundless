#[cfg(not(windows))]
fn main() {
    eprintln!("boundlesstray is currently supported on Windows only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_app::run()
}

#[cfg(windows)]
mod windows_app {
    use anyhow::{Context, Result, bail};
    use clap::Parser;
    use hyper_util::rt::TokioIo;
    use ipc_api::boundless::v1::{
        Empty, HotkeyTriggerRequest, ImportTrustBundleRequest, NearbyPairingDecisionRequest,
        StatusRequest, daemon_service_client::DaemonServiceClient,
        diagnostics_service_client::DiagnosticsServiceClient,
        pairing_service_client::PairingServiceClient,
        topology_service_client::TopologyServiceClient,
    };
    use serde::{Deserialize, Serialize};
    use std::{
        future::Future,
        os::windows::process::CommandExt,
        pin::Pin,
        process::{Command as ProcessCommand, Stdio},
        task::{Context as TaskContext, Poll},
        thread::sleep,
        time::{Duration, Instant},
    };
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{TcpStream, windows::named_pipe::ClientOptions},
        time::timeout,
    };
    use tonic::{
        codegen::Service,
        transport::{Channel, Endpoint, Uri},
    };
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{Menu, MenuItem, PredefinedMenuItem},
    };

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const ACTION_DASHBOARD: &str = "dashboard";
    const ACTION_QUIT: &str = "quit";
    const NEARBY_PAIRING_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
    const NEARBY_PAIRING_IO_TIMEOUT: Duration = Duration::from_secs(6);
    const NEARBY_PAIRING_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

    #[derive(Debug, Parser)]
    #[command(
        name = "boundlesstray",
        version,
        about = "Boundless tray control surface"
    )]
    struct Cli {
        #[arg(
            long,
            env = "BOUNDLESS_API_ENDPOINT",
            default_value_t = default_endpoint()
        )]
        endpoint: String,
        #[arg(long, default_value_t = true)]
        start_daemon: bool,
    }

    #[derive(Debug)]
    struct AppContext {
        endpoint: String,
        start_daemon: bool,
        ctl_candidates: Vec<String>,
        daemon_candidates: Vec<String>,
    }

    #[derive(Debug, Clone, Deserialize, Default)]
    struct UiSnapshot {
        generated_at: String,
        daemon_online: bool,
        machine_id: String,
        layout_matrix: String,
        discovered_peers: Vec<UiDiscoveredPeer>,
        paired_peers: Vec<UiPairedPeer>,
        pending_requests: Vec<UiPendingRequest>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiDiscoveredPeer {
        machine_id: String,
        display_name: String,
        endpoint: String,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiPairedPeer {
        peer_id: String,
        display_name: String,
        address: String,
        connected: bool,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct UiPendingRequest {
        request_id: String,
        requester_machine_id: String,
        requester_display_name: String,
        created_at: String,
        #[serde(default)]
        verification_code: String,
        #[serde(default)]
        verification_expires_at: String,
        #[serde(default)]
        requires_verification_code: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct StoredTrustBundle {
        machine_id: String,
        display_name: String,
        network_address: String,
        ca_cert_pem: String,
    }

    #[derive(Debug, Serialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    enum NearbyJoinWireRequest {
        NearbyRequestCode {
            requester_bundle: StoredTrustBundle,
            requester_alias: Option<String>,
        },
        NearbySubmitCode {
            request_id: String,
            code: String,
            verification_nonce: String,
            requester_alias: Option<String>,
        },
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum NearbyJoinWireResponse {
        Pending {
            #[serde(rename = "request_id")]
            _request_id: String,
            message: String,
        },
        Approved {
            request_id: String,
            responder_bundle: StoredTrustBundle,
        },
        Rejected {
            message: String,
        },
        Error {
            message: String,
        },
        CodeRequired {
            request_id: String,
            message: String,
            verification_nonce: String,
            expires_at: String,
        },
    }

    enum NearbyRequestCodeStart {
        CodeRequired {
            request_id: String,
            verification_nonce: String,
            expires_at: String,
        },
        Unsupported {
            reason: String,
        },
    }

    #[derive(Debug, Clone)]
    struct GuidedPairingFlow {
        dialog_title: String,
        host: String,
        pairing_port: u16,
        default_alias: String,
        orientation_selector_fallback: String,
    }

    #[derive(Debug, Clone)]
    struct PairingChallengeState {
        request_id: String,
        verification_nonce: String,
        expires_at: String,
    }

    #[derive(Debug, Clone)]
    struct GuidedPairingResult {
        peer_machine_id: String,
        orientation_selector: String,
    }

    include!("dashboard.rs");

    fn run_boundlessctl(ctx: &AppContext, args: &[String]) -> Result<String> {
        run_boundlessctl_with_timeout(ctx, args, Duration::from_secs(20))
    }

    fn run_boundlessctl_with_timeout(
        ctx: &AppContext,
        args: &[String],
        timeout: Duration,
    ) -> Result<String> {
        let mut attempted = Vec::<String>::new();

        for candidate in &ctx.ctl_candidates {
            let mut command = ProcessCommand::new(candidate);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command.creation_flags(CREATE_NO_WINDOW);
            command.arg("--endpoint").arg(&ctx.endpoint);
            for arg in args {
                command.arg(arg);
            }

            match command.spawn() {
                Ok(mut child) => {
                    let started_at = Instant::now();
                    while started_at.elapsed() < timeout {
                        match child.try_wait() {
                            Ok(Some(_status)) => break,
                            Ok(None) => sleep(Duration::from_millis(20)),
                            Err(error) => {
                                bail!(
                                    "failed waiting for `{}` args=`{}`: {}",
                                    candidate,
                                    args.join(" "),
                                    error
                                );
                            }
                        }
                    }

                    let finished = matches!(child.try_wait(), Ok(Some(_)));
                    if !finished {
                        let _ = child.kill();
                        let _ = child.wait();
                        bail!(
                            "command timed out via `{}` args=`{}` timeout={}s",
                            candidate,
                            args.join(" "),
                            timeout.as_secs()
                        );
                    }

                    let output = child.wait_with_output().with_context(|| {
                        format!(
                            "collect output for `{}` args=`{}`",
                            candidate,
                            args.join(" ")
                        )
                    })?;
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        return Ok(stdout);
                    }

                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    bail!(
                        "command failed via `{}` args=`{}`\nstdout: {}\nstderr: {}",
                        candidate,
                        args.join(" "),
                        truncate(&stdout, 600),
                        truncate(&stderr, 600)
                    );
                }
                Err(error) => attempted.push(format!("{candidate}: {error}")),
            }
        }

        bail!(
            "failed to launch boundlessctl; candidates attempted: {}",
            attempted.join("; ")
        )
    }

    fn fetch_ui_snapshot_blocking(endpoint: &str) -> Result<UiSnapshot> {
        block_on_result(fetch_ui_snapshot(endpoint))
    }

    fn pair_nearby_request_code_blocking(
        endpoint: &str,
        host: String,
        port: u16,
    ) -> Result<NearbyRequestCodeStart> {
        block_on_result(pair_nearby_request_code(endpoint, host, port))
    }

    fn pair_nearby_submit_code_blocking(
        endpoint: &str,
        request_id: String,
        code: String,
        verification_nonce: String,
        host: String,
        port: u16,
        alias: Option<String>,
    ) -> Result<String> {
        block_on_result(pair_nearby_submit_code(
            endpoint,
            request_id,
            code,
            verification_nonce,
            host,
            port,
            alias,
        ))
    }

    fn approve_nearby_pairing_request_blocking(endpoint: &str, request_id: &str) -> Result<String> {
        block_on_result(approve_nearby_pairing_request(
            endpoint,
            request_id.to_string(),
        ))
    }

    fn reject_nearby_pairing_request_blocking(endpoint: &str, request_id: &str) -> Result<String> {
        block_on_result(reject_nearby_pairing_request(
            endpoint,
            request_id.to_string(),
        ))
    }

    fn trigger_hotkey_action_blocking(endpoint: &str, action: &str) -> Result<String> {
        block_on_result(trigger_hotkey_action(endpoint, action.to_string()))
    }

    fn ensure_daemon_available_blocking(ctx: &AppContext) -> Result<Option<String>> {
        block_on_result(ensure_daemon_available(
            &ctx.endpoint,
            ctx.start_daemon,
            &ctx.daemon_candidates,
        ))
    }

    fn block_on_result<F, T>(future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create tokio runtime for tray async flow")?;
        runtime.block_on(future)
    }

    async fn trigger_hotkey_action(endpoint: &str, action: String) -> Result<String> {
        let mut diagnostics_client = DiagnosticsServiceClient::new(channel(endpoint).await?);
        let response = diagnostics_client
            .trigger_hotkey_action(HotkeyTriggerRequest { action })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn ensure_daemon_available(
        endpoint: &str,
        start_daemon: bool,
        daemon_candidates: &[String],
    ) -> Result<Option<String>> {
        if channel(endpoint).await.is_ok() {
            return Ok(None);
        }

        if !start_daemon {
            bail!("daemon is not reachable at {endpoint}; run boundlessd or pass --start-daemon");
        }

        let launched = spawn_daemon_process(daemon_candidates)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match channel(endpoint).await {
                Ok(_) => return Ok(Some(launched)),
                Err(error) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "daemon did not become reachable at {endpoint} after start attempt: {error}"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    fn spawn_daemon_process(candidates: &[String]) -> Result<String> {
        let mut errors = Vec::new();
        for candidate in candidates {
            let mut command = ProcessCommand::new(candidate);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command.creation_flags(CREATE_NO_WINDOW);

            match command.spawn() {
                Ok(_) => return Ok(candidate.clone()),
                Err(error) => errors.push(format!("{candidate}: {error}")),
            }
        }

        bail!(
            "failed to start boundlessd; candidates attempted: {}",
            errors.join("; ")
        )
    }

    async fn fetch_ui_snapshot(endpoint: &str) -> Result<UiSnapshot> {
        let channel = channel(endpoint).await?;

        let mut daemon_client = DaemonServiceClient::new(channel.clone());
        let status = daemon_client
            .get_status(StatusRequest {})
            .await?
            .into_inner();

        let mut topology_client = TopologyServiceClient::new(channel.clone());
        let peers = topology_client
            .list_peers(Empty {})
            .await?
            .into_inner()
            .peers;
        let layout = topology_client
            .layout_show(Empty {})
            .await?
            .into_inner()
            .matrix_spec;

        let mut diagnostics_client = DiagnosticsServiceClient::new(channel.clone());
        let discovery = diagnostics_client
            .list_discovery_peers(Empty {})
            .await?
            .into_inner();

        let mut pairing_client = PairingServiceClient::new(channel);
        let pending = pairing_client
            .list_nearby_pairing_requests(Empty {})
            .await?
            .into_inner()
            .requests;

        let mut discovered_peers = discovery
            .peers
            .into_iter()
            .map(|peer| UiDiscoveredPeer {
                machine_id: peer.machine_id,
                display_name: peer.display_name,
                endpoint: peer.endpoint,
            })
            .collect::<Vec<_>>();
        discovered_peers.sort_by(|left, right| {
            left.display_name
                .cmp(&right.display_name)
                .then_with(|| left.machine_id.cmp(&right.machine_id))
        });

        let mut paired_peers = peers
            .into_iter()
            .map(|peer| UiPairedPeer {
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                address: peer.address,
                connected: peer.connected,
            })
            .collect::<Vec<_>>();
        paired_peers.sort_by(|left, right| {
            left.display_name
                .cmp(&right.display_name)
                .then_with(|| left.peer_id.cmp(&right.peer_id))
        });

        let mut pending_requests = pending
            .into_iter()
            .map(|request| UiPendingRequest {
                request_id: request.request_id,
                requester_machine_id: request.requester_machine_id,
                requester_display_name: request.requester_display_name,
                created_at: request.created_at,
                verification_code: request.verification_code,
                verification_expires_at: request.verification_expires_at,
                requires_verification_code: request.requires_verification_code,
            })
            .collect::<Vec<_>>();
        pending_requests.sort_by(|left, right| left.created_at.cmp(&right.created_at));

        Ok(UiSnapshot {
            generated_at: String::new(),
            daemon_online: status.running,
            machine_id: status.machine_id,
            layout_matrix: layout,
            discovered_peers,
            paired_peers,
            pending_requests,
        })
    }

    async fn pair_nearby_request_code(
        endpoint: &str,
        host: String,
        port: u16,
    ) -> Result<NearbyRequestCodeStart> {
        let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
        let local_bundle = pairing_client
            .export_trust_bundle(Empty {})
            .await?
            .into_inner();
        let requester_bundle = StoredTrustBundle {
            machine_id: local_bundle.machine_id,
            display_name: local_bundle.display_name,
            network_address: local_bundle.network_address,
            ca_cert_pem: local_bundle.ca_cert_pem,
        };

        let target = format_host_port(&host, port);
        let response = send_nearby_pairing_request(
            &target,
            NearbyJoinWireRequest::NearbyRequestCode {
                requester_bundle,
                requester_alias: None,
            },
        )
        .await?;

        match response {
            NearbyJoinWireResponse::CodeRequired {
                request_id,
                verification_nonce,
                expires_at,
                ..
            } => Ok(NearbyRequestCodeStart::CodeRequired {
                request_id,
                verification_nonce,
                expires_at,
            }),
            NearbyJoinWireResponse::Error { message } => {
                let lowered = message.to_ascii_lowercase();
                if lowered.contains("unknown variant")
                    || lowered.contains("parse pairing request")
                    || lowered.contains("missing field")
                {
                    return Ok(NearbyRequestCodeStart::Unsupported { reason: message });
                }
                bail!("nearby pairing request failed: {message}");
            }
            NearbyJoinWireResponse::Rejected { message, .. } => {
                bail!("nearby pairing request rejected: {message}");
            }
            NearbyJoinWireResponse::Pending { message, .. } => {
                bail!("unexpected nearby pairing status: {message}");
            }
            NearbyJoinWireResponse::Approved { .. } => {
                bail!("unexpected nearby pairing status: approved");
            }
        }
    }

    async fn pair_nearby_submit_code(
        endpoint: &str,
        request_id: String,
        code: String,
        verification_nonce: String,
        host: String,
        port: u16,
        alias: Option<String>,
    ) -> Result<String> {
        let target = format_host_port(&host, port);
        let response = send_nearby_pairing_request(
            &target,
            NearbyJoinWireRequest::NearbySubmitCode {
                request_id: request_id.clone(),
                code,
                verification_nonce,
                requester_alias: None,
            },
        )
        .await?;
        let responder_bundle = match response {
            NearbyJoinWireResponse::Approved {
                request_id: approved_request_id,
                responder_bundle,
                ..
            } => {
                if approved_request_id != request_id {
                    bail!("nearby pairing request id mismatch");
                }
                responder_bundle
            }
            NearbyJoinWireResponse::Pending { .. } => {
                bail!(
                    "unexpected pending response for code submission; start a new pairing request"
                );
            }
            NearbyJoinWireResponse::Rejected { message, .. } => {
                bail!("nearby pairing rejected: {message}");
            }
            NearbyJoinWireResponse::Error { message } => {
                bail!("nearby pairing failed: {message}");
            }
            NearbyJoinWireResponse::CodeRequired { message, .. } => {
                bail!("nearby pairing failed: {message}");
            }
        };
        import_nearby_responder_bundle(endpoint, responder_bundle, &host, alias).await
    }

    async fn approve_nearby_pairing_request(endpoint: &str, request_id: String) -> Result<String> {
        let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
        let response = pairing_client
            .approve_nearby_pairing_request(NearbyPairingDecisionRequest {
                request_id,
                alias: String::new(),
            })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn reject_nearby_pairing_request(endpoint: &str, request_id: String) -> Result<String> {
        let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
        let response = pairing_client
            .reject_nearby_pairing_request(NearbyPairingDecisionRequest {
                request_id,
                alias: String::new(),
            })
            .await?
            .into_inner();
        Ok(response.message)
    }

    async fn import_nearby_responder_bundle(
        endpoint: &str,
        mut responder_bundle: StoredTrustBundle,
        host: &str,
        alias: Option<String>,
    ) -> Result<String> {
        normalize_bundle_address_for_host(&mut responder_bundle, host)?;

        let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
        pairing_client
            .import_trust_bundle(ImportTrustBundleRequest {
                machine_id: responder_bundle.machine_id.clone(),
                display_name: responder_bundle.display_name,
                network_address: responder_bundle.network_address,
                ca_cert_pem: responder_bundle.ca_cert_pem,
                alias: alias.unwrap_or_default(),
            })
            .await?
            .into_inner();

        let mut diagnostics_client = DiagnosticsServiceClient::new(channel(endpoint).await?);
        let _ = diagnostics_client
            .trigger_hotkey_action(HotkeyTriggerRequest {
                action: "reconnect".to_string(),
            })
            .await;

        Ok(responder_bundle.machine_id)
    }

    async fn send_nearby_pairing_request(
        target: &str,
        request: NearbyJoinWireRequest,
    ) -> Result<NearbyJoinWireResponse> {
        let mut socket = timeout(NEARBY_PAIRING_CONNECT_TIMEOUT, TcpStream::connect(target))
            .await
            .with_context(|| {
                format!(
                    "connect nearby pairing endpoint {target} timed out after {}s",
                    NEARBY_PAIRING_CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("connect nearby pairing endpoint {target}"))?;
        let payload =
            serde_json::to_string(&request).context("serialize nearby pairing request")?;
        timeout(
            NEARBY_PAIRING_IO_TIMEOUT,
            socket.write_all(payload.as_bytes()),
        )
        .await
        .with_context(|| {
            format!(
                "send nearby pairing request timed out after {}s",
                NEARBY_PAIRING_IO_TIMEOUT.as_secs()
            )
        })?
        .context("send nearby pairing request")?;
        timeout(NEARBY_PAIRING_IO_TIMEOUT, socket.write_all(b"\n"))
            .await
            .with_context(|| {
                format!(
                    "terminate nearby pairing request timed out after {}s",
                    NEARBY_PAIRING_IO_TIMEOUT.as_secs()
                )
            })?
            .context("terminate nearby pairing request")?;
        timeout(NEARBY_PAIRING_IO_TIMEOUT, socket.flush())
            .await
            .with_context(|| {
                format!(
                    "flush nearby pairing request timed out after {}s",
                    NEARBY_PAIRING_IO_TIMEOUT.as_secs()
                )
            })?
            .context("flush nearby pairing request")?;

        let mut reader = BufReader::new(socket);
        let mut response_line = String::new();
        let read = timeout(
            NEARBY_PAIRING_RESPONSE_TIMEOUT,
            reader.read_line(&mut response_line),
        )
        .await
        .with_context(|| {
            format!(
                "read nearby pairing response timed out after {}s",
                NEARBY_PAIRING_RESPONSE_TIMEOUT.as_secs()
            )
        })?
        .context("read nearby pairing response")?;
        if read == 0 {
            bail!("nearby pairing endpoint closed without a response");
        }
        serde_json::from_str(&response_line).context("parse nearby pairing response")
    }

    async fn channel(endpoint: &str) -> Result<Channel> {
        if let Some(pipe_path) = parse_npipe_endpoint(endpoint)? {
            return Endpoint::from_static("http://[::]:50051")
                .connect_with_connector(NamedPipeConnector::new(pipe_path))
                .await
                .with_context(|| format!("failed to connect to named pipe endpoint {endpoint}"));
        }

        Endpoint::from_shared(endpoint.to_string())
            .with_context(|| format!("invalid endpoint {endpoint}"))?
            .connect()
            .await
            .with_context(|| format!("failed to connect to {endpoint}"))
    }

    fn parse_npipe_endpoint(endpoint: &str) -> Result<Option<String>> {
        let Some(rest) = endpoint.strip_prefix("npipe://") else {
            return Ok(None);
        };
        if let Some(name) = rest.strip_prefix("./pipe/") {
            return pipe_path_from_name(name).map(Some);
        }
        if let Some(name) = rest.strip_prefix(r"\\.\pipe\") {
            return pipe_path_from_name(name).map(Some);
        }
        bail!("invalid named-pipe endpoint {endpoint}; expected npipe://./pipe/<name>");
    }

    fn pipe_path_from_name(name: &str) -> Result<String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("named-pipe endpoint is missing pipe name");
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            bail!("named-pipe endpoint pipe name must not contain path separators");
        }
        Ok(format!(r"\\.\pipe\{trimmed}"))
    }

    #[derive(Clone)]
    struct NamedPipeConnector {
        pipe_path: String,
    }

    impl NamedPipeConnector {
        fn new(pipe_path: String) -> Self {
            Self { pipe_path }
        }
    }

    impl Service<Uri> for NamedPipeConnector {
        type Response = TokioIo<NamedPipeClient>;
        type Error = std::io::Error;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Uri) -> Self::Future {
            let pipe_path = self.pipe_path.clone();
            Box::pin(async move {
                let client = open_named_pipe_with_retry(pipe_path).await?;
                Ok(TokioIo::new(client))
            })
        }
    }

    const ERROR_PIPE_BUSY_CODE: i32 = 231;
    const PIPE_BUSY_MAX_RETRIES: u32 = 20;
    const PIPE_BUSY_BACKOFF_MS: u64 = 25;

    async fn open_named_pipe_with_retry(pipe_path: String) -> std::io::Result<NamedPipeClient> {
        let mut attempt = 0_u32;
        loop {
            match ClientOptions::new().open(pipe_path.as_str()) {
                Ok(client) => return Ok(client),
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE)
                        && attempt < PIPE_BUSY_MAX_RETRIES =>
                {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(PIPE_BUSY_BACKOFF_MS)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn format_host_port(host: &str, port: u16) -> String {
        let trimmed = host.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            format!("{trimmed}:{port}")
        } else if trimmed.contains(':') {
            format!("[{trimmed}]:{port}")
        } else {
            format!("{trimmed}:{port}")
        }
    }

    fn normalize_bundle_address_for_host(bundle: &mut StoredTrustBundle, host: &str) -> Result<()> {
        let port = extract_port_from_network_address(bundle.network_address.trim())?;
        bundle.network_address = format_host_port(host, port);
        Ok(())
    }

    fn extract_port_from_network_address(address: &str) -> Result<u16> {
        let trimmed = address.trim();
        if trimmed.is_empty() {
            bail!("invalid responder network address: empty");
        }
        if let Ok(socket) = trimmed.parse::<std::net::SocketAddr>() {
            return Ok(socket.port());
        }
        if let Some((host_part, port_part)) = trimmed.rsplit_once(':') {
            if host_part.trim().is_empty() {
                bail!("invalid responder network address: missing host");
            }
            let port = port_part
                .trim()
                .parse::<u16>()
                .context("invalid responder network address port")?;
            if port == 0 {
                bail!("invalid responder network address port: 0");
            }
            return Ok(port);
        }
        bail!("invalid responder network address: missing port");
    }

    #[cfg(test)]
    fn resolve_discovered_peer<'a>(
        peers: &'a [UiDiscoveredPeer],
        selector: &str,
    ) -> Result<&'a UiDiscoveredPeer> {
        if let Ok(index) = selector.parse::<usize>() {
            if index == 0 {
                bail!("setup selector index must start at 1");
            }
            return peers
                .get(index - 1)
                .ok_or_else(|| anyhow::anyhow!("no discovered peer at index {index}"));
        }

        let normalized = selector.trim();
        if normalized.is_empty() {
            bail!("setup selector must not be empty");
        }
        let selector_lower = normalized.to_ascii_lowercase();
        let matches = peers
            .iter()
            .filter(|peer| {
                peer.machine_id.eq_ignore_ascii_case(normalized)
                    || peer
                        .machine_id
                        .to_ascii_lowercase()
                        .starts_with(&selector_lower)
                    || peer.display_name.eq_ignore_ascii_case(normalized)
                    || peer
                        .display_name
                        .to_ascii_lowercase()
                        .starts_with(&selector_lower)
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!("no discovered peer matching `{selector}`");
        }
        if matches.len() > 1 {
            bail!("multiple discovered peers match `{selector}`; use full machine_id or index");
        }
        Ok(matches[0])
    }

    fn host_and_pairing_port_from_discovery_endpoint(endpoint: &str) -> Option<(String, u16)> {
        let trimmed = endpoint.trim();
        if trimmed.is_empty() {
            return None;
        }

        if let Ok(socket) = trimmed.parse::<std::net::SocketAddr>() {
            return Some((socket.ip().to_string(), nearby_pairing_port(socket.port())));
        }

        if let Some(host) = trimmed
            .strip_prefix('[')
            .and_then(|value| value.split_once(']'))
            .map(|(host, _)| host.to_string())
        {
            let port = extract_port_from_endpoint(trimmed)?;
            return Some((host, nearby_pairing_port(port)));
        }

        if let Some((host, _)) = trimmed.rsplit_once(':') {
            let host = host.trim();
            if host.is_empty() {
                return None;
            }
            let port = extract_port_from_endpoint(trimmed)?;
            return Some((host.to_string(), nearby_pairing_port(port)));
        }

        None
    }

    fn extract_port_from_endpoint(endpoint: &str) -> Option<u16> {
        endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.trim().parse::<u16>().ok())
            .filter(|port| *port != 0)
    }

    fn parse_pairing_port(value: &str) -> Result<u16> {
        let pairing_port = value
            .parse::<u16>()
            .context("pairing port must be a number in 1..=65535")?;
        if pairing_port == 0 {
            bail!("pairing port must be in 1..=65535");
        }
        Ok(pairing_port)
    }

    fn nearby_pairing_port(transport_port: u16) -> u16 {
        if transport_port <= u16::MAX - 100 {
            return transport_port + 100;
        }
        transport_port.saturating_sub(100).max(1)
    }

    fn resolve_boundlessctl_candidates() -> Vec<String> {
        let mut candidates = Vec::<String>::new();
        if let Ok(path) = std::env::var("BOUNDLESS_CTL_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                candidates.push(trimmed.to_string());
            }
        }

        if let Ok(current_exe) = std::env::current_exe()
            && let Some(parent) = current_exe.parent()
        {
            candidates.push(parent.join("boundlessctl.exe").display().to_string());
            candidates.push(parent.join("boundlessctl").display().to_string());
        }

        candidates.push("boundlessctl.exe".to_string());
        candidates.push("boundlessctl".to_string());
        candidates.sort();
        candidates.dedup();
        candidates
    }

    fn resolve_boundlessd_candidates() -> Vec<String> {
        let mut candidates = Vec::<String>::new();
        if let Ok(path) = std::env::var("BOUNDLESS_DAEMON_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                candidates.push(trimmed.to_string());
            }
        }

        if let Ok(current_exe) = std::env::current_exe()
            && let Some(parent) = current_exe.parent()
        {
            candidates.push(parent.join("boundlessd.exe").display().to_string());
            candidates.push(parent.join("boundlessd").display().to_string());
        }

        candidates.push("boundlessd.exe".to_string());
        candidates.push("boundlessd".to_string());
        candidates.sort();
        candidates.dedup();
        candidates
    }

    fn make_tray_icon() -> Result<Icon> {
        let width = 16_u32;
        let height = 16_u32;
        let mut rgba = vec![0_u8; (width * height * 4) as usize];

        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                let is_border = x == 0 || y == 0 || x == width - 1 || y == height - 1;
                let is_cross = x == width / 2 || y == height / 2;

                let (r, g, b) = if is_border {
                    (220, 224, 228)
                } else if is_cross {
                    (78, 148, 188)
                } else {
                    (24, 30, 36)
                };
                rgba[idx] = r;
                rgba[idx + 1] = g;
                rgba[idx + 2] = b;
                rgba[idx + 3] = 255;
            }
        }

        Icon::from_rgba(rgba, width, height).context("create tray icon image")
    }

    fn short_token(value: &str) -> &str {
        value.get(..8).unwrap_or(value)
    }

    fn format_error_for_dialog(error: &anyhow::Error) -> String {
        let message = error.to_string();
        let lowered = message.to_ascii_lowercase();

        if lowered.contains("attempts_remaining=") {
            if let Some(attempts_remaining) = extract_attempts_remaining(&message) {
                return format!(
                    "{message}\n\nCode confirmation failed.\nDouble-check the 6-digit code and retry.\nAttempts remaining: {attempts_remaining}."
                );
            }
            return format!(
                "{message}\n\nCode confirmation failed.\nDouble-check the 6-digit code and retry."
            );
        }

        if lowered.contains("temporarily locked") {
            return format!(
                "{message}\n\nToo many invalid attempts were submitted.\nWait for lockout to expire, then start a new pairing request."
            );
        }

        if lowered.contains("verification nonce is invalid")
            || lowered.contains("verification code and nonce are invalid")
        {
            return format!(
                "{message}\n\nThis pairing request is stale or mismatched.\nStart a new request and enter the fresh code from the target machine."
            );
        }

        if lowered.contains("pairing request rejected") {
            return format!(
                "{message}\n\nThe target rejected the request.\nStart a new pairing request from the tray and confirm on the target machine."
            );
        }

        if lowered.contains("timed out waiting for nearby pairing approval") {
            return format!(
                "{message}\n\nThe target did not approve in time.\nStart a new pairing request and approve it on the target before timeout."
            );
        }

        if lowered.contains("nearby code request rate limited") {
            return format!(
                "{message}\n\nCode requests are briefly rate-limited.\nWait a few seconds and retry."
            );
        }

        if lowered.contains("nearby pairing endpoint closed without a response") {
            return format!(
                "{message}\n\nThe remote pairing service did not respond.\nVerify both trays are updated and retry."
            );
        }
        if lowered.contains("read nearby pairing response timed out")
            || lowered.contains("connect nearby pairing endpoint")
            || lowered.contains("send nearby pairing request timed out")
        {
            return format!(
                "{message}\n\nThe remote pairing service stalled.\nRetry pairing. If this repeats, restart the target daemon and tray."
            );
        }

        message
    }

    fn should_offer_new_request_retry(error: &anyhow::Error) -> bool {
        let lowered = error.to_string().to_ascii_lowercase();
        lowered.contains("pairing request rejected")
            || lowered.contains("verification code expired")
            || lowered.contains("timed out waiting for nearby pairing approval")
            || lowered.contains("nearby pairing request not found")
            || lowered.contains("nearby pairing endpoint closed without a response")
            || lowered.contains("read nearby pairing response timed out")
            || lowered.contains("connect nearby pairing endpoint")
            || lowered.contains("send nearby pairing request timed out")
    }

    fn extract_attempts_remaining(message: &str) -> Option<u8> {
        const MARKER: &str = "attempts_remaining=";
        let marker_index = message.find(MARKER)?;
        let start = marker_index + MARKER.len();
        let digits = message[start..]
            .chars()
            .take_while(|char| char.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        digits.parse::<u8>().ok()
    }

    fn truncate(value: &str, max_chars: usize) -> String {
        let mut out = value.chars().take(max_chars).collect::<String>();
        if value.chars().count() > max_chars {
            out.push_str("...");
        }
        out
    }

    fn default_endpoint() -> String {
        "npipe://./pipe/boundlessd-api".to_string()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn extract_attempts_remaining_reads_numeric_suffix() {
            let message = "verification code is invalid; attempts_remaining=4";
            assert_eq!(extract_attempts_remaining(message), Some(4));
        }

        #[test]
        fn extract_attempts_remaining_ignores_missing_marker() {
            assert_eq!(extract_attempts_remaining("no attempts here"), None);
        }

        #[test]
        fn resolve_discovered_peer_supports_index_and_prefix() {
            let peers = vec![
                UiDiscoveredPeer {
                    machine_id: "machine-alpha-1234".to_string(),
                    display_name: "Office Desktop".to_string(),
                    endpoint: "10.10.0.10:15100".to_string(),
                },
                UiDiscoveredPeer {
                    machine_id: "machine-bravo-5678".to_string(),
                    display_name: "Living Room".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
                },
            ];

            let by_index = resolve_discovered_peer(&peers, "1").expect("peer by index");
            assert_eq!(by_index.machine_id, "machine-alpha-1234");

            let by_prefix = resolve_discovered_peer(&peers, "living").expect("peer by prefix");
            assert_eq!(by_prefix.machine_id, "machine-bravo-5678");
        }

        #[test]
        fn resolve_discovered_peer_rejects_ambiguous_matches() {
            let peers = vec![
                UiDiscoveredPeer {
                    machine_id: "machine-alpha-1234".to_string(),
                    display_name: "Office".to_string(),
                    endpoint: "10.10.0.10:15100".to_string(),
                },
                UiDiscoveredPeer {
                    machine_id: "machine-beta-5678".to_string(),
                    display_name: "Office Laptop".to_string(),
                    endpoint: "10.10.0.11:15100".to_string(),
                },
            ];

            let error =
                resolve_discovered_peer(&peers, "office").expect_err("must reject ambiguous");
            assert!(
                error
                    .to_string()
                    .contains("multiple discovered peers match"),
                "ambiguous selector should be rejected"
            );
        }

        #[test]
        fn parse_pairing_port_validates_range() {
            assert_eq!(parse_pairing_port("15200").expect("valid port"), 15200);
            assert!(
                parse_pairing_port("0").is_err(),
                "port zero must be rejected"
            );
            assert!(
                parse_pairing_port("not-a-number").is_err(),
                "non-numeric input must be rejected"
            );
        }

        #[test]
        fn should_offer_new_request_retry_matches_rejected_and_timeout() {
            let rejected =
                anyhow::anyhow!("verification code is invalid; pairing request rejected");
            assert!(
                should_offer_new_request_retry(&rejected),
                "rejected requests should offer retry"
            );

            let timeout =
                anyhow::anyhow!("timed out waiting for nearby pairing approval request_id=abc");
            assert!(
                should_offer_new_request_retry(&timeout),
                "timeout should offer retry"
            );
        }

        #[test]
        fn should_offer_new_request_retry_ignores_lockout() {
            let lockout = anyhow::anyhow!(
                "verification temporarily locked after repeated invalid attempts; retry later"
            );
            assert!(
                !should_offer_new_request_retry(&lockout),
                "lockout should not offer immediate retry"
            );
        }

        #[test]
        fn should_offer_new_request_retry_matches_transport_stall_signals() {
            let endpoint_closed =
                anyhow::anyhow!("nearby pairing endpoint closed without a response");
            assert!(
                should_offer_new_request_retry(&endpoint_closed),
                "closed endpoint should offer retry"
            );

            let response_timeout =
                anyhow::anyhow!("read nearby pairing response timed out after 20s");
            assert!(
                should_offer_new_request_retry(&response_timeout),
                "response timeout should offer retry"
            );
        }

        #[test]
        fn layout_local_token_recognizes_canonical_aliases_and_machine_id() {
            let machine_id = "local-machine-id";
            assert!(is_local_layout_token("self", machine_id));
            assert!(is_local_layout_token("local", machine_id));
            assert!(is_local_layout_token("this", machine_id));
            assert!(is_local_layout_token("me", machine_id));
            assert!(is_local_layout_token("LOCAL-MACHINE-ID", machine_id));
        }

        #[test]
        fn layout_local_token_rejects_legacy_this_pc() {
            assert!(!is_local_layout_token("THIS-PC", "local-machine-id"));
        }

        #[test]
        fn layout_serialization_uses_canonical_self_token() {
            let mut grid = std::collections::HashMap::<(i32, i32), String>::new();
            grid.insert((1, 1), "local-machine-id".to_string());
            grid.insert((2, 1), "peer-right".to_string());

            let matrix = serialize_layout_matrix(&grid, "local-machine-id");
            assert_eq!(matrix, "self,peer-right");
        }

        #[test]
        fn layout_apply_validation_requires_exactly_one_local_cell() {
            let mut grid = std::collections::HashMap::<(i32, i32), String>::new();
            grid.insert((0, 0), "peer-a".to_string());
            assert!(
                validate_layout_before_apply(&grid, "local-machine-id").is_err(),
                "layout with zero local cells must fail apply validation"
            );

            grid.insert((1, 0), "local-machine-id".to_string());
            assert!(
                validate_layout_before_apply(&grid, "local-machine-id").is_ok(),
                "layout with one local cell should pass apply validation"
            );

            grid.insert((2, 0), "LOCAL-MACHINE-ID".to_string());
            assert!(
                validate_layout_before_apply(&grid, "local-machine-id").is_err(),
                "layout with multiple local cells must fail apply validation"
            );
        }
    }
}
