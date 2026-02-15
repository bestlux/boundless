use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tonic::transport::Channel;

use ipc_api::boundless::v1::{
    DiagnosticsDumpRequest, Empty, FeatureSetRequest, HotkeySetRequest, LayoutSetRequest,
    PairCreateCodeRequest, PairJoinRequest, RemovePeerRequest, SafeResetRequest, StatusRequest,
    daemon_service_client::DaemonServiceClient,
    diagnostics_service_client::DiagnosticsServiceClient,
    feature_service_client::FeatureServiceClient, pairing_service_client::PairingServiceClient,
    topology_service_client::TopologyServiceClient,
};

#[derive(Debug, Parser)]
#[command(name = "boundlessctl", version, about = "Boundless CLI")]
struct Cli {
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Status => daemon_status().await,
        },
        Command::Pair { command } => match command {
            PairCommand::CreateCode { ttl } => pair_create_code(ttl).await,
            PairCommand::Join { code, host, alias } => pair_join(code, host, alias).await,
        },
        Command::Peer { command } => match command {
            PeerCommand::List => peer_list().await,
            PeerCommand::Remove { peer_id } => peer_remove(peer_id).await,
        },
        Command::Layout { command } => match command {
            LayoutCommand::Show => layout_show().await,
            LayoutCommand::Set { matrix } => layout_set(matrix).await,
        },
        Command::Feature { command } => match command {
            FeatureCommand::List => feature_list().await,
            FeatureCommand::Set { name, value } => feature_set(name, value).await,
        },
        Command::Hotkey { action, combo } => hotkey_set(action, combo).await,
        Command::Diagnostics { command } => match command {
            DiagnosticsCommand::Dump { output } => diagnostics_dump(output).await,
        },
        Command::SafeReset { network, all } => safe_reset(network, all).await,
    }
}

fn endpoint() -> String {
    std::env::var("BOUNDLESS_API_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:50051".to_string())
}

async fn channel() -> Result<Channel> {
    let ep = endpoint();
    Channel::from_shared(ep.clone())
        .with_context(|| format!("invalid endpoint {ep}"))?
        .connect()
        .await
        .with_context(|| format!("failed to connect to {ep}"))
}

async fn daemon_status() -> Result<()> {
    let mut client = DaemonServiceClient::new(channel().await?);
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

async fn pair_create_code(ttl: u32) -> Result<()> {
    let mut client = PairingServiceClient::new(channel().await?);
    let response = client
        .create_code(PairCreateCodeRequest { ttl_seconds: ttl })
        .await?
        .into_inner();

    println!("code={} expires_at={}", response.code, response.expires_at);
    Ok(())
}

async fn pair_join(code: String, host: String, alias: Option<String>) -> Result<()> {
    let mut client = PairingServiceClient::new(channel().await?);
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

async fn peer_list() -> Result<()> {
    let mut client = TopologyServiceClient::new(channel().await?);
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

async fn peer_remove(peer_id: String) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel().await?);
    let response = client
        .remove_peer(RemovePeerRequest { peer_id })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn layout_show() -> Result<()> {
    let mut client = TopologyServiceClient::new(channel().await?);
    let response = client.layout_show(Empty {}).await?.into_inner();
    println!("{}", response.matrix_spec);
    Ok(())
}

async fn layout_set(matrix: String) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel().await?);
    let response = client
        .layout_set(LayoutSetRequest {
            matrix_spec: matrix,
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn feature_list() -> Result<()> {
    let mut client = FeatureServiceClient::new(channel().await?);
    let response = client.list_features(Empty {}).await?.into_inner();

    let mut features = response.features.into_iter().collect::<Vec<_>>();
    features.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, enabled) in features {
        println!("{name}={enabled}");
    }

    Ok(())
}

async fn feature_set(name: String, value: ToggleValue) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel().await?);
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

async fn hotkey_set(action: String, combo: String) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel().await?);
    let response = client
        .set_hotkey(HotkeySetRequest { action, combo })
        .await?
        .into_inner();
    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

async fn diagnostics_dump(output: Option<String>) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel().await?);
    let response = client
        .dump(DiagnosticsDumpRequest {
            output_path: output.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("bundle_path={}", response.bundle_path);
    Ok(())
}

async fn safe_reset(network_only: bool, all: bool) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel().await?);
    let response = client
        .safe_reset(SafeResetRequest { network_only, all })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}
