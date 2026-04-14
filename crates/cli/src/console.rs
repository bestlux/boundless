use super::*;
use ipc_api::boundless::v1::control_plane_service_client::ControlPlaneServiceClient;

#[derive(Debug)]
pub(super) struct ConsolePeer {
    pub(super) peer_id: String,
    pub(super) display_name: String,
    pub(super) address: String,
    pub(super) connected: bool,
}

#[derive(Debug)]
pub(super) struct ConsoleDiscoveredPeer {
    pub(super) machine_id: String,
    pub(super) display_name: String,
    pub(super) endpoint: String,
}

#[derive(Debug)]
pub(super) struct ConsolePendingRequest {
    pub(super) request_id: String,
    pub(super) requester_display_name: String,
}

#[derive(Debug)]
pub(super) struct ConsoleSnapshot {
    pub(super) status: StatusReply,
    pub(super) peers: Vec<ConsolePeer>,
    pub(super) features: Vec<(String, bool)>,
    pub(super) discovered_peers: Vec<ConsoleDiscoveredPeer>,
    pub(super) pending_requests: Vec<ConsolePendingRequest>,
    pub(super) input_owner: Option<String>,
    pub(super) capture_target: Option<String>,
    pub(super) mdns_active: bool,
    pub(super) anti_idle_reason: String,
}

impl ConsoleSnapshot {
    fn feature_enabled(&self, name: &str) -> Option<bool> {
        self.features
            .iter()
            .find(|(feature, _)| feature == name)
            .map(|(_, enabled)| *enabled)
    }
}

pub(super) async fn console_run(endpoint: &str, start_daemon: bool) -> Result<()> {
    ensure_daemon_available(endpoint, start_daemon).await?;

    println!("Boundless interactive console");
    println!("endpoint={endpoint}");
    println!("type `help` for commands, `q` to exit");

    loop {
        let snapshot = fetch_console_snapshot(endpoint).await?;
        print_console_snapshot(endpoint, &snapshot);

        print!("boundless> ");
        io::stdout().flush().context("flush stdout")?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("read console input")?;
        let line = line.trim();

        if line.is_empty() || line.eq_ignore_ascii_case("r") || line.eq_ignore_ascii_case("refresh")
        {
            continue;
        }
        if line.eq_ignore_ascii_case("q")
            || line.eq_ignore_ascii_case("quit")
            || line.eq_ignore_ascii_case("exit")
        {
            break;
        }
        if line.eq_ignore_ascii_case("help") {
            print_console_help();
            continue;
        }

        if let Err(error) = handle_console_command(endpoint, &snapshot, line).await {
            eprintln!("error: {error:#}");
        }
    }

    Ok(())
}

async fn ensure_daemon_available(endpoint: &str, start_daemon: bool) -> Result<()> {
    super::commands::ensure_daemon_available(endpoint, start_daemon).await
}

async fn fetch_console_snapshot(endpoint: &str) -> Result<ConsoleSnapshot> {
    let mut control_plane = ControlPlaneServiceClient::new(channel(endpoint).await?);
    let snapshot = control_plane
        .get_console_snapshot(Empty {})
        .await?
        .into_inner();

    let status = snapshot
        .status
        .ok_or_else(|| anyhow::anyhow!("console snapshot missing status payload"))?;

    let peers = snapshot
        .peers
        .into_iter()
        .map(|peer| ConsolePeer {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            address: peer.address,
            connected: peer.connected,
        })
        .collect::<Vec<_>>();

    let mut features = snapshot.features.into_iter().collect::<Vec<_>>();
    features.sort_by(|a, b| a.0.cmp(&b.0));

    let discovered_peers = snapshot
        .discovered_peers
        .into_iter()
        .map(|peer| ConsoleDiscoveredPeer {
            machine_id: peer.machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint,
        })
        .collect::<Vec<_>>();
    let paired_peer_ids = peers
        .iter()
        .map(|peer| peer.peer_id.clone())
        .collect::<Vec<_>>();
    let mut discovered_peers = filter_connectable_discovery_records(
        discovered_peers,
        &status.machine_id,
        &paired_peer_ids,
        |peer| peer.machine_id.clone(),
    );
    discovered_peers.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.machine_id.cmp(&b.machine_id))
    });

    let input_owner = if snapshot.input_owner_peer_id.trim().is_empty() {
        None
    } else {
        Some(snapshot.input_owner_peer_id)
    };

    let capture_target = if snapshot.input_capture_target_peer_id.trim().is_empty() {
        None
    } else {
        Some(snapshot.input_capture_target_peer_id)
    };

    let pending_requests = snapshot
        .pending_requests
        .into_iter()
        .map(|request| ConsolePendingRequest {
            request_id: request.request_id,
            requester_display_name: request.requester_display_name,
        })
        .collect::<Vec<_>>();

    Ok(ConsoleSnapshot {
        status,
        peers,
        features,
        discovered_peers,
        pending_requests,
        input_owner,
        capture_target,
        mdns_active: snapshot.mdns_active,
        anti_idle_reason: snapshot
            .anti_idle_status
            .as_ref()
            .map(|status| status.reason.clone())
            .unwrap_or_else(|| "none".to_string()),
    })
}

fn print_console_snapshot(endpoint: &str, snapshot: &ConsoleSnapshot) {
    println!();
    println!("=== Boundless Status ===");
    println!(
        "daemon=running endpoint={} machine_id={} protocol={} input_locked={} input_lock_supported={} active_capture_target={} anti_idle_supported={} anti_idle_enabled={} anti_idle_active={} anti_idle_display_required={} anti_idle_reason={}",
        endpoint,
        snapshot.status.machine_id,
        snapshot.status.protocol_version,
        snapshot.status.input_locked,
        snapshot.status.input_lock_supported,
        if snapshot.status.capture_target_peer_id.is_empty() {
            "none"
        } else {
            snapshot.status.capture_target_peer_id.as_str()
        },
        snapshot.status.anti_idle_supported,
        snapshot.status.anti_idle_enabled,
        snapshot.status.anti_idle_active,
        snapshot.status.anti_idle_display_required,
        snapshot.anti_idle_reason
    );
    println!(
        "api_transport={} api_bind={} api_pipe_name={}",
        snapshot.status.api_transport, snapshot.status.api_bind, snapshot.status.api_pipe_name
    );

    let mdns = if snapshot.mdns_active {
        "searching"
    } else {
        "stopped"
    };
    println!(
        "mdns={} discovered_peers={}",
        mdns,
        snapshot.discovered_peers.len()
    );
    for (index, peer) in snapshot.discovered_peers.iter().enumerate() {
        println!(
            "  [{}] discovered name={} endpoint={} machine_id={}",
            index + 1,
            peer.display_name,
            peer.endpoint,
            short_machine_id(&peer.machine_id),
        );
    }

    println!("trusted_peers={}", snapshot.peers.len());
    for peer in &snapshot.peers {
        let owner = if snapshot.input_owner.as_deref() == Some(peer.peer_id.as_str()) {
            " owner"
        } else {
            ""
        };
        let capture = if snapshot.capture_target.as_deref() == Some(peer.peer_id.as_str()) {
            " capture"
        } else {
            ""
        };
        println!(
            "  peer_id={} name={} address={} connected={}{}{}",
            peer.peer_id, peer.display_name, peer.address, peer.connected, owner, capture
        );
    }

    println!("features:");
    for (name, enabled) in &snapshot.features {
        println!("  {name}={enabled}");
    }

    println!(
        "input_owner={} capture_target={} pending_pair_requests={}",
        snapshot.input_owner.as_deref().unwrap_or("none"),
        snapshot.capture_target.as_deref().unwrap_or("none"),
        snapshot.pending_requests.len()
    );
    for request in &snapshot.pending_requests {
        println!(
            "  pending request_id={} requester={}",
            request.request_id, request.requester_display_name
        );
    }
}

fn print_console_help() {
    println!("commands:");
    println!("  refresh | r");
    println!("  quit | q");
    println!("  toggle <feature_name>");
    println!("  set <feature_name> <on|off>");
    println!("  control <peer_id|off>");
    println!("  reconnect");
    println!("  pair code [ttl_seconds]");
    println!("  pair pending");
    println!("  pair approve <request_id> [alias]");
    println!("  pair reject <request_id>");
    println!("  pair request <index|machine_id|display-name> [code] [alias]");
    println!("  pair nearby <host> <code> [port] [alias]");
}

async fn handle_console_command(
    endpoint: &str,
    snapshot: &ConsoleSnapshot,
    line: &str,
) -> Result<()> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        return Ok(());
    }

    match parts[0] {
        "toggle" => {
            if parts.len() != 2 {
                bail!("usage: toggle <feature_name>");
            }
            let name = parts[1];
            let current = snapshot
                .feature_enabled(name)
                .ok_or_else(|| anyhow::anyhow!("unknown feature {name}"))?;
            let next = if current {
                ToggleValue::Off
            } else {
                ToggleValue::On
            };
            feature_set(endpoint, name.to_string(), next).await
        }
        "set" => {
            if parts.len() != 3 {
                bail!("usage: set <feature_name> <on|off>");
            }
            let value = match parts[2] {
                "on" | "true" | "1" => ToggleValue::On,
                "off" | "false" | "0" => ToggleValue::Off,
                _ => bail!("value must be on|off"),
            };
            feature_set(endpoint, parts[1].to_string(), value).await
        }
        "control" => {
            if parts.len() != 2 {
                bail!("usage: control <peer_id|off>");
            }
            let target = parts[1];
            if target.eq_ignore_ascii_case("off") {
                input_capture_stop(endpoint).await?;
                if let Some(owner_peer_id) = &snapshot.input_owner {
                    let _ = input_release(endpoint, owner_peer_id.clone()).await;
                }
                println!("control=off");
                Ok(())
            } else {
                if snapshot.feature_enabled("share_input") == Some(false) {
                    feature_set(endpoint, "share_input".to_string(), ToggleValue::On).await?;
                }
                input_capture_start(endpoint, target.to_string()).await?;
                input_claim(endpoint, target.to_string(), false).await?;
                println!("control=on peer_id={target}");
                Ok(())
            }
        }
        "reconnect" => diagnostics_run_action(endpoint, "reconnect".to_string()).await,
        "pair" => handle_console_pair_command(endpoint, snapshot, &parts[1..]).await,
        _ => bail!("unknown command `{}`; run `help`", parts[0]),
    }
}

async fn handle_console_pair_command(
    endpoint: &str,
    snapshot: &ConsoleSnapshot,
    args: &[&str],
) -> Result<()> {
    if args.is_empty() {
        bail!("usage: pair <code|pending|approve|reject|request|nearby> ...");
    }

    match args[0] {
        "code" => {
            let ttl = if args.len() >= 2 {
                args[1]
                    .parse::<u32>()
                    .context("pair code ttl must be a positive integer")?
            } else {
                300
            };
            pair_create_code(endpoint, ttl).await
        }
        "pending" => pair_pending(endpoint).await,
        "approve" => {
            if args.len() < 2 {
                bail!("usage: pair approve <request_id> [alias]");
            }
            let alias = args.get(2).map(|value| (*value).to_string());
            let request_id = if args[1].eq_ignore_ascii_case("latest") {
                snapshot
                    .pending_requests
                    .last()
                    .map(|request| request.request_id.clone())
                    .ok_or_else(|| anyhow::anyhow!("no pending pairing requests"))?
            } else {
                args[1].to_string()
            };
            pair_approve(endpoint, request_id, alias).await
        }
        "reject" => {
            if args.len() != 2 {
                bail!("usage: pair reject <request_id>");
            }
            pair_reject(endpoint, args[1].to_string()).await
        }
        "request" => {
            if args.len() < 2 {
                bail!("usage: pair request <index|machine_id|display-name> [code] [alias]");
            }

            let discovered = resolve_discovered_peer(snapshot, args[1])?;
            let (host, pairing_port) = host_and_pairing_port_from_discovery_endpoint(
                &discovered.endpoint,
            )
            .with_context(|| format!("invalid discovered endpoint {}", discovered.endpoint))?;

            let code = if let Some(code) = args.get(2) {
                code.to_string()
            } else {
                prompt_pairing_code()?
            };
            if code.trim().is_empty() {
                bail!("pairing code must not be empty");
            }

            let alias = args.get(3).map(|value| (*value).to_string());
            println!(
                "pair_request target={} endpoint={} pairing_port={} machine_id={}",
                discovered.display_name,
                discovered.endpoint,
                pairing_port,
                short_machine_id(&discovered.machine_id),
            );
            pair_nearby_join(endpoint, code, host, pairing_port, 120, alias).await
        }
        "nearby" => {
            if args.len() < 3 {
                bail!("usage: pair nearby <host> <code> [port] [alias]");
            }
            let host = args[1].to_string();
            let code = args[2].to_string();
            let mut port = 15200_u16;
            let mut alias = None;
            if let Some(arg) = args.get(3) {
                if let Ok(parsed) = arg.parse::<u16>() {
                    port = parsed;
                    if let Some(alias_arg) = args.get(4) {
                        alias = Some((*alias_arg).to_string());
                    }
                    if args.len() > 5 {
                        bail!("usage: pair nearby <host> <code> [port] [alias]");
                    }
                } else {
                    alias = Some((*arg).to_string());
                    if args.len() > 4 {
                        bail!("usage: pair nearby <host> <code> [port] [alias]");
                    }
                }
            }
            pair_nearby_join(endpoint, code, host, port, 120, alias).await
        }
        _ => bail!("unknown pair command `{}`", args[0]),
    }
}
