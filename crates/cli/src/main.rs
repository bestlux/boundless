use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use core_clipboard::validate_bmp_payload as validate_bmp_bytes;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    net::SocketAddr,
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};
use tonic::transport::{Channel, Endpoint};

#[cfg(windows)]
use hyper_util::rt::TokioIo;
#[cfg(windows)]
use std::{
    future::Future,
    os::windows::process::CommandExt,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
#[cfg(windows)]
use tonic::{codegen::Service, transport::Uri};

use ipc_api::boundless::v1::{
    DiagnosticsDumpRequest, Empty, FeatureSetRequest, HotkeySetRequest, HotkeyTriggerRequest,
    ImportTrustBundleRequest, InputCaptureTargetRequest, InputOwnerRequest, LayoutSetRequest,
    NearbyPairingDecisionRequest, PairCreateCodeRequest, PairJoinRequest, RemovePeerRequest,
    SafeResetRequest, SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest,
    SendInputKeyRequest, SendInputMoveRequest, StatusReply, StatusRequest,
    daemon_service_client::DaemonServiceClient,
    diagnostics_service_client::DiagnosticsServiceClient,
    feature_service_client::FeatureServiceClient, pairing_service_client::PairingServiceClient,
    topology_service_client::TopologyServiceClient,
};

mod cli_helpers;
mod commands;

#[cfg(windows)]
use cli_helpers::NamedPipeConnector;
#[cfg(test)]
use cli_helpers::extract_port_from_network_address;
#[cfg(all(test, windows))]
use cli_helpers::is_pipe_busy_error;
use cli_helpers::{
    format_host_port, nearby_pairing_port, normalize_bundle_address_for_host, parse_npipe_endpoint,
    prompt_pairing_code, resolve_discovered_peer, send_nearby_pairing_request, short_machine_id,
    validate_bmp_payload,
};
use commands::*;

#[derive(Debug, Parser)]
#[command(name = "boundlessctl", version, about = "Boundless CLI")]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "BOUNDLESS_API_ENDPOINT",
        default_value_t = default_endpoint()
    )]
    endpoint: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Console {
        #[arg(long, default_value_t = true)]
        start_daemon: bool,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Pair {
        #[command(subcommand)]
        command: PairCommand,
    },
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    Feature {
        #[command(subcommand)]
        command: FeatureCommand,
    },
    Transport {
        #[command(subcommand)]
        command: TransportCommand,
    },
    Input {
        #[command(subcommand)]
        command: InputCommand,
    },
    Hotkey {
        action: String,
        combo: String,
    },
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
    SafeReset {
        #[arg(long, default_value_t = false)]
        network: bool,
        #[arg(long, default_value_t = false)]
        all: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Status,
}

#[derive(Debug, Subcommand)]
enum PairCommand {
    CreateCode {
        #[arg(long, default_value_t = 300)]
        ttl: u32,
    },
    Join {
        code: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        alias: Option<String>,
    },
    NearbyJoin {
        code: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 15200)]
        port: u16,
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
        #[arg(long)]
        alias: Option<String>,
    },
    Pending,
    Approve {
        request_id: String,
        #[arg(long)]
        alias: Option<String>,
    },
    Reject {
        request_id: String,
    },
    ExportTrust {
        #[arg(long)]
        output: Option<String>,
    },
    ImportTrust {
        #[arg(long)]
        input: String,
        #[arg(long)]
        alias: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    List,
    Remove { peer_id: String },
}

#[derive(Debug, Subcommand)]
enum LayoutCommand {
    Show,
    Set { matrix: String },
}

#[derive(Debug, Subcommand)]
enum FeatureCommand {
    List,
    Set { name: String, value: ToggleValue },
}

#[derive(Debug, Subcommand)]
enum TransportCommand {
    SendText {
        peer_id: String,
        text: String,
    },
    SendImage {
        peer_id: String,
        path: String,
    },
    SendFile {
        peer_id: String,
        path: String,
    },
    Events {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum InputCommand {
    Owner,
    CaptureTarget,
    CaptureStart {
        peer_id: String,
    },
    CaptureStop,
    SendMove {
        peer_id: String,
        dx: i32,
        dy: i32,
    },
    SendKey {
        peer_id: String,
        scan_code: u16,
        state: InputKeyState,
    },
    Claim {
        peer_id: String,
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    Release {
        peer_id: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ToggleValue {
    On,
    Off,
}

impl ToggleValue {
    fn as_bool(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum InputKeyState {
    Down,
    Up,
}

impl InputKeyState {
    fn is_down(self) -> bool {
        matches!(self, Self::Down)
    }
}

#[derive(Debug, Subcommand)]
enum DiagnosticsCommand {
    Dump {
        #[arg(long)]
        output: Option<String>,
    },
    RunAction {
        action: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTrustBundle {
    machine_id: String,
    display_name: String,
    network_address: String,
    ca_cert_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum NearbyJoinWireRequest {
    NearbyJoin {
        code: String,
        requester_bundle: StoredTrustBundle,
        requester_alias: Option<String>,
    },
    CheckNearbyJoin {
        request_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum NearbyJoinWireResponse {
    Pending {
        request_id: String,
        message: String,
    },
    Approved {
        request_id: String,
        message: String,
        responder_bundle: StoredTrustBundle,
    },
    Rejected {
        request_id: String,
        message: String,
    },
    Error {
        message: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Console { start_daemon } => console_run(&cli.endpoint, start_daemon).await,
        Command::Daemon { command } => match command {
            DaemonCommand::Status => daemon_status(&cli.endpoint).await,
        },
        Command::Pair { command } => match command {
            PairCommand::CreateCode { ttl } => pair_create_code(&cli.endpoint, ttl).await,
            PairCommand::Join { code, host, alias } => {
                pair_join(&cli.endpoint, code, host, alias).await
            }
            PairCommand::NearbyJoin {
                code,
                host,
                port,
                timeout_seconds,
                alias,
            } => pair_nearby_join(&cli.endpoint, code, host, port, timeout_seconds, alias).await,
            PairCommand::Pending => pair_pending(&cli.endpoint).await,
            PairCommand::Approve { request_id, alias } => {
                pair_approve(&cli.endpoint, request_id, alias).await
            }
            PairCommand::Reject { request_id } => pair_reject(&cli.endpoint, request_id).await,
            PairCommand::ExportTrust { output } => pair_export_trust(&cli.endpoint, output).await,
            PairCommand::ImportTrust { input, alias } => {
                pair_import_trust(&cli.endpoint, input, alias).await
            }
        },
        Command::Peer { command } => match command {
            PeerCommand::List => peer_list(&cli.endpoint).await,
            PeerCommand::Remove { peer_id } => peer_remove(&cli.endpoint, peer_id).await,
        },
        Command::Layout { command } => match command {
            LayoutCommand::Show => layout_show(&cli.endpoint).await,
            LayoutCommand::Set { matrix } => layout_set(&cli.endpoint, matrix).await,
        },
        Command::Feature { command } => match command {
            FeatureCommand::List => feature_list(&cli.endpoint).await,
            FeatureCommand::Set { name, value } => feature_set(&cli.endpoint, name, value).await,
        },
        Command::Transport { command } => match command {
            TransportCommand::SendText { peer_id, text } => {
                transport_send_text(&cli.endpoint, peer_id, text).await
            }
            TransportCommand::SendImage { peer_id, path } => {
                transport_send_image(&cli.endpoint, peer_id, path).await
            }
            TransportCommand::SendFile { peer_id, path } => {
                transport_send_file(&cli.endpoint, peer_id, path).await
            }
            TransportCommand::Events { limit } => transport_events(&cli.endpoint, limit).await,
        },
        Command::Input { command } => match command {
            InputCommand::Owner => input_owner(&cli.endpoint).await,
            InputCommand::CaptureTarget => input_capture_target(&cli.endpoint).await,
            InputCommand::CaptureStart { peer_id } => {
                input_capture_start(&cli.endpoint, peer_id).await
            }
            InputCommand::CaptureStop => input_capture_stop(&cli.endpoint).await,
            InputCommand::SendMove { peer_id, dx, dy } => {
                input_send_move(&cli.endpoint, peer_id, dx, dy).await
            }
            InputCommand::SendKey {
                peer_id,
                scan_code,
                state,
            } => input_send_key(&cli.endpoint, peer_id, scan_code, state).await,
            InputCommand::Claim { peer_id, force } => {
                input_claim(&cli.endpoint, peer_id, force).await
            }
            InputCommand::Release { peer_id } => input_release(&cli.endpoint, peer_id).await,
        },
        Command::Hotkey { action, combo } => hotkey_set(&cli.endpoint, action, combo).await,
        Command::Diagnostics { command } => match command {
            DiagnosticsCommand::Dump { output } => diagnostics_dump(&cli.endpoint, output).await,
            DiagnosticsCommand::RunAction { action } => {
                diagnostics_run_action(&cli.endpoint, action).await
            }
        },
        Command::SafeReset { network, all } => safe_reset(&cli.endpoint, network, all).await,
    }
}

fn default_endpoint() -> String {
    if cfg!(windows) {
        "npipe://./pipe/boundlessd-api".to_string()
    } else {
        "http://127.0.0.1:50051".to_string()
    }
}

async fn channel(endpoint: &str) -> Result<Channel> {
    if let Some(pipe_path) = parse_npipe_endpoint(endpoint)? {
        #[cfg(windows)]
        {
            return Endpoint::from_static("http://[::]:50051")
                .connect_with_connector(NamedPipeConnector::new(pipe_path))
                .await
                .with_context(|| format!("failed to connect to named pipe endpoint {endpoint}"));
        }

        #[cfg(not(windows))]
        {
            let _ = pipe_path;
            bail!("named-pipe endpoint is only supported on Windows: {endpoint}");
        }
    }

    Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("invalid endpoint {endpoint}"))?
        .connect()
        .await
        .with_context(|| format!("failed to connect to {endpoint}"))
}

#[derive(Debug)]
struct ConsolePeer {
    peer_id: String,
    display_name: String,
    address: String,
    connected: bool,
}

#[derive(Debug)]
struct ConsoleDiscoveredPeer {
    machine_id: String,
    display_name: String,
    endpoint: String,
}

#[derive(Debug)]
struct ConsolePendingRequest {
    request_id: String,
    requester_display_name: String,
}

#[derive(Debug)]
struct ConsoleSnapshot {
    status: StatusReply,
    peers: Vec<ConsolePeer>,
    features: Vec<(String, bool)>,
    discovered_peers: Vec<ConsoleDiscoveredPeer>,
    pending_requests: Vec<ConsolePendingRequest>,
    input_owner: Option<String>,
    capture_target: Option<String>,
    mdns_active: bool,
}

impl ConsoleSnapshot {
    fn feature_enabled(&self, name: &str) -> Option<bool> {
        self.features
            .iter()
            .find(|(feature, _)| feature == name)
            .map(|(_, enabled)| *enabled)
    }
}

async fn console_run(endpoint: &str, start_daemon: bool) -> Result<()> {
    ensure_daemon_available(endpoint, start_daemon).await?;

    println!("Boundless interactive console");
    println!("endpoint={endpoint}");
    println!("type `help` for commands, `q` to exit");

    loop {
        let snapshot = fetch_console_snapshot(endpoint).await?;
        print_console_snapshot(endpoint, &snapshot);

        print!("boundless> ");
        io::stdout().flush().context("flush stdout")?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("read console input")?;
        let line = line.trim();

        if line.is_empty() || line.eq_ignore_ascii_case("r") || line.eq_ignore_ascii_case("refresh")
        {
            continue;
        }
        if line.eq_ignore_ascii_case("q")
            || line.eq_ignore_ascii_case("quit")
            || line.eq_ignore_ascii_case("exit")
        {
            break;
        }
        if line.eq_ignore_ascii_case("help") {
            print_console_help();
            continue;
        }

        if let Err(error) = handle_console_command(endpoint, &snapshot, line).await {
            eprintln!("error: {error:#}");
        }
    }

    Ok(())
}

async fn ensure_daemon_available(endpoint: &str, start_daemon: bool) -> Result<()> {
    if channel(endpoint).await.is_ok() {
        return Ok(());
    }

    if !start_daemon {
        bail!("daemon is not reachable at {endpoint}; run boundlessd or pass --start-daemon");
    }

    let launched = spawn_daemon_process()?;
    println!("daemon_start=spawned path={launched}");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match channel(endpoint).await {
            Ok(_) => return Ok(()),
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

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn spawn_daemon_process() -> Result<String> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("BOUNDLESS_DAEMON_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        #[cfg(windows)]
        {
            candidates.push(parent.join("boundlessd.exe").display().to_string());
        }
        #[cfg(not(windows))]
        {
            candidates.push(parent.join("boundlessd").display().to_string());
        }
    }

    candidates.push("boundlessd".to_string());
    #[cfg(windows)]
    candidates.push("boundlessd.exe".to_string());

    candidates.sort();
    candidates.dedup();

    let mut errors = Vec::new();
    for candidate in candidates {
        let mut command = ProcessCommand::new(&candidate);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        match command.spawn() {
            Ok(_) => return Ok(candidate),
            Err(error) => errors.push(format!("{candidate}: {error}")),
        }
    }

    bail!(
        "failed to start boundlessd; candidates attempted: {}",
        errors.join("; ")
    )
}

async fn fetch_console_snapshot(endpoint: &str) -> Result<ConsoleSnapshot> {
    let mut daemon_client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = daemon_client
        .get_status(StatusRequest {})
        .await?
        .into_inner();

    let mut topology_client = TopologyServiceClient::new(channel(endpoint).await?);
    let peers = topology_client
        .list_peers(Empty {})
        .await?
        .into_inner()
        .peers
        .into_iter()
        .map(|peer| ConsolePeer {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            address: peer.address,
            connected: peer.connected,
        })
        .collect::<Vec<_>>();

    let mut feature_client = FeatureServiceClient::new(channel(endpoint).await?);
    let mut features = feature_client
        .list_features(Empty {})
        .await?
        .into_inner()
        .features
        .into_iter()
        .collect::<Vec<_>>();
    features.sort_by(|a, b| a.0.cmp(&b.0));

    let mut diagnostics_client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let discovery = diagnostics_client
        .list_discovery_peers(Empty {})
        .await?
        .into_inner();
    let discovered_peers = discovery
        .peers
        .into_iter()
        .map(|peer| ConsoleDiscoveredPeer {
            machine_id: peer.machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint,
        })
        .collect::<Vec<_>>();

    let owner = diagnostics_client
        .get_input_owner(Empty {})
        .await?
        .into_inner();
    let input_owner = if owner.owner_peer_id.trim().is_empty() {
        None
    } else {
        Some(owner.owner_peer_id)
    };

    let capture = diagnostics_client
        .get_input_capture_target(Empty {})
        .await?
        .into_inner();
    let capture_target = if capture.peer_id.trim().is_empty() {
        None
    } else {
        Some(capture.peer_id)
    };

    let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
    let pending_requests = pairing_client
        .list_nearby_pairing_requests(Empty {})
        .await?
        .into_inner()
        .requests
        .into_iter()
        .map(|request| ConsolePendingRequest {
            request_id: request.request_id,
            requester_display_name: request.requester_display_name,
        })
        .collect::<Vec<_>>();

    Ok(ConsoleSnapshot {
        status,
        peers,
        features,
        discovered_peers,
        pending_requests,
        input_owner,
        capture_target,
        mdns_active: discovery.mdns_active,
    })
}

fn print_console_snapshot(endpoint: &str, snapshot: &ConsoleSnapshot) {
    println!();
    println!("=== Boundless Status ===");
    println!(
        "daemon=running endpoint={} machine_id={} protocol={} input_locked={} input_lock_supported={} active_capture_target={}",
        endpoint,
        snapshot.status.machine_id,
        snapshot.status.protocol_version,
        snapshot.status.input_locked,
        snapshot.status.input_lock_supported,
        if snapshot.status.capture_target_peer_id.is_empty() {
            "none"
        } else {
            snapshot.status.capture_target_peer_id.as_str()
        }
    );
    println!(
        "api_transport={} api_bind={} api_pipe_name={}",
        snapshot.status.api_transport, snapshot.status.api_bind, snapshot.status.api_pipe_name
    );

    let mdns = if snapshot.mdns_active {
        "searching"
    } else {
        "stopped"
    };
    println!(
        "mdns={} discovered_peers={}",
        mdns,
        snapshot.discovered_peers.len()
    );
    for (index, peer) in snapshot.discovered_peers.iter().enumerate() {
        println!(
            "  [{}] discovered name={} endpoint={} machine_id={}",
            index + 1,
            peer.display_name,
            peer.endpoint,
            short_machine_id(&peer.machine_id),
        );
    }

    println!("trusted_peers={}", snapshot.peers.len());
    for peer in &snapshot.peers {
        let owner = if snapshot.input_owner.as_deref() == Some(peer.peer_id.as_str()) {
            " owner"
        } else {
            ""
        };
        let capture = if snapshot.capture_target.as_deref() == Some(peer.peer_id.as_str()) {
            " capture"
        } else {
            ""
        };
        println!(
            "  peer_id={} name={} address={} connected={}{}{}",
            peer.peer_id, peer.display_name, peer.address, peer.connected, owner, capture
        );
    }

    println!("features:");
    for (name, enabled) in &snapshot.features {
        println!("  {name}={enabled}");
    }

    println!(
        "input_owner={} capture_target={} pending_pair_requests={}",
        snapshot.input_owner.as_deref().unwrap_or("none"),
        snapshot.capture_target.as_deref().unwrap_or("none"),
        snapshot.pending_requests.len()
    );
    for request in &snapshot.pending_requests {
        println!(
            "  pending request_id={} requester={}",
            request.request_id, request.requester_display_name
        );
    }
}

fn print_console_help() {
    println!("commands:");
    println!("  refresh | r");
    println!("  quit | q");
    println!("  toggle <feature_name>");
    println!("  set <feature_name> <on|off>");
    println!("  control <peer_id|off>");
    println!("  reconnect");
    println!("  pair code [ttl_seconds]");
    println!("  pair pending");
    println!("  pair approve <request_id> [alias]");
    println!("  pair reject <request_id>");
    println!("  pair request <index|machine_id> [code] [alias]");
    println!("  pair nearby <host> <code> [port] [alias]");
}

async fn handle_console_command(
    endpoint: &str,
    snapshot: &ConsoleSnapshot,
    line: &str,
) -> Result<()> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        return Ok(());
    }

    match parts[0] {
        "toggle" => {
            if parts.len() != 2 {
                bail!("usage: toggle <feature_name>");
            }
            let name = parts[1];
            let current = snapshot
                .feature_enabled(name)
                .ok_or_else(|| anyhow::anyhow!("unknown feature {name}"))?;
            let next = if current {
                ToggleValue::Off
            } else {
                ToggleValue::On
            };
            feature_set(endpoint, name.to_string(), next).await
        }
        "set" => {
            if parts.len() != 3 {
                bail!("usage: set <feature_name> <on|off>");
            }
            let value = match parts[2] {
                "on" | "true" | "1" => ToggleValue::On,
                "off" | "false" | "0" => ToggleValue::Off,
                _ => bail!("value must be on|off"),
            };
            feature_set(endpoint, parts[1].to_string(), value).await
        }
        "control" => {
            if parts.len() != 2 {
                bail!("usage: control <peer_id|off>");
            }
            let target = parts[1];
            if target.eq_ignore_ascii_case("off") {
                input_capture_stop(endpoint).await?;
                if let Some(owner_peer_id) = &snapshot.input_owner {
                    let _ = input_release(endpoint, owner_peer_id.clone()).await;
                }
                println!("control=off");
                Ok(())
            } else {
                if snapshot.feature_enabled("share_input") == Some(false) {
                    feature_set(endpoint, "share_input".to_string(), ToggleValue::On).await?;
                }
                input_capture_start(endpoint, target.to_string()).await?;
                input_claim(endpoint, target.to_string(), false).await?;
                println!("control=on peer_id={target}");
                Ok(())
            }
        }
        "reconnect" => diagnostics_run_action(endpoint, "reconnect".to_string()).await,
        "pair" => handle_console_pair_command(endpoint, snapshot, &parts[1..]).await,
        _ => bail!("unknown command `{}`; run `help`", parts[0]),
    }
}

async fn handle_console_pair_command(
    endpoint: &str,
    snapshot: &ConsoleSnapshot,
    args: &[&str],
) -> Result<()> {
    if args.is_empty() {
        bail!("usage: pair <code|pending|approve|reject|request|nearby> ...");
    }

    match args[0] {
        "code" => {
            let ttl = if args.len() >= 2 {
                args[1]
                    .parse::<u32>()
                    .context("pair code ttl must be a positive integer")?
            } else {
                300
            };
            pair_create_code(endpoint, ttl).await
        }
        "pending" => pair_pending(endpoint).await,
        "approve" => {
            if args.len() < 2 {
                bail!("usage: pair approve <request_id> [alias]");
            }
            let alias = args.get(2).map(|value| (*value).to_string());
            let request_id = if args[1].eq_ignore_ascii_case("latest") {
                snapshot
                    .pending_requests
                    .last()
                    .map(|request| request.request_id.clone())
                    .ok_or_else(|| anyhow::anyhow!("no pending pairing requests"))?
            } else {
                args[1].to_string()
            };
            pair_approve(endpoint, request_id, alias).await
        }
        "reject" => {
            if args.len() != 2 {
                bail!("usage: pair reject <request_id>");
            }
            pair_reject(endpoint, args[1].to_string()).await
        }
        "request" => {
            if args.len() < 2 {
                bail!("usage: pair request <index|machine_id> [code] [alias]");
            }

            let discovered = resolve_discovered_peer(snapshot, args[1])?;
            let socket = discovered
                .endpoint
                .parse::<SocketAddr>()
                .with_context(|| format!("invalid discovered endpoint {}", discovered.endpoint))?;
            let host = socket.ip().to_string();
            let pairing_port = nearby_pairing_port(socket.port());

            let code = if let Some(code) = args.get(2) {
                code.to_string()
            } else {
                prompt_pairing_code()?
            };
            if code.trim().is_empty() {
                bail!("pairing code must not be empty");
            }

            let alias = args.get(3).map(|value| (*value).to_string());
            println!(
                "pair_request target={} endpoint={} pairing_port={} machine_id={}",
                discovered.display_name,
                discovered.endpoint,
                pairing_port,
                short_machine_id(&discovered.machine_id),
            );
            pair_nearby_join(endpoint, code, host, pairing_port, 120, alias).await
        }
        "nearby" => {
            if args.len() < 3 {
                bail!("usage: pair nearby <host> <code> [port] [alias]");
            }
            let host = args[1].to_string();
            let code = args[2].to_string();
            let mut port = 15200_u16;
            let mut alias = None;
            if let Some(arg) = args.get(3) {
                if let Ok(parsed) = arg.parse::<u16>() {
                    port = parsed;
                    if let Some(alias_arg) = args.get(4) {
                        alias = Some((*alias_arg).to_string());
                    }
                    if args.len() > 5 {
                        bail!("usage: pair nearby <host> <code> [port] [alias]");
                    }
                } else {
                    alias = Some((*arg).to_string());
                    if args.len() > 4 {
                        bail!("usage: pair nearby <host> <code> [port] [alias]");
                    }
                }
            }
            pair_nearby_join(endpoint, code, host, port, 120, alias).await
        }
        _ => bail!("unknown pair command `{}`", args[0]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_npipe_endpoint_accepts_pipe_name() {
        let path = parse_npipe_endpoint("npipe://./pipe/boundlessd-api")
            .expect("parse")
            .expect("npipe");
        assert_eq!(path, r"\\.\pipe\boundlessd-api");
    }

    #[test]
    fn parse_npipe_endpoint_rejects_invalid_shape() {
        let err = parse_npipe_endpoint("npipe://boundlessd-api").expect_err("must fail");
        assert!(err.to_string().contains("expected npipe://./pipe/<name>"));
    }

    #[test]
    fn parse_npipe_endpoint_ignores_http_endpoint() {
        let parsed = parse_npipe_endpoint("http://127.0.0.1:50051").expect("parse");
        assert!(parsed.is_none());
    }

    #[test]
    fn validate_bmp_payload_rejects_non_bmp() {
        let err = validate_bmp_payload(&[0, 1, 2]).expect_err("must fail");
        assert!(err.to_string().contains("too small"));

        let mut invalid_signature = vec![0u8; 54];
        invalid_signature[0] = b'P';
        invalid_signature[1] = b'N';
        let err = validate_bmp_payload(&invalid_signature).expect_err("must fail");
        assert!(err.to_string().contains("BM"));
    }

    #[test]
    fn validate_bmp_payload_rejects_truncated_bitmap() {
        let payload = [
            b'B', b'M', 54, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 100, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let err = validate_bmp_payload(&payload).expect_err("must fail");
        assert!(err.to_string().contains("pixel"));
    }

    #[test]
    fn validate_bmp_payload_accepts_minimal_bmp() {
        let payload = [
            b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 255, 0,
        ];
        validate_bmp_payload(&payload).expect("must accept");
    }

    #[test]
    fn format_host_port_handles_ipv4_and_ipv6() {
        assert_eq!(format_host_port("10.0.0.7", 15200), "10.0.0.7:15200");
        assert_eq!(format_host_port("fe80::1", 15200), "[fe80::1]:15200");
        assert_eq!(format_host_port("[fe80::1]", 15200), "[fe80::1]:15200");
    }

    #[test]
    fn extract_port_from_network_address_accepts_hostname_with_port() {
        assert_eq!(
            extract_port_from_network_address("DESKTOP-ABC:15100").expect("port"),
            15100
        );
        assert_eq!(
            extract_port_from_network_address("[fe80::1%4]:17100").expect("port"),
            17100
        );
    }

    #[test]
    fn nearby_pairing_port_uses_offset_and_overflow_fallback() {
        assert_eq!(nearby_pairing_port(15100), 15200);
        assert_eq!(nearby_pairing_port(65436), 65336);
    }

    #[test]
    fn resolve_discovered_peer_supports_index_and_prefix_selector() {
        let snapshot = ConsoleSnapshot {
            status: StatusReply::default(),
            peers: Vec::new(),
            features: Vec::new(),
            discovered_peers: vec![
                ConsoleDiscoveredPeer {
                    machine_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                    display_name: "MACHINE-A".to_string(),
                    endpoint: "10.0.0.10:15100".to_string(),
                },
                ConsoleDiscoveredPeer {
                    machine_id: "11111111-2222-3333-4444-555555555555".to_string(),
                    display_name: "MACHINE-B".to_string(),
                    endpoint: "10.0.0.11:15100".to_string(),
                },
            ],
            pending_requests: Vec::new(),
            input_owner: None,
            capture_target: None,
            mdns_active: true,
        };

        let by_index = resolve_discovered_peer(&snapshot, "2").expect("index");
        assert_eq!(by_index.display_name, "MACHINE-B");
        let by_prefix = resolve_discovered_peer(&snapshot, "aaaaaaaa").expect("prefix");
        assert_eq!(by_prefix.display_name, "MACHINE-A");
    }

    #[cfg(windows)]
    #[test]
    fn detects_pipe_busy_error_code() {
        let busy = std::io::Error::from_raw_os_error(231);
        let other = std::io::Error::from_raw_os_error(5);
        assert!(is_pipe_busy_error(&busy));
        assert!(!is_pipe_busy_error(&other));
    }
}
