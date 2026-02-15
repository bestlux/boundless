mod config;
mod logging;
mod services;
mod state;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tonic::transport::Server;
use tracing::{info, warn};

use crate::{services::ServiceBundle, state::AppState};

#[derive(Debug, Parser)]
#[command(name = "boundlessd", version, about = "Boundless daemon")]
struct Args {
    #[arg(long)]
    bind: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    PrintConfigPath,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _logging = logging::init_logging()?;
    let args = Args::parse();

    if let Some(Command::PrintConfigPath) = args.command {
        println!("{}", config::config_path().display());
        return Ok(());
    }

    let state = AppState::load_or_create().context("load app state")?;

    if let Some(bind) = args.bind {
        state
            .update_bind(bind)
            .await
            .context("update bind address")?;
    }

    let snapshot = state.snapshot().await;
    let addr = snapshot
        .api_bind
        .parse()
        .with_context(|| format!("invalid bind address {}", snapshot.api_bind))?;

    let ServiceBundle {
        daemon,
        pairing,
        topology,
        feature,
        diagnostics,
    } = ServiceBundle::new(state.clone());

    info!(
        machine_id = %snapshot.machine_id,
        api_bind = %snapshot.api_bind,
        protocol = %snapshot.protocol_version,
        "boundless daemon starting"
    );

    Server::builder()
        .add_service(daemon)
        .add_service(pairing)
        .add_service(topology)
        .add_service(feature)
        .add_service(diagnostics)
        .serve_with_shutdown(addr, async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                warn!(%error, "ctrl_c signal error");
            }
            info!("shutdown requested");
        })
        .await
        .context("gRPC server failure")?;

    Ok(())
}
