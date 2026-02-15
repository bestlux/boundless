use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use core_clipboard::validate_bmp_payload as validate_bmp_bytes;
use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, Endpoint};

#[cfg(windows)]
use hyper_util::rt::TokioIo;
#[cfg(windows)]
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
#[cfg(windows)]
use tonic::{codegen::Service, transport::Uri};

use ipc_api::boundless::v1::{
    DiagnosticsDumpRequest, Empty, FeatureSetRequest, HotkeySetRequest, ImportTrustBundleRequest,
    InputCaptureTargetRequest, InputOwnerRequest, LayoutSetRequest, PairCreateCodeRequest,
    PairJoinRequest, RemovePeerRequest, SafeResetRequest, SendClipboardImageRequest,
    SendClipboardTextRequest, SendFileRequest, SendInputKeyRequest, SendInputMoveRequest,
    StatusRequest, daemon_service_client::DaemonServiceClient,
    diagnostics_service_client::DiagnosticsServiceClient,
    feature_service_client::FeatureServiceClient, pairing_service_client::PairingServiceClient,
    topology_service_client::TopologyServiceClient,
};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTrustBundle {
    machine_id: String,
    display_name: String,
    network_address: String,
    ca_cert_pem: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Status => daemon_status(&cli.endpoint).await,
        },
        Command::Pair { command } => match command {
            PairCommand::CreateCode { ttl } => pair_create_code(&cli.endpoint, ttl).await,
            PairCommand::Join { code, host, alias } => {
                pair_join(&cli.endpoint, code, host, alias).await
            }
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

    bail!("invalid named-pipe endpoint {endpoint}; expected npipe://./pipe/<name>")
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

async fn daemon_status(endpoint: &str) -> Result<()> {
    let mut client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = client.get_status(StatusRequest {}).await?.into_inner();
    println!(
        "running={} machine_id={} peers={} protocol={} api_transport={} api_bind={} api_pipe_name={}",
        status.running,
        status.machine_id,
        status.peer_count,
        status.protocol_version,
        status.api_transport,
        status.api_bind,
        status.api_pipe_name
    );
    Ok(())
}

async fn pair_create_code(endpoint: &str, ttl: u32) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .create_code(PairCreateCodeRequest { ttl_seconds: ttl })
        .await?
        .into_inner();

    println!("code={} expires_at={}", response.code, response.expires_at);
    Ok(())
}

async fn pair_join(
    endpoint: &str,
    code: String,
    host: String,
    alias: Option<String>,
) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .join(PairJoinRequest {
            code,
            host,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();
    println!(
        "accepted={} peer_id={} message={}",
        response.accepted, response.peer_id, response.message
    );
    Ok(())
}

async fn pair_export_trust(endpoint: &str, output: Option<String>) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client.export_trust_bundle(Empty {}).await?.into_inner();

    let bundle = StoredTrustBundle {
        machine_id: response.machine_id,
        display_name: response.display_name,
        network_address: response.network_address,
        ca_cert_pem: response.ca_cert_pem,
    };

    let json = serde_json::to_string_pretty(&bundle).context("serialize trust bundle")?;

    if let Some(path) = output {
        std::fs::write(&path, &json).with_context(|| format!("write {path}"))?;
        println!("wrote trust bundle to {path}");
    } else {
        println!("{json}");
    }

    Ok(())
}

async fn pair_import_trust(endpoint: &str, input: String, alias: Option<String>) -> Result<()> {
    let raw = std::fs::read_to_string(&input).with_context(|| format!("read {input}"))?;
    let bundle: StoredTrustBundle = serde_json::from_str(&raw).context("parse trust bundle")?;

    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .import_trust_bundle(ImportTrustBundleRequest {
            machine_id: bundle.machine_id,
            display_name: bundle.display_name,
            network_address: bundle.network_address,
            ca_cert_pem: bundle.ca_cert_pem,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn peer_list(endpoint: &str) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client.list_peers(Empty {}).await?.into_inner();

    if response.peers.is_empty() {
        println!("no peers configured");
        return Ok(());
    }

    for peer in response.peers {
        println!(
            "peer_id={} name={} address={} connected={}",
            peer.peer_id, peer.display_name, peer.address, peer.connected
        );
    }

    Ok(())
}

async fn peer_remove(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client
        .remove_peer(RemovePeerRequest { peer_id })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn layout_show(endpoint: &str) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client.layout_show(Empty {}).await?.into_inner();
    println!("{}", response.matrix_spec);
    Ok(())
}

async fn layout_set(endpoint: &str, matrix: String) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client
        .layout_set(LayoutSetRequest {
            matrix_spec: matrix,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn feature_list(endpoint: &str) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel(endpoint).await?);
    let response = client.list_features(Empty {}).await?.into_inner();

    let mut features = response.features.into_iter().collect::<Vec<_>>();
    features.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, enabled) in features {
        println!("{name}={enabled}");
    }

    Ok(())
}

async fn feature_set(endpoint: &str, name: String, value: ToggleValue) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel(endpoint).await?);
    let response = client
        .set_feature(FeatureSetRequest {
            name,
            enabled: value.as_bool(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn hotkey_set(endpoint: &str, action: String, combo: String) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel(endpoint).await?);
    let response = client
        .set_hotkey(HotkeySetRequest { action, combo })
        .await?
        .into_inner();
    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn transport_send_text(endpoint: &str, peer_id: String, text: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_clipboard_text(SendClipboardTextRequest { peer_id, text })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn transport_send_image(endpoint: &str, peer_id: String, path: String) -> Result<()> {
    let image_bmp = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    validate_bmp_payload(&image_bmp).with_context(|| format!("invalid BMP payload at {path}"))?;

    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_clipboard_image(SendClipboardImageRequest { peer_id, image_bmp })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn transport_send_file(endpoint: &str, peer_id: String, path: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_file(SendFileRequest {
            peer_id,
            file_path: path,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn transport_events(endpoint: &str, limit: usize) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let mut events = client
        .list_transport_events(Empty {})
        .await?
        .into_inner()
        .events;

    if limit > 0 && events.len() > limit {
        events = events.split_off(events.len() - limit);
    }

    if events.is_empty() {
        println!("no transport events");
        return Ok(());
    }

    for event in events {
        println!(
            "{} direction={} kind={} peer_id={} size_bytes={} detail={}",
            event.timestamp,
            event.direction,
            event.kind,
            event.peer_id,
            event.size_bytes,
            event.detail
        );
    }

    Ok(())
}

async fn input_owner(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client.get_input_owner(Empty {}).await?.into_inner();
    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

async fn input_capture_target(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .get_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

async fn input_capture_start(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .set_input_capture_target(InputCaptureTargetRequest { peer_id })
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

async fn input_capture_stop(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .clear_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

async fn input_send_move(endpoint: &str, peer_id: String, dx: i32, dy: i32) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_input_move(SendInputMoveRequest { peer_id, dx, dy })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn input_send_key(
    endpoint: &str,
    peer_id: String,
    scan_code: u16,
    state: InputKeyState,
) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_input_key(SendInputKeyRequest {
            peer_id,
            scan_code: scan_code as u32,
            key_down: state.is_down(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn input_claim(endpoint: &str, peer_id: String, force: bool) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .claim_input_owner(InputOwnerRequest { peer_id, force })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

async fn input_release(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .release_input_owner(InputOwnerRequest {
            peer_id,
            force: false,
        })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

async fn diagnostics_dump(endpoint: &str, output: Option<String>) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .dump(DiagnosticsDumpRequest {
            output_path: output.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("bundle_path={}", response.bundle_path);
    Ok(())
}

async fn safe_reset(endpoint: &str, network_only: bool, all: bool) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .safe_reset(SafeResetRequest { network_only, all })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

#[cfg(windows)]
#[derive(Clone)]
struct NamedPipeConnector {
    pipe_path: String,
}

#[cfg(windows)]
impl NamedPipeConnector {
    fn new(pipe_path: String) -> Self {
        Self { pipe_path }
    }
}

#[cfg(windows)]
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

#[cfg(windows)]
const ERROR_PIPE_BUSY_CODE: i32 = 231;
#[cfg(windows)]
const PIPE_BUSY_MAX_RETRIES: u32 = 20;
#[cfg(windows)]
const PIPE_BUSY_BACKOFF_MS: u64 = 25;

#[cfg(windows)]
async fn open_named_pipe_with_retry(pipe_path: String) -> io::Result<NamedPipeClient> {
    let mut attempt = 0_u32;

    loop {
        match ClientOptions::new().open(pipe_path.as_str()) {
            Ok(client) => return Ok(client),
            Err(error) if is_pipe_busy_error(&error) && attempt < PIPE_BUSY_MAX_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(PIPE_BUSY_BACKOFF_MS)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn is_pipe_busy_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE)
}

fn validate_bmp_payload(bytes: &[u8]) -> Result<()> {
    validate_bmp_bytes(bytes).map_err(anyhow::Error::from)
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

    #[cfg(windows)]
    #[test]
    fn detects_pipe_busy_error_code() {
        let busy = std::io::Error::from_raw_os_error(231);
        let other = std::io::Error::from_raw_os_error(5);
        assert!(is_pipe_busy_error(&busy));
        assert!(!is_pipe_busy_error(&other));
    }
}
