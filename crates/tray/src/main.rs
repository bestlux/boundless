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
    use serde::Deserialize;
    use std::{
        os::windows::process::CommandExt,
        process::{Command as ProcessCommand, Stdio},
        thread::sleep,
        time::{Duration, Instant},
    };
    use tinyfiledialogs::{MessageBoxIcon, YesNo, input_box, message_box_ok, message_box_yes_no};
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
                        self.last_error = Some(format!("parse snapshot: {error}"));
                        self.snapshot = UiSnapshot::default();
                    }
                },
                Err(error) => {
                    self.last_error = Some(error.to_string());
                    self.snapshot = UiSnapshot::default();
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

            menu.append(&MenuItem::new("Boundless Tray v2", false, None))?;
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
                self.run_simple_command(
                    vec![
                        "diagnostics".to_string(),
                        "run-action".to_string(),
                        "reconnect".to_string(),
                    ],
                    "Reconnect",
                )
            } else if menu_id == ACTION_QUIT {
                event_loop.exit();
                Ok(())
            } else if let Some(machine_id) = menu_id.strip_prefix(ACTION_DISCOVER_PREFIX) {
                self.run_pair_request(machine_id)
            } else if let Some(request_id) = menu_id.strip_prefix(ACTION_APPROVE_PREFIX) {
                self.run_simple_command(
                    vec![
                        "pair".to_string(),
                        "approve".to_string(),
                        request_id.to_string(),
                    ],
                    "Approve Request",
                )
            } else if let Some(request_id) = menu_id.strip_prefix(ACTION_REJECT_PREFIX) {
                self.run_simple_command(
                    vec![
                        "pair".to_string(),
                        "reject".to_string(),
                        request_id.to_string(),
                    ],
                    "Reject Request",
                )
            } else {
                Ok(())
            };

            if let Err(error) = result {
                message_box_ok("Boundless", &error.to_string(), MessageBoxIcon::Error);
            }
            self.refresh_snapshot();
            self.rebuild_menu();
        }

        fn run_simple_command(&self, args: Vec<String>, title: &str) -> Result<()> {
            let output = run_boundlessctl(&self.ctx, &args)?;
            message_box_ok(
                "Boundless",
                &format!("{title} completed:\n{output}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn run_pair_request(&self, machine_id: &str) -> Result<()> {
            let default_alias = self
                .snapshot
                .discovered_peers
                .iter()
                .find(|peer| peer.machine_id == machine_id)
                .map(|peer| peer.display_name.as_str())
                .unwrap_or("");
            let target_override = self
                .snapshot
                .discovered_peers
                .iter()
                .find(|peer| peer.machine_id == machine_id)
                .and_then(|peer| host_and_pairing_port_from_discovery_endpoint(&peer.endpoint));

            let mut start_args = vec![
                "pair".to_string(),
                "request".to_string(),
                machine_id.to_string(),
                "--timeout-seconds".to_string(),
                "120".to_string(),
            ];
            if let Some((host, pairing_port)) = &target_override {
                start_args.push("--host".to_string());
                start_args.push(host.clone());
                start_args.push("--port".to_string());
                start_args.push(pairing_port.to_string());
            }

            let start_output = match run_boundlessctl(&self.ctx, &start_args) {
                Ok(output) => output,
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("does not support guided pairing request flow") {
                        return self.run_pair_request_legacy(machine_id, default_alias);
                    }
                    return Err(error);
                }
            };

            let request_id = parse_key_value(&start_output, "request_id").ok_or_else(|| {
                anyhow::anyhow!("pairing response missing request_id: {start_output}")
            })?;
            let expires_at =
                parse_key_value(&start_output, "expires_at").unwrap_or_else(|| "soon".to_string());

            let code = input_box(
                "Boundless Pairing",
                &format!(
                    "Request sent to target.\nAsk for the 6-digit code shown there.\nRequest ID: {}\nExpires: {}\n\nEnter code:",
                    short_token(&request_id),
                    expires_at
                ),
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("pair request cancelled"))?;
            let code = code.trim().to_string();
            if code.is_empty() {
                bail!("pairing code cannot be empty");
            }

            let alias = input_box(
                "Boundless Pairing",
                "Alias for this peer (optional):",
                default_alias,
            )
            .unwrap_or_default();

            let mut submit_args = vec![
                "pair".to_string(),
                "request".to_string(),
                machine_id.to_string(),
                "--request-id".to_string(),
                request_id,
                "--code".to_string(),
                code,
                "--timeout-seconds".to_string(),
                "120".to_string(),
            ];
            if let Some((host, pairing_port)) = &target_override {
                submit_args.push("--host".to_string());
                submit_args.push(host.clone());
                submit_args.push("--port".to_string());
                submit_args.push(pairing_port.to_string());
            }
            if !alias.trim().is_empty() {
                submit_args.push("--alias".to_string());
                submit_args.push(alias.trim().to_string());
            }

            let output = run_boundlessctl(&self.ctx, &submit_args)?;
            message_box_ok(
                "Boundless",
                &format!("Pairing request completed:\n{output}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn run_pair_request_legacy(&self, machine_id: &str, default_alias: &str) -> Result<()> {
            let code = input_box(
                "Boundless Pairing",
                "Target does not support guided pairing yet.\nEnter the 6-digit code shown on the target machine:",
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("pair request cancelled"))?;
            let code = code.trim().to_string();
            if code.is_empty() {
                bail!("pairing code cannot be empty");
            }

            let alias = input_box(
                "Boundless Pairing",
                "Alias for this peer (optional):",
                default_alias,
            )
            .unwrap_or_default();

            let mut args = vec![
                "pair".to_string(),
                "request".to_string(),
                machine_id.to_string(),
                "--code".to_string(),
                code,
                "--timeout-seconds".to_string(),
                "120".to_string(),
            ];
            if !alias.trim().is_empty() {
                args.push("--alias".to_string());
                args.push(alias.trim().to_string());
            }

            let output = run_boundlessctl(&self.ctx, &args)?;
            message_box_ok(
                "Boundless",
                &format!("Pairing request completed:\n{output}"),
                MessageBoxIcon::Info,
            );
            Ok(())
        }

        fn run_setup_wizard(&self) -> Result<()> {
            if self.snapshot.discovered_peers.is_empty() {
                return self.run_setup_wizard_manual();
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
                return self.run_setup_wizard_manual();
            }

            let code = input_box(
                "Boundless Setup",
                "Enter the 6-digit code shown on the target machine:",
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("setup cancelled"))?;
            let code = code.trim().to_string();
            if code.is_empty() {
                bail!("pairing code cannot be empty");
            }
            let alias = input_box("Boundless Setup", "Alias for this peer (optional):", "")
                .unwrap_or_default();

            let mut args = vec![
                "pair".to_string(),
                "request".to_string(),
                selector.clone(),
                "--code".to_string(),
                code,
                "--timeout-seconds".to_string(),
                "120".to_string(),
            ];
            if !alias.trim().is_empty() {
                args.push("--alias".to_string());
                args.push(alias.trim().to_string());
            }
            let output = run_boundlessctl(&self.ctx, &args)?;
            message_box_ok(
                "Boundless Setup",
                &format!("Pairing completed:\n{output}"),
                MessageBoxIcon::Info,
            );

            let orientation_selector = if !alias.trim().is_empty() {
                alias.trim().to_string()
            } else {
                selector
            };
            self.prompt_orientation(orientation_selector)
        }

        fn run_setup_wizard_manual(&self) -> Result<()> {
            let host = input_box(
                "Boundless Setup",
                "No discovered peers found.\nEnter host/IP:",
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("setup cancelled"))?;
            let host = host.trim().to_string();
            if host.is_empty() {
                bail!("host/IP is required");
            }

            let code = input_box(
                "Boundless Setup",
                "Enter the 6-digit code shown on the target machine:",
                "",
            )
            .ok_or_else(|| anyhow::anyhow!("setup cancelled"))?;
            let code = code.trim().to_string();
            if code.is_empty() {
                bail!("pairing code cannot be empty");
            }

            let port = input_box("Boundless Setup", "Pairing port:", "15200")
                .unwrap_or_else(|| "15200".to_string());
            let alias = input_box("Boundless Setup", "Alias for this peer (optional):", "")
                .unwrap_or_default();

            let mut args = vec![
                "pair".to_string(),
                "nearby-join".to_string(),
                code,
                "--host".to_string(),
                host.clone(),
                "--port".to_string(),
                port.trim().to_string(),
            ];
            if !alias.trim().is_empty() {
                args.push("--alias".to_string());
                args.push(alias.trim().to_string());
            }

            let output = run_boundlessctl(&self.ctx, &args)?;
            message_box_ok(
                "Boundless Setup",
                &format!("Pairing completed:\n{output}"),
                MessageBoxIcon::Info,
            );

            let orientation_selector = if !alias.trim().is_empty() {
                alias.trim().to_string()
            } else {
                host
            };
            self.prompt_orientation(orientation_selector)
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

    fn parse_key_value(output: &str, key: &str) -> Option<String> {
        let prefix = format!("{key}=");
        output
            .split_whitespace()
            .find_map(|token| token.strip_prefix(&prefix))
            .map(ToString::to_string)
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
}
