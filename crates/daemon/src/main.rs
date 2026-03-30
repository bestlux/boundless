use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tonic::transport::Server;

use boundless_daemon::{
    config::ApiTransport,
    host::{HostOverrides, run_with, shutdown_signal},
    logging,
    shared_control_plane_app,
};
#[cfg(windows)]
use platform_windows::runtime::named_pipe_incoming;

#[derive(Debug, Parser)]
#[command(name = "boundlessd", version, about = "Boundless daemon")]
struct Args {
    #[arg(long)]
    bind: Option<String>,

    #[arg(long, value_enum)]
    api_transport: Option<ApiTransportArg>,

    #[arg(long)]
    api_pipe_name: Option<String>,

    #[arg(long)]
    network_port: Option<u16>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    PrintConfigPath,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ApiTransportArg {
    Tcp,
    NamedPipe,
}

impl From<ApiTransportArg> for ApiTransport {
    fn from(value: ApiTransportArg) -> Self {
        match value {
            ApiTransportArg::Tcp => ApiTransport::Tcp,
            ApiTransportArg::NamedPipe => ApiTransport::NamedPipe,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _logging = logging::init_logging()?;
    let args = Args::parse();

    if let Some(Command::PrintConfigPath) = args.command {
        println!("{}", boundless_daemon::config::config_path().display());
        return Ok(());
    }

    run_with(
        HostOverrides {
            bind: args.bind,
            api_transport: args.api_transport.map(Into::into),
            api_pipe_name: args.api_pipe_name,
            network_port: args.network_port,
        },
        |runtime| async move {
            let control_plane = adapter_ipc_grpc::ControlPlaneApi::new(shared_control_plane_app(
                runtime.state.clone(),
            ))
            .into_server();

            if matches!(runtime.effective_api_transport, ApiTransport::NamedPipe) {
                #[cfg(windows)]
                {
                    let incoming = named_pipe_incoming(&runtime.snapshot.api_pipe_name)
                        .with_context(|| {
                            format!("initialize named pipe {}", runtime.snapshot.api_pipe_name)
                        })?;

                    Server::builder()
                        .add_service(control_plane)
                        .serve_with_incoming_shutdown(incoming, shutdown_signal())
                        .await
                        .context("gRPC named-pipe server failure")?;

                    return Ok(());
                }
            }

            let addr =
                runtime.snapshot.api_bind.parse().with_context(|| {
                    format!("invalid bind address {}", runtime.snapshot.api_bind)
                })?;

            Server::builder()
                .add_service(control_plane)
                .serve_with_shutdown(addr, shutdown_signal())
                .await
                .context("gRPC server failure")?;

            Ok(())
        },
    )
    .await
}
