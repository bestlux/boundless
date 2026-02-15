mod clipboard;
mod config;
mod discovery;
mod logging;
mod network;
mod services;
mod state;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::ApiTransport;
use tonic::transport::Server;
use tracing::{info, warn};

use crate::{services::ServiceBundle, state::AppState};

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
        println!("{}", config::config_path().display());
        return Ok(());
    }

    let state = AppState::load_or_create().context("load app state")?;

    if let Some(bind) = args.bind {
        bind.parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid --bind address {bind}"))?;
        state
            .update_bind(bind)
            .await
            .context("update bind address")?;
    }
    if let Some(transport) = args.api_transport {
        state
            .update_api_transport(transport.into())
            .await
            .context("update API transport")?;
    }
    if let Some(pipe_name) = args.api_pipe_name {
        state
            .update_api_pipe_name(pipe_name)
            .await
            .context("update API pipe name")?;
    }
    if let Some(port) = args.network_port {
        state
            .update_network_port(port)
            .await
            .context("update network port")?;
    }

    let snapshot = state.snapshot().await;

    let ServiceBundle {
        daemon,
        pairing,
        topology,
        feature,
        diagnostics,
    } = ServiceBundle::new(state.clone());

    clipboard::start(state.clone());
    discovery::start(state.clone());
    network::start(state.clone());

    let configured_api_transport = snapshot.api_transport;
    let effective_api_transport = configured_api_transport.effective();
    if configured_api_transport != effective_api_transport {
        warn!(
            configured = configured_api_transport.as_str(),
            effective = effective_api_transport.as_str(),
            "configured API transport is not supported on this platform; using fallback"
        );
    }

    info!(
        machine_id = %snapshot.machine_id,
        api_bind = %snapshot.api_bind,
        api_transport = effective_api_transport.as_str(),
        api_pipe_name = %snapshot.api_pipe_name,
        network_port = snapshot.network_port,
        protocol = %snapshot.protocol_version,
        "boundless daemon starting"
    );

    if matches!(effective_api_transport, ApiTransport::NamedPipe) {
        #[cfg(windows)]
        {
            let incoming = named_pipe::incoming(&snapshot.api_pipe_name)
                .with_context(|| format!("initialize named pipe {}", snapshot.api_pipe_name))?;

            Server::builder()
                .add_service(daemon)
                .add_service(pairing)
                .add_service(topology)
                .add_service(feature)
                .add_service(diagnostics)
                .serve_with_incoming_shutdown(incoming, shutdown_signal())
                .await
                .context("gRPC named-pipe server failure")?;

            return Ok(());
        }
    }

    let addr = snapshot
        .api_bind
        .parse()
        .with_context(|| format!("invalid bind address {}", snapshot.api_bind))?;

    Server::builder()
        .add_service(daemon)
        .add_service(pairing)
        .add_service(topology)
        .add_service(feature)
        .add_service(diagnostics)
        .serve_with_shutdown(addr, shutdown_signal())
        .await
        .context("gRPC server failure")?;

    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(%error, "ctrl_c signal error");
    }
    info!("shutdown requested");
}

#[cfg(windows)]
mod named_pipe {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::{
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::windows::named_pipe::{NamedPipeServer, ServerOptions},
        sync::mpsc,
    };
    use tonic::{codegen::tokio_stream::Stream, transport::server::Connected};

    #[derive(Debug)]
    pub struct NamedPipeIncoming {
        receiver: mpsc::Receiver<io::Result<NamedPipeIo>>,
    }

    impl Stream for NamedPipeIncoming {
        type Item = io::Result<NamedPipeIo>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.receiver).poll_recv(cx)
        }
    }

    #[derive(Debug)]
    pub struct NamedPipeIo {
        inner: NamedPipeServer,
    }

    impl Connected for NamedPipeIo {
        type ConnectInfo = ();

        fn connect_info(&self) -> Self::ConnectInfo {}
    }

    impl AsyncRead for NamedPipeIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for NamedPipeIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    pub fn incoming(pipe_name: &str) -> io::Result<NamedPipeIncoming> {
        let pipe_path = pipe_path_for_name(pipe_name)?;
        let (sender, receiver) = mpsc::channel(32);
        let first_server = create_server(&pipe_path, true)?;

        tokio::spawn(async move {
            accept_loop(pipe_path, first_server, sender).await;
        });

        Ok(NamedPipeIncoming { receiver })
    }

    async fn accept_loop(
        pipe_path: String,
        mut server: NamedPipeServer,
        sender: mpsc::Sender<io::Result<NamedPipeIo>>,
    ) {
        loop {
            if let Err(error) = server.connect().await {
                let _ = sender.send(Err(error)).await;
                break;
            }

            let next_server = match create_server(&pipe_path, false) {
                Ok(next) => next,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    break;
                }
            };

            let io = NamedPipeIo { inner: server };
            if sender.send(Ok(io)).await.is_err() {
                break;
            }

            server = next_server;
        }
    }

    fn create_server(pipe_path: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        if first_instance {
            options.first_pipe_instance(true);
        }
        options.create(pipe_path)
    }

    fn pipe_path_for_name(pipe_name: &str) -> io::Result<String> {
        let trimmed = pipe_name.trim();
        if trimmed.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe name must not be empty",
            ));
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe name must not contain path separators",
            ));
        }

        Ok(format!(r"\\.\pipe\{trimmed}"))
    }
}
