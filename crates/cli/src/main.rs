use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use control_plane_client::{
    channel, connect_control_plane, default_endpoint, has_access_denied_io_error,
    is_named_pipe_endpoint,
};
use core_clipboard::validate_bmp_payload as validate_bmp_bytes;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    time::Duration,
};
use tokio::time::Instant;

use ipc_api::boundless::v1::{
    AntiIdleSetRequest, DiagnosticsDumpRequest, Empty, FeatureSetRequest, FileTransferSetRequest,
    HotkeySetRequest, HotkeyTriggerRequest, ImportTrustBundleRequest, InputCaptureTargetRequest,
    InputHandoffConfigReply, InputHandoffSetRequest, InputOwnerRequest, InputRuntimeStatusReply,
    LayoutSetRequest, NearbyJoinStartRequest, NearbyJoinStatusRequest,
    NearbyPairingDecisionRequest, NearbyRequestCodeStartRequest, NearbySubmitCodeRequest,
    PairCreateCodeRequest, PairJoinRequest, PeerInfo, RemovePeerRequest, RotateTrustRequest,
    SafeResetRequest, SendClipboardImageRequest, SendClipboardTextRequest, SendFileRequest,
    SendInputKeyRequest, SendInputMoveRequest, StatusReply, StatusRequest, TransportEvent,
    UiSnapshotReply,
};

mod cli_helpers;
mod commands;
mod console;
mod paired_testing;

#[cfg(test)]
use app_services::desktop::nearby_pairing_port;
use cli_helpers::{
    filter_connectable_discovery_records, format_host_port, prompt_pairing_code,
    prompt_pairing_nonce, resolve_discovered_peer, short_machine_id, validate_bmp_payload,
};
use commands::*;
use console::console_run;
use console::{ConsoleDiscoveredPeer, ConsoleSnapshot};

#[derive(Debug, Parser)]
#[command(name = "boundlessctl", version, about = "Boundless CLI")]
struct Cli {
    #[arg(long, global = true, default_value_t = false)]
    json: bool,

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
    /// Explicitly permitted, bounded diagnostics across an existing paired connection.
    PairedTest {
        #[command(subcommand)]
        command: paired_testing::PairedTestCommand,
    },
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
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
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
    FileTransfer {
        #[command(subcommand)]
        command: FileTransferCommand,
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
    Doctor {
        #[arg(long, default_value_t = false)]
        install: bool,
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
        #[arg(long)]
        confirm: String,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Status,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    Status,
    Install {
        #[arg(long)]
        binary: Option<String>,
        #[arg(long, default_value_t = false)]
        auto_start: bool,
    },
    Start,
    Stop,
    Uninstall,
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
        #[arg(long, default_value_t = false)]
        role_reversal: bool,
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
    RotateTrust {
        #[arg(long)]
        confirm: String,
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
enum FileTransferCommand {
    Config,
    SetReceiveDir {
        path: String,
        #[arg(long)]
        organize_by_peer: bool,
        #[arg(long)]
        no_organize_by_peer: bool,
        #[arg(long)]
        auto_accept_trusted_peers: Option<bool>,
        #[arg(long)]
        max_file_bytes: Option<u64>,
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
        #[arg(required = true)]
        paths: Vec<String>,
    },
    Events {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        exclude_kind: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum InputCommand {
    Status,
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
    Config,
    SetConfig {
        #[arg(long)]
        block_screen_corners: Option<bool>,
        #[arg(long)]
        corner_block_px: Option<u32>,
        #[arg(long)]
        relative_mouse: Option<bool>,
        #[arg(long)]
        hide_cursor_at_edge: Option<bool>,
        #[arg(long)]
        draw_cursor_marker: Option<bool>,
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
        #[arg(long, default_value_t = false)]
        include_filenames: bool,
        #[arg(long, default_value_t = false)]
        offline: bool,
        #[arg(long, default_value_t = false)]
        open_folder: bool,
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
    if cli.json && !command_supports_json(&cli.command) {
        anyhow::bail!(
            "--json is supported by daemon status, peer list, feature list, transport events, doctor --install, and paired-test"
        );
    }
    let output = OutputFormat::from_json_flag(cli.json);

    match cli.command {
        Command::Setup { start_daemon } => setup_wizard(&cli.endpoint, start_daemon).await,
        Command::Console { start_daemon } => console_run(&cli.endpoint, start_daemon).await,
        Command::Daemon { command } => match command {
            DaemonCommand::Status => daemon_status(&cli.endpoint, output).await,
        },
        Command::Service { command } => match command {
            ServiceCommand::Status => service_status().await,
            ServiceCommand::Install { binary, auto_start } => {
                service_install(binary, auto_start).await
            }
            ServiceCommand::Start => service_start().await,
            ServiceCommand::Stop => service_stop().await,
            ServiceCommand::Uninstall => service_uninstall().await,
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
                role_reversal,
            } => {
                pair_nearby_join(
                    &cli.endpoint,
                    NearbyJoinCliRequest {
                        code,
                        host,
                        port,
                        timeout_seconds,
                        alias,
                        endpoint_candidates: Vec::new(),
                        role_reversal,
                    },
                )
                .await
            }
            PairCommand::Pending => pair_pending(&cli.endpoint).await,
            PairCommand::Approve { request_id, alias } => {
                pair_approve(&cli.endpoint, request_id, alias).await
            }
            PairCommand::Reject { request_id } => pair_reject(&cli.endpoint, request_id).await,
            PairCommand::ExportTrust { output } => pair_export_trust(&cli.endpoint, output).await,
            PairCommand::ImportTrust { input, alias } => {
                pair_import_trust(&cli.endpoint, input, alias).await
            }
            PairCommand::RotateTrust { confirm } => pair_rotate_trust(&cli.endpoint, confirm).await,
        },
        Command::Peer { command } => match command {
            PeerCommand::List => peer_list(&cli.endpoint, output).await,
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
            FeatureCommand::List => feature_list(&cli.endpoint, output).await,
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
        Command::FileTransfer { command } => match command {
            FileTransferCommand::Config => file_transfer_config(&cli.endpoint).await,
            FileTransferCommand::SetReceiveDir {
                path,
                organize_by_peer,
                no_organize_by_peer,
                auto_accept_trusted_peers,
                max_file_bytes,
            } => {
                file_transfer_set_receive_dir(
                    &cli.endpoint,
                    path,
                    organize_by_peer,
                    no_organize_by_peer,
                    auto_accept_trusted_peers,
                    max_file_bytes,
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
            TransportCommand::SendFile { peer_id, paths } => {
                transport_send_files(&cli.endpoint, peer_id, paths).await
            }
            TransportCommand::Events {
                limit,
                kind,
                exclude_kind,
            } => {
                transport_events(
                    &cli.endpoint,
                    limit,
                    kind.as_deref(),
                    exclude_kind.as_deref(),
                    output,
                )
                .await
            }
        },
        Command::Input { command } => match command {
            InputCommand::Status => input_status(&cli.endpoint).await,
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
            InputCommand::Config => input_config(&cli.endpoint).await,
            InputCommand::SetConfig {
                block_screen_corners,
                corner_block_px,
                relative_mouse,
                hide_cursor_at_edge,
                draw_cursor_marker,
            } => {
                input_set_config(
                    &cli.endpoint,
                    block_screen_corners,
                    corner_block_px,
                    relative_mouse,
                    hide_cursor_at_edge,
                    draw_cursor_marker,
                )
                .await
            }
        },
        Command::PairedTest { command } => {
            paired_testing::execute(&cli.endpoint, command, output).await
        }
        Command::Hotkey { action, combo } => hotkey_set(&cli.endpoint, action, combo).await,
        Command::Diagnostics { command } => match command {
            DiagnosticsCommand::Dump {
                output,
                include_filenames,
                offline,
                open_folder,
            } => {
                diagnostics_dump(
                    &cli.endpoint,
                    output,
                    include_filenames,
                    offline,
                    open_folder,
                )
                .await
            }
            DiagnosticsCommand::RunAction { action } => {
                diagnostics_run_action(&cli.endpoint, action).await
            }
        },
        Command::Doctor { install } => {
            if !install {
                anyhow::bail!("doctor requires --install");
            }
            doctor_install(&cli.endpoint, output).await
        }
        Command::Ui { command } => match command {
            UiCommand::Snapshot { start_daemon } => ui_snapshot(&cli.endpoint, start_daemon).await,
        },
        Command::SafeReset {
            network,
            all,
            confirm,
        } => safe_reset(&cli.endpoint, network, all, confirm).await,
    }
}

fn command_supports_json(command: &Command) -> bool {
    matches!(
        command,
        Command::Daemon {
            command: DaemonCommand::Status
        } | Command::Peer {
            command: PeerCommand::List
        } | Command::Feature {
            command: FeatureCommand::List
        } | Command::Transport {
            command: TransportCommand::Events { .. }
        } | Command::Doctor { install: true }
            | Command::PairedTest { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_test_commands_accept_json_and_reject_unbounded_work() {
        let cli = Cli::try_parse_from(["boundlessctl", "paired-test", "run", "peer-id", "--json"])
            .unwrap();
        assert!(command_supports_json(&cli.command));
        assert!(
            Cli::try_parse_from([
                "boundlessctl",
                "paired-test",
                "run",
                "peer-id",
                "--samples",
                "101"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "boundlessctl",
                "paired-test",
                "run",
                "peer-id",
                "--payload-bytes",
                "65537"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "boundlessctl",
                "paired-test",
                "allow",
                "peer-id",
                "--seconds",
                "601"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["boundlessctl", "paired-test", "revoke", "--json"]).is_ok());
    }

    #[test]
    fn json_flag_is_global_and_defaults_to_human_output() {
        let before = Cli::try_parse_from(["boundlessctl", "--json", "daemon", "status"])
            .expect("parse global flag before command");
        assert!(before.json);

        let after = Cli::try_parse_from(["boundlessctl", "daemon", "status", "--json"])
            .expect("parse global flag after nested command");
        assert!(after.json);

        let default = Cli::try_parse_from(["boundlessctl", "daemon", "status"])
            .expect("parse without global flag");
        assert!(!default.json);

        let unsupported = Cli::try_parse_from(["boundlessctl", "--json", "service", "status"])
            .expect("global syntax remains parseable");
        assert!(!command_supports_json(&unsupported.command));

        let doctor = Cli::try_parse_from(["boundlessctl", "--json", "doctor", "--install"])
            .expect("parse install doctor JSON");
        assert!(command_supports_json(&doctor.command));
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
                    endpoint_candidates: vec!["10.0.0.10:15100".to_string()],
                },
                ConsoleDiscoveredPeer {
                    machine_id: "11111111-2222-3333-4444-555555555555".to_string(),
                    display_name: "MACHINE-B".to_string(),
                    endpoint: "10.0.0.11:15100".to_string(),
                    endpoint_candidates: vec!["10.0.0.11:15100".to_string()],
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
}
