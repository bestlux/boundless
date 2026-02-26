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
    use tinyfiledialogs::{MessageBoxIcon, YesNo, input_box, message_box_ok, message_box_yes_no};
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{TcpStream, windows::named_pipe::ClientOptions},
    };
    use tonic::{
        codegen::Service,
        transport::{Channel, Endpoint, Uri},
    };
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    };
    use winit::{
        application::ApplicationHandler,
        event::StartCause,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    };

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const ACTION_REFRESH: &str = "action.refresh";
    const ACTION_SETUP: &str = "action.setup";
    const ACTION_LAYOUT: &str = "action.layout";
    const ACTION_RECONNECT: &str = "action.reconnect";
    const ACTION_QUIT: &str = "action.quit";
    const ACTION_DISCOVER_PREFIX: &str = "discover.";
    const ACTION_APPROVE_PREFIX: &str = "pending.approve.";
    const ACTION_REJECT_PREFIX: &str = "pending.reject.";

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
    struct PairingSubmissionState {
        code: String,
        alias: String,
    }

    #[derive(Debug, Clone)]
    struct GuidedPairingResult {
        peer_machine_id: String,
        orientation_selector: String,
    }

    #[derive(Debug, Clone)]
    enum SetupWizardTarget {
        Discovered(UiDiscoveredPeer),
        Manual { host: String, pairing_port: u16 },
    }

    #[derive(Debug)]
    enum UserEvent {
        Menu(MenuEvent),
        Tray(TrayIconEvent),
    }

    pub(super) fn run() -> Result<()> {
        let cli = Cli::parse();
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .context("build event loop")?;

        let proxy = event_loop.create_proxy();
        tray_icon::menu::MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));
        let proxy = event_loop.create_proxy();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Tray(event));
        }));

        let mut app = TrayApp::new(AppContext {
            endpoint: cli.endpoint,
            start_daemon: cli.start_daemon,
            ctl_candidates: resolve_boundlessctl_candidates(),
        });
        event_loop.run_app(&mut app).context("run tray event loop")
    }

    struct TrayApp {
        ctx: AppContext,
        tray_icon: Option<TrayIcon>,
        snapshot: UiSnapshot,
        last_error: Option<String>,
        next_refresh_at: Instant,
    }

    impl TrayApp {
        fn new(ctx: AppContext) -> Self {
            Self {
                ctx,
                tray_icon: None,
                snapshot: UiSnapshot::default(),
                last_error: None,
                next_refresh_at: Instant::now(),
            }
        }

        fn initialize(&mut self) -> Result<()> {
            self.refresh_snapshot();
            let menu = self.build_menu()?;
            let icon = make_tray_icon()?;
            let tray = TrayIconBuilder::new()
                .with_tooltip("Boundless")
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .build()
                .context("create tray icon")?;
            self.tray_icon = Some(tray);
            self.next_refresh_at = Instant::now() + Duration::from_secs(6);
            Ok(())
        }

        fn refresh_snapshot(&mut self) {
            match fetch_ui_snapshot_blocking(&self.ctx.endpoint) {
                Ok(snapshot) => {
                    self.snapshot = snapshot;
                    self.last_error = None;
                }
                Err(api_error) => {
                    let mut args = vec!["ui".to_string(), "snapshot".to_string()];
                    if self.ctx.start_daemon {
                        args.push("--start-daemon".to_string());
                    }
                    match run_boundlessctl_with_timeout(&self.ctx, &args, Duration::from_secs(4)) {
                        Ok(stdout) => match serde_json::from_str::<UiSnapshot>(&stdout) {
                            Ok(snapshot) => {
                                self.snapshot = snapshot;
                                self.last_error = None;
                            }
                            Err(error) => {
                                self.last_error = Some(format!("parse snapshot fallback: {error}"));
                                self.snapshot = UiSnapshot::default();
                            }
                        },
                        Err(fallback_error) => {
                            self.last_error = Some(format!(
                                "snapshot fetch failed (api: {api_error}; fallback: {fallback_error})"
                            ));
                            self.snapshot = UiSnapshot::default();
                        }
                    }
                }
            }
        }

        fn rebuild_menu(&mut self) {
            let Some(tray) = &self.tray_icon else {
                return;
            };
            match self.build_menu() {
                Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                Err(error) => {
                    self.last_error = Some(format!("build menu: {error}"));
                }
            }
        }

        fn build_menu(&self) -> Result<Menu> {
            let menu = Menu::new();

            let connected = self
                .snapshot
                .paired_peers
                .iter()
                .filter(|peer| peer.connected)
                .count();

            menu.append(&MenuItem::new("Boundless Tray v1", false, None))?;
            menu.append(&MenuItem::new(
                if self.snapshot.daemon_online {
                    "daemon=online"
                } else {
                    "daemon=offline"
                },
                false,
                None,
            ))?;
            menu.append(&MenuItem::new(
                format!(
                    "discovered={} paired={} connected={} pending={}",
                    self.snapshot.discovered_peers.len(),
                    self.snapshot.paired_peers.len(),
                    connected,
                    self.snapshot.pending_requests.len()
                ),
                false,
                None,
            ))?;
            menu.append(&MenuItem::new(
                format!("machine_id={}", short_token(&self.snapshot.machine_id)),
                false,
                None,
            ))?;
            menu.append(&MenuItem::new(
                format!("layout={}", self.snapshot.layout_matrix),
                false,
                None,
            ))?;
            menu.append(&MenuItem::new(
                format!("snapshot_at={}", self.snapshot.generated_at),
                false,
                None,
            ))?;
            if let Some(error) = &self.last_error {
                menu.append(&MenuItem::new(
                    format!("last_error={}", truncate(error, 80)),
                    false,
                    None,
                ))?;
            }
            menu.append(&PredefinedMenuItem::separator())?;

            menu.append(&MenuItem::with_id(
                ACTION_SETUP,
                "First-Run Setup...",
                true,
                None,
            ))?;
            menu.append(&MenuItem::with_id(
                ACTION_LAYOUT,
                "Layout Wizard...",
                true,
                None,
            ))?;
            menu.append(&MenuItem::with_id(
                ACTION_REFRESH,
                "Refresh Now",
                true,
                None,
            ))?;
            menu.append(&PredefinedMenuItem::separator())?;

            let discovered_menu = Submenu::new("Discovered peers", true);
            if self.snapshot.discovered_peers.is_empty() {
                discovered_menu.append(&MenuItem::new("(none)", false, None))?;
            } else {
                for peer in &self.snapshot.discovered_peers {
                    discovered_menu.append(&MenuItem::with_id(
                        format!("{ACTION_DISCOVER_PREFIX}{}", peer.machine_id),
                        format!("{} [{}]", peer.display_name, short_token(&peer.machine_id)),
                        true,
                        None,
                    ))?;
                }
            }
            menu.append(&discovered_menu)?;

            let paired_menu = Submenu::new("Paired peers", true);
            if self.snapshot.paired_peers.is_empty() {
                paired_menu.append(&MenuItem::new("(none)", false, None))?;
            } else {
                for peer in &self.snapshot.paired_peers {
                    paired_menu.append(&MenuItem::new(
                        format!(
                            "{} [{}] {}",
                            peer.display_name,
                            short_token(&peer.peer_id),
                            if peer.connected {
                                "connected"
                            } else {
                                "offline"
                            }
                        ),
                        false,
                        None,
                    ))?;
                }
            }
            menu.append(&paired_menu)?;

            let connected_menu = Submenu::new("Connected peers", true);
            let connected_peers = self
                .snapshot
                .paired_peers
                .iter()
                .filter(|peer| peer.connected)
                .collect::<Vec<_>>();
            if connected_peers.is_empty() {
                connected_menu.append(&MenuItem::new("(none)", false, None))?;
            } else {
                for peer in connected_peers {
                    connected_menu.append(&MenuItem::new(
                        format!(
                            "{} [{}] address={}",
                            peer.display_name,
                            short_token(&peer.peer_id),
                            peer.address
                        ),
                        false,
                        None,
                    ))?;
                }
            }
            menu.append(&connected_menu)?;

            let pending_menu = Submenu::new("Pending pair requests", true);
            if self.snapshot.pending_requests.is_empty() {
                pending_menu.append(&MenuItem::new("(none)", false, None))?;
            } else {
                for pending in &self.snapshot.pending_requests {
                    let requires_verification_code = pending.requires_verification_code;
                    let has_verification_code =
                        requires_verification_code && !pending.verification_code.trim().is_empty();
                    let req_submenu = Submenu::new(
                        format!(
                            "{} [{}]",
                            pending.requester_display_name,
                            short_token(&pending.request_id)
                        ),
                        true,
                    );
                    req_submenu.append(&MenuItem::new(
                        format!(
                            "from={} at={}",
                            short_token(&pending.requester_machine_id),
                            pending.created_at
                        ),
                        false,
                        None,
                    ))?;
                    if requires_verification_code {
                        let detail = if has_verification_code {
                            format!(
                                "code={} expires={}",
                                pending.verification_code, pending.verification_expires_at
                            )
                        } else {
                            "code confirmation required (hidden on this endpoint)".to_string()
                        };
                        req_submenu.append(&MenuItem::new(detail, false, None))?;
                    } else {
                        req_submenu.append(&MenuItem::with_id(
                            format!("{ACTION_APPROVE_PREFIX}{}", pending.request_id),
                            "Approve",
                            true,
                            None,
                        ))?;
                    }
                    req_submenu.append(&MenuItem::with_id(
                        format!("{ACTION_REJECT_PREFIX}{}", pending.request_id),
                        if requires_verification_code {
                            "Cancel"
                        } else {
                            "Reject"
                        },
                        true,
                        None,
                    ))?;
                    pending_menu.append(&req_submenu)?;
                }
            }
            menu.append(&pending_menu)?;

            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&MenuItem::with_id(
                ACTION_RECONNECT,
                "Reconnect All Peers",
                true,
                None,
            ))?;
            menu.append(&MenuItem::with_id(ACTION_QUIT, "Quit", true, None))?;
            Ok(menu)
        }

        fn handle_menu_event(&mut self, event: MenuEvent, event_loop: &ActiveEventLoop) {
            let menu_id = event.id.as_ref();
            let result = if menu_id == ACTION_REFRESH {
                self.refresh_snapshot();
                self.rebuild_menu();
                Ok(())
            } else if menu_id == ACTION_SETUP {
                self.run_setup_wizard()
            } else if menu_id == ACTION_LAYOUT {
                self.run_layout_wizard()
            } else if menu_id == ACTION_RECONNECT {
                self.run_reconnect_action()
            } else if menu_id == ACTION_QUIT {
                event_loop.exit();
                Ok(())
            } else if let Some(machine_id) = menu_id.strip_prefix(ACTION_DISCOVER_PREFIX) {
                self.run_pair_request(machine_id)
            } else if let Some(request_id) = menu_id.strip_prefix(ACTION_APPROVE_PREFIX) {
                self.approve_pending_request(request_id)
            } else if let Some(request_id) = menu_id.strip_prefix(ACTION_REJECT_PREFIX) {
                self.reject_pending_request(request_id)
            } else {
                Ok(())
            };

            if let Err(error) = result {
                message_box_ok(
                    "Boundless",
                    &format_error_for_dialog(&error),
                    MessageBoxIcon::Error,
                );
            }
            self.refresh_snapshot();
            self.rebuild_menu();
        }

        fn run_reconnect_action(&self) -> Result<()> {
            let output = trigger_hotkey_action_blocking(&self.ctx.endpoint, "reconnect")?;
            message_box_ok(
                "Boundless",
                &format!("Reconnect completed:\n{output}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn approve_pending_request(&self, request_id: &str) -> Result<()> {
            let message = approve_nearby_pairing_request_blocking(&self.ctx.endpoint, request_id)?;
            message_box_ok(
                "Boundless",
                &format!("Approve Request completed:\n{message}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn reject_pending_request(&self, request_id: &str) -> Result<()> {
            let message = reject_nearby_pairing_request_blocking(&self.ctx.endpoint, request_id)?;
            message_box_ok(
                "Boundless",
                &format!("Reject Request completed:\n{message}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn run_pair_request(&self, machine_id: &str) -> Result<()> {
            let flow = self.guided_pairing_flow_for_discovered(machine_id, "Boundless Pairing")?;
            let Some(result) = self.run_guided_pairing_flow_with_recovery(&flow)? else {
                return Ok(());
            };
            message_box_ok(
                "Boundless",
                &format!(
                    "Pairing request completed.\npeer_machine_id={}\ntarget={}:{}",
                    short_token(&result.peer_machine_id),
                    flow.host,
                    flow.pairing_port
                ),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn run_setup_wizard(&self) -> Result<()> {
            let target = self.select_setup_wizard_target()?;
            let flow = self.guided_pairing_flow_for_setup_target(target)?;
            let Some(result) = self.run_guided_pairing_flow_with_recovery(&flow)? else {
                return Ok(());
            };
            message_box_ok(
                "Boundless Setup",
                &format!(
                    "Pairing completed.\npeer_machine_id={}\ntarget={}:{}",
                    short_token(&result.peer_machine_id),
                    flow.host,
                    flow.pairing_port
                ),
                MessageBoxIcon::Info,
            );
            self.prompt_orientation(result.orientation_selector)
        }

        fn select_setup_wizard_target(&self) -> Result<SetupWizardTarget> {
            if self.snapshot.discovered_peers.is_empty() {
                return self
                    .prompt_manual_setup_target("No discovered peers found.\nEnter host/IP:");
            }

            let mut choices =
                String::from("Discovered peers:\n\nType an index or machine_id (or `manual`).\n\n");
            for (index, peer) in self.snapshot.discovered_peers.iter().enumerate() {
                choices.push_str(&format!(
                    "[{}] {} machine={} endpoint={}\n",
                    index + 1,
                    peer.display_name,
                    short_token(&peer.machine_id),
                    peer.endpoint
                ));
            }

            let selector = input_box("Boundless Setup", &choices, "1")
                .ok_or_else(|| anyhow::anyhow!("setup cancelled"))?;
            let selector = selector.trim().to_string();
            if selector.eq_ignore_ascii_case("manual") {
                return self.prompt_manual_setup_target("Enter host/IP:");
            }

            let selected =
                resolve_discovered_peer(&self.snapshot.discovered_peers, &selector)?.clone();
            Ok(SetupWizardTarget::Discovered(selected))
        }

        fn prompt_manual_setup_target(&self, host_prompt: &str) -> Result<SetupWizardTarget> {
            let host = input_box("Boundless Setup", host_prompt, "")
                .ok_or_else(|| anyhow::anyhow!("setup cancelled"))?;
            let host = host.trim().to_string();
            if host.is_empty() {
                bail!("host/IP is required");
            }

            let port = input_box("Boundless Setup", "Pairing port:", "15200")
                .unwrap_or_else(|| "15200".to_string());
            let pairing_port = parse_pairing_port(port.trim())?;
            Ok(SetupWizardTarget::Manual { host, pairing_port })
        }

        fn guided_pairing_flow_for_setup_target(
            &self,
            target: SetupWizardTarget,
        ) -> Result<GuidedPairingFlow> {
            match target {
                SetupWizardTarget::Discovered(peer) => {
                    self.guided_pairing_flow_for_discovered(&peer.machine_id, "Boundless Setup")
                }
                SetupWizardTarget::Manual { host, pairing_port } => Ok(GuidedPairingFlow {
                    dialog_title: "Boundless Setup".to_string(),
                    host: host.clone(),
                    pairing_port,
                    default_alias: String::new(),
                    orientation_selector_fallback: host,
                }),
            }
        }

        fn guided_pairing_flow_for_discovered(
            &self,
            machine_id: &str,
            dialog_title: &str,
        ) -> Result<GuidedPairingFlow> {
            let discovered_peer = self
                .snapshot
                .discovered_peers
                .iter()
                .find(|peer| peer.machine_id == machine_id)
                .ok_or_else(|| anyhow::anyhow!("discovered peer not found for {machine_id}"))?;
            let (host, pairing_port) =
                host_and_pairing_port_from_discovery_endpoint(&discovered_peer.endpoint)
                    .ok_or_else(|| {
                        anyhow::anyhow!("invalid discovered endpoint {}", discovered_peer.endpoint)
                    })?;

            Ok(GuidedPairingFlow {
                dialog_title: dialog_title.to_string(),
                host,
                pairing_port,
                default_alias: discovered_peer.display_name.clone(),
                orientation_selector_fallback: discovered_peer.display_name.clone(),
            })
        }

        fn run_guided_pairing_flow(&self, flow: &GuidedPairingFlow) -> Result<GuidedPairingResult> {
            let challenge = self.request_pairing_challenge_state(flow)?;
            let submission = self.prompt_pairing_submission_state(flow, &challenge)?;
            self.submit_pairing_submission_state(flow, challenge, submission)
        }

        fn run_guided_pairing_flow_with_recovery(
            &self,
            flow: &GuidedPairingFlow,
        ) -> Result<Option<GuidedPairingResult>> {
            loop {
                match self.run_guided_pairing_flow(flow) {
                    Ok(result) => return Ok(Some(result)),
                    Err(error) => {
                        if !should_offer_new_request_retry(&error) {
                            return Err(error);
                        }

                        let retry = message_box_yes_no(
                            "Boundless Pairing",
                            &format!(
                                "{}\n\nWould you like to start a new pairing request now?",
                                format_error_for_dialog(&error)
                            ),
                            MessageBoxIcon::Warning,
                            YesNo::No,
                        );
                        if retry != YesNo::Yes {
                            return Ok(None);
                        }
                    }
                }
            }
        }

        fn request_pairing_challenge_state(
            &self,
            flow: &GuidedPairingFlow,
        ) -> Result<PairingChallengeState> {
            match pair_nearby_request_code_blocking(
                &self.ctx.endpoint,
                flow.host.clone(),
                flow.pairing_port,
            )? {
                NearbyRequestCodeStart::CodeRequired {
                    request_id,
                    verification_nonce,
                    expires_at,
                } => Ok(PairingChallengeState {
                    request_id,
                    verification_nonce,
                    expires_at,
                }),
                NearbyRequestCodeStart::Unsupported { reason } => {
                    bail!(
                        "target does not support guided nearby pairing on {}:{} ({reason})",
                        flow.host,
                        flow.pairing_port
                    );
                }
            }
        }

        fn prompt_pairing_submission_state(
            &self,
            flow: &GuidedPairingFlow,
            challenge: &PairingChallengeState,
        ) -> Result<PairingSubmissionState> {
            let code = input_box(
                &flow.dialog_title,
                &format!(
                    "Request sent to target.\nAsk for the 6-digit code shown there.\nRequest ID: {}\nExpires: {}\n\nEnter code:",
                    short_token(&challenge.request_id),
                    challenge.expires_at
                ),
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("pairing cancelled"))?;
            let code = code.trim().to_string();
            if code.is_empty() {
                bail!("pairing code cannot be empty");
            }

            let alias = input_box(
                &flow.dialog_title,
                "Alias for this peer (optional):",
                &flow.default_alias,
            )
            .unwrap_or_default()
            .trim()
            .to_string();

            Ok(PairingSubmissionState { code, alias })
        }

        fn submit_pairing_submission_state(
            &self,
            flow: &GuidedPairingFlow,
            challenge: PairingChallengeState,
            submission: PairingSubmissionState,
        ) -> Result<GuidedPairingResult> {
            let peer_machine_id = pair_nearby_submit_code_blocking(
                &self.ctx.endpoint,
                challenge.request_id,
                submission.code,
                challenge.verification_nonce,
                flow.host.clone(),
                flow.pairing_port,
                Some(submission.alias.clone()).filter(|value| !value.is_empty()),
            )?;
            let orientation_selector = if submission.alias.is_empty() {
                flow.orientation_selector_fallback.clone()
            } else {
                submission.alias
            };

            Ok(GuidedPairingResult {
                peer_machine_id,
                orientation_selector,
            })
        }

        fn prompt_orientation(&self, default_selector: String) -> Result<()> {
            let side = input_box(
                "Boundless Layout",
                "Place the paired peer relative to this PC:\nleft | right | up | down | skip",
                "skip",
            )
            .unwrap_or_else(|| "skip".to_string())
            .trim()
            .to_ascii_lowercase();

            let flag = match side.as_str() {
                "left" | "l" => Some("--left"),
                "right" | "r" => Some("--right"),
                "up" | "u" | "top" => Some("--up"),
                "down" | "d" | "bottom" => Some("--down"),
                _ => None,
            };

            if let Some(flag) = flag {
                let selector = input_box(
                    "Boundless Layout",
                    "Peer selector for orientation:",
                    &default_selector,
                )
                .unwrap_or(default_selector)
                .trim()
                .to_string();
                if selector.is_empty() {
                    bail!("orientation selector cannot be empty");
                }

                let args = vec![
                    "layout".to_string(),
                    "orient".to_string(),
                    flag.to_string(),
                    selector,
                ];
                let output = run_boundlessctl(&self.ctx, &args)?;
                message_box_ok(
                    "Boundless Layout",
                    &format!("Layout updated:\n{output}"),
                    MessageBoxIcon::Info,
                );
            }
            Ok(())
        }

        fn run_layout_wizard(&self) -> Result<()> {
            let mut peer_list = String::from("Paired peers:\n\n");
            for (index, peer) in self.snapshot.paired_peers.iter().enumerate() {
                peer_list.push_str(&format!(
                    "[{}] {} [{}] {}\n",
                    index + 1,
                    peer.display_name,
                    short_token(&peer.peer_id),
                    if peer.connected {
                        "connected"
                    } else {
                        "offline"
                    }
                ));
            }
            peer_list.push_str(
                "\nUse index, peer_id, or display name prefix. Leave blank to skip a side.",
            );
            message_box_ok("Boundless Layout", &peer_list, MessageBoxIcon::Info);

            let left = input_box("Boundless Layout", "Left peer selector (optional):", "")
                .unwrap_or_default();
            let right = input_box("Boundless Layout", "Right peer selector (optional):", "")
                .unwrap_or_default();
            let up = input_box("Boundless Layout", "Up peer selector (optional):", "")
                .unwrap_or_default();
            let down = input_box("Boundless Layout", "Down peer selector (optional):", "")
                .unwrap_or_default();

            let mut args = vec!["layout".to_string(), "orient".to_string()];
            if !left.trim().is_empty() {
                args.push("--left".to_string());
                args.push(left.trim().to_string());
            }
            if !right.trim().is_empty() {
                args.push("--right".to_string());
                args.push(right.trim().to_string());
            }
            if !up.trim().is_empty() {
                args.push("--up".to_string());
                args.push(up.trim().to_string());
            }
            if !down.trim().is_empty() {
                args.push("--down".to_string());
                args.push(down.trim().to_string());
            }
            if args.len() == 2 {
                message_box_ok(
                    "Boundless Layout",
                    "No layout changes entered.",
                    MessageBoxIcon::Info,
                );
                return Ok(());
            }

            if message_box_yes_no(
                "Boundless Layout",
                "Apply layout changes now?",
                MessageBoxIcon::Question,
                YesNo::Yes,
            ) == YesNo::No
            {
                return Ok(());
            }

            let output = run_boundlessctl(&self.ctx, &args)?;
            message_box_ok(
                "Boundless Layout",
                &format!("Layout updated:\n{output}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }
    }

    impl ApplicationHandler<UserEvent> for TrayApp {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            if cause == StartCause::Init
                && let Err(error) = self.initialize()
            {
                message_box_ok(
                    "Boundless Tray",
                    &format!("failed to initialize tray UI: {error}"),
                    MessageBoxIcon::Error,
                );
                event_loop.exit();
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::Menu(event) => self.handle_menu_event(event, event_loop),
                UserEvent::Tray(_event) => {}
            }
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: winit::window::WindowId,
            _event: winit::event::WindowEvent,
        ) {
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if Instant::now() >= self.next_refresh_at {
                self.refresh_snapshot();
                self.rebuild_menu();
                self.next_refresh_at = Instant::now() + Duration::from_secs(6);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh_at));
        }
    }

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
        let mut socket = TcpStream::connect(target)
            .await
            .with_context(|| format!("connect nearby pairing endpoint {target}"))?;
        let payload =
            serde_json::to_string(&request).context("serialize nearby pairing request")?;
        socket
            .write_all(payload.as_bytes())
            .await
            .context("send nearby pairing request")?;
        socket
            .write_all(b"\n")
            .await
            .context("terminate nearby pairing request")?;
        socket
            .flush()
            .await
            .context("flush nearby pairing request")?;

        let mut reader = BufReader::new(socket);
        let mut response_line = String::new();
        let read = reader
            .read_line(&mut response_line)
            .await
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

        message
    }

    fn should_offer_new_request_retry(error: &anyhow::Error) -> bool {
        let lowered = error.to_string().to_ascii_lowercase();
        lowered.contains("pairing request rejected")
            || lowered.contains("verification code expired")
            || lowered.contains("timed out waiting for nearby pairing approval")
            || lowered.contains("nearby pairing request not found")
            || lowered.contains("nearby pairing endpoint closed without a response")
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
    }
}
