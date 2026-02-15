use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use tonic::transport::Channel;

use ipc_api::boundless::v1::{
    DiagnosticsDumpRequest, Empty, FeatureSetRequest, HotkeySetRequest, ImportTrustBundleRequest,
    LayoutSetRequest, PairCreateCodeRequest, PairJoinRequest, RemovePeerRequest, SafeResetRequest,
    SendClipboardTextRequest, SendFileRequest, StatusRequest,
    daemon_service_client::DaemonServiceClient,
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
        default_value = "http://127.0.0.1:50051"
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
    SendFile {
        peer_id: String,
        path: String,
    },
    Events {
        #[arg(long, default_value_t = 50)]
        limit: usize,
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
            TransportCommand::SendFile { peer_id, path } => {
                transport_send_file(&cli.endpoint, peer_id, path).await
            }
            TransportCommand::Events { limit } => transport_events(&cli.endpoint, limit).await,
        },
        Command::Hotkey { action, combo } => hotkey_set(&cli.endpoint, action, combo).await,
        Command::Diagnostics { command } => match command {
            DiagnosticsCommand::Dump { output } => diagnostics_dump(&cli.endpoint, output).await,
        },
        Command::SafeReset { network, all } => safe_reset(&cli.endpoint, network, all).await,
    }
}

async fn channel(endpoint: &str) -> Result<Channel> {
    Channel::from_shared(endpoint.to_string())
        .with_context(|| format!("invalid endpoint {endpoint}"))?
        .connect()
        .await
        .with_context(|| format!("failed to connect to {endpoint}"))
}

async fn daemon_status(endpoint: &str) -> Result<()> {
    let mut client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = client.get_status(StatusRequest {}).await?.into_inner();
    println!(
        "running={} machine_id={} peers={} protocol={} api={}",
        status.running,
        status.machine_id,
        status.peer_count,
        status.protocol_version,
        status.api_bind
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
