use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use core_clipboard::validate_bmp_payload as validate_bmp_bytes;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    time::Duration,
};
use tokio::time::Instant;
use tonic::transport::{Channel, Endpoint};

#[cfg(windows)]
use hyper_util::rt::TokioIo;
#[cfg(windows)]
use std::{
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
#[cfg(windows)]
use tonic::{codegen::Service, transport::Uri};

use ipc_api::boundless::v1::{
    AntiIdleSetRequest, DiagnosticsDumpRequest, Empty, FeatureSetRequest, HotkeySetRequest,
    HotkeyTriggerRequest, ImportTrustBundleRequest, InputCaptureTargetRequest, InputOwnerRequest,
    LayoutSetRequest, NearbyJoinStartRequest, NearbyJoinStatusRequest,
    NearbyPairingDecisionRequest, NearbyRequestCodeStartRequest, NearbySubmitCodeRequest,
    PairCreateCodeRequest, PairJoinRequest, RemovePeerRequest, SafeResetRequest,
    SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest, SendInputKeyRequest,
    SendInputMoveRequest, StatusReply, StatusRequest, UiSnapshotReply,
    control_plane_service_client::ControlPlaneServiceClient,
};

mod cli_helpers;
mod commands;
mod console;

#[cfg(test)]
use app_services::desktop::nearby_pairing_port;
#[cfg(windows)]
use cli_helpers::NamedPipeConnector;
#[cfg(all(test, windows))]
use cli_helpers::is_pipe_busy_error;
use cli_helpers::{
    filter_connectable_discovery_records, format_host_port, parse_npipe_endpoint,
    prompt_pairing_code, prompt_pairing_nonce, resolve_discovered_peer, short_machine_id,
    validate_bmp_payload,
};
use commands::*;
use console::console_run;
use console::{ConsoleDiscoveredPeer, ConsoleSnapshot};

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
    Setup {
        #[arg(long, default_value_t = true)]
        start_daemon: bool,
    },
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
    AntiIdle {
        #[command(subcommand)]
        command: AntiIdleCommand,
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
    Ui {
        #[command(subcommand)]
        command: UiCommand,
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
    Discover,
    CreateCode {
        #[arg(long, default_value_t = 300)]
        ttl: u32,
    },
    Request {
        selector: String,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        nonce: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        code: Option<String>,
        #[arg(long)]
        alias: Option<String>,
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
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
    Preview,
    Set {
        matrix: String,
    },
    Orient {
        #[arg(long)]
        left: Option<String>,
        #[arg(long)]
        right: Option<String>,
        #[arg(long)]
        up: Option<String>,
        #[arg(long)]
        down: Option<String>,
    },
    Wizard,
}

#[derive(Debug, Subcommand)]
enum FeatureCommand {
    List,
    Set { name: String, value: ToggleValue },
}

#[derive(Debug, Subcommand)]
enum AntiIdleCommand {
    Show,
    Set {
        #[arg(long)]
        enabled: bool,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=30))]
        window_minutes: u32,
        #[arg(long)]
        allow_on_battery: bool,
        #[arg(long)]
        keep_display_on: bool,
    },
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

#[derive(Debug, Subcommand)]
enum UiCommand {
    Snapshot {
        #[arg(long, default_value_t = false)]
        start_daemon: bool,
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
        Command::Setup { start_daemon } => setup_wizard(&cli.endpoint, start_daemon).await,
        Command::Console { start_daemon } => console_run(&cli.endpoint, start_daemon).await,
        Command::Daemon { command } => match command {
            DaemonCommand::Status => daemon_status(&cli.endpoint).await,
        },
        Command::Pair { command } => match command {
            PairCommand::Discover => pair_discover(&cli.endpoint).await,
            PairCommand::CreateCode { ttl } => pair_create_code(&cli.endpoint, ttl).await,
            PairCommand::Request {
                selector,
                request_id,
                nonce,
                host,
                port,
                code,
                alias,
                timeout_seconds,
            } => {
                pair_request(
                    &cli.endpoint,
                    PairRequestArgs {
                        selector,
                        request_id,
                        verification_nonce: nonce,
                        host_override: host,
                        port_override: port,
                        code,
                        alias,
                        timeout_seconds,
                    },
                )
                .await
            }
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
            LayoutCommand::Preview => layout_preview(&cli.endpoint).await,
            LayoutCommand::Set { matrix } => layout_set(&cli.endpoint, matrix).await,
            LayoutCommand::Orient {
                left,
                right,
                up,
                down,
            } => layout_orient(&cli.endpoint, left, right, up, down).await,
            LayoutCommand::Wizard => layout_wizard(&cli.endpoint).await,
        },
        Command::Feature { command } => match command {
            FeatureCommand::List => feature_list(&cli.endpoint).await,
            FeatureCommand::Set { name, value } => feature_set(&cli.endpoint, name, value).await,
        },
        Command::AntiIdle { command } => match command {
            AntiIdleCommand::Show => anti_idle_show(&cli.endpoint).await,
            AntiIdleCommand::Set {
                enabled,
                window_minutes,
                allow_on_battery,
                keep_display_on,
            } => {
                anti_idle_set(
                    &cli.endpoint,
                    enabled,
                    window_minutes,
                    allow_on_battery,
                    keep_display_on,
                )
                .await
            }
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
        Command::Ui { command } => match command {
            UiCommand::Snapshot { start_daemon } => ui_snapshot(&cli.endpoint, start_daemon).await,
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
            anti_idle_reason: "none".to_string(),
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
