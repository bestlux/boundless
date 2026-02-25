use super::*;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub(super) async fn ensure_daemon_available(endpoint: &str, start_daemon: bool) -> Result<()> {
    if channel(endpoint).await.is_ok() {
        return Ok(());
    }

    if !start_daemon {
        bail!("daemon is not reachable at {endpoint}; run boundlessd or pass --start-daemon");
    }

    let launched = spawn_daemon_process()?;
    println!("daemon_start=spawned path={launched}");

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match channel(endpoint).await {
            Ok(_) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    bail!(
                        "daemon did not become reachable at {endpoint} after start attempt: {error}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn spawn_daemon_process() -> Result<String> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("BOUNDLESS_DAEMON_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            candidates.push(trimmed.to_string());
        }
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(parent) = current_exe.parent()
    {
        #[cfg(windows)]
        {
            candidates.push(parent.join("boundlessd.exe").display().to_string());
        }
        #[cfg(not(windows))]
        {
            candidates.push(parent.join("boundlessd").display().to_string());
        }
    }

    candidates.push("boundlessd".to_string());
    #[cfg(windows)]
    candidates.push("boundlessd.exe".to_string());

    candidates.sort();
    candidates.dedup();

    let mut errors = Vec::new();
    for candidate in candidates {
        let mut command = ProcessCommand::new(&candidate);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        match command.spawn() {
            Ok(_) => return Ok(candidate),
            Err(error) => errors.push(format!("{candidate}: {error}")),
        }
    }

    bail!(
        "failed to start boundlessd; candidates attempted: {}",
        errors.join("; ")
    )
}

pub(super) async fn daemon_status(endpoint: &str) -> Result<()> {
    let mut client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = client.get_status(StatusRequest {}).await?.into_inner();
    println!(
        "running={} machine_id={} peers={} protocol={} api_transport={} api_bind={} api_pipe_name={} input_locked={} input_lock_supported={} active_capture_target={}",
        status.running,
        status.machine_id,
        status.peer_count,
        status.protocol_version,
        status.api_transport,
        status.api_bind,
        status.api_pipe_name,
        status.input_locked,
        status.input_lock_supported,
        if status.capture_target_peer_id.is_empty() {
            "none"
        } else {
            status.capture_target_peer_id.as_str()
        }
    );
    Ok(())
}

pub(super) async fn pair_create_code(endpoint: &str, ttl: u32) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .create_code(PairCreateCodeRequest { ttl_seconds: ttl })
        .await?
        .into_inner();

    println!("code={} expires_at={}", response.code, response.expires_at);
    Ok(())
}

pub(super) async fn pair_discover(endpoint: &str) -> Result<()> {
    let discovered = list_discovered_peer_records(endpoint).await?;
    if discovered.is_empty() {
        println!("no discovered peers (mDNS may still be warming up)");
        return Ok(());
    }

    for (index, peer) in discovered.iter().enumerate() {
        let pairing_port = host_and_pairing_port_from_discovery_endpoint(&peer.endpoint)
            .map(|(_, port)| port)
            .unwrap_or(15200);
        println!(
            "[{}] name={} endpoint={} machine_id={} pairing_port={}",
            index + 1,
            peer.display_name,
            peer.endpoint,
            short_machine_id(&peer.machine_id),
            pairing_port
        );
    }
    Ok(())
}

pub(super) struct PairRequestArgs {
    pub(super) selector: String,
    pub(super) request_id: Option<String>,
    pub(super) host_override: Option<String>,
    pub(super) port_override: Option<u16>,
    pub(super) code: Option<String>,
    pub(super) alias: Option<String>,
    pub(super) timeout_seconds: u64,
}

pub(super) async fn pair_request(endpoint: &str, args: PairRequestArgs) -> Result<()> {
    let PairRequestArgs {
        selector,
        request_id,
        host_override,
        port_override,
        code,
        alias,
        timeout_seconds,
    } = args;
    let (host, pairing_port, default_alias, selector_hint, target_label, target_endpoint) =
        if let Some(host_override) = host_override {
            let host = host_override.trim().to_string();
            if host.is_empty() {
                bail!("--host must not be empty");
            }
            let pairing_port = port_override.unwrap_or(15200);
            (
                host.clone(),
                pairing_port,
                None,
                selector.clone(),
                host.clone(),
                format_host_port(&host, pairing_port),
            )
        } else {
            let discovered = list_discovered_peer_records(endpoint).await?;
            if discovered.is_empty() {
                if request_id.is_some() {
                    bail!(
                        "no discovered peers available for selector `{selector}`; retry with `--host <target-host-or-ip> --port <pairing-port>`"
                    );
                }
                bail!(
                    "no discovered peers available; try `pair nearby-join <code> --host <host> --port <port>`"
                );
            }
            let selected = resolve_discovered_peer_record(&discovered, &selector)?;
            let (host, pairing_port) = host_and_pairing_port_from_discovery_endpoint(
                &selected.endpoint,
            )
            .with_context(|| format!("invalid discovered endpoint {}", selected.endpoint))?;
            (
                host,
                pairing_port,
                Some(selected.display_name.clone()),
                selected.machine_id.clone(),
                selected.display_name.clone(),
                selected.endpoint.clone(),
            )
        };

    let alias = alias.or(default_alias);
    println!(
        "pair_request target={} endpoint={} pairing_port={} machine_id={}",
        target_label,
        target_endpoint,
        pairing_port,
        short_machine_id(&selector_hint),
    );

    if let Some(request_id) = request_id {
        let code = if let Some(value) = code {
            value
        } else {
            prompt_pairing_code()?
        };
        if code.trim().is_empty() {
            bail!("pairing code must not be empty");
        }
        return pair_nearby_submit_code(
            endpoint,
            request_id,
            code,
            host,
            pairing_port,
            timeout_seconds,
            alias,
        )
        .await;
    }

    if let Some(code) = code {
        if code.trim().is_empty() {
            bail!("pairing code must not be empty");
        }
        return pair_nearby_join(endpoint, code, host, pairing_port, timeout_seconds, alias).await;
    }

    match pair_nearby_request_code(endpoint, host.clone(), pairing_port).await? {
        NearbyRequestCodeStart::CodeRequired {
            request_id,
            expires_at,
        } => {
            println!(
                "pair_request_code_started=true request_id={} expires_at={}",
                request_id, expires_at
            );
            println!("enter code shown on target machine, then submit:");
            println!(
                "  boundlessctl pair request {} --request-id {} --code <6-digit-code> --host {} --port {}",
                selector_hint, request_id, host, pairing_port
            );
            Ok(())
        }
        NearbyRequestCodeStart::Unsupported { reason } => {
            bail!("target does not support the canonical guided pairing request flow ({reason})");
        }
    }
}

pub(super) async fn setup_wizard(endpoint: &str, start_daemon: bool) -> Result<()> {
    ensure_daemon_available(endpoint, start_daemon).await?;

    println!("Boundless setup wizard");
    println!("This flow pairs this PC with one peer and optionally sets orientation.");

    let existing_peers = list_peer_records(endpoint).await?;
    if !existing_peers.is_empty() {
        println!(
            "note: {} peer(s) already configured; setup will add/update one peer only",
            existing_peers.len()
        );
    }

    let discovered = list_discovered_peer_records(endpoint).await?;
    let (host, pairing_port, default_alias) = if discovered.is_empty() {
        println!("No discovered peers yet. Falling back to manual host entry.");
        let host = prompt_required("Peer host/IP")?;
        let port = prompt_u16_with_default("Peer nearby pairing port", 15200)?;
        (host, port, None)
    } else {
        println!("Discovered peers:");
        for (index, peer) in discovered.iter().enumerate() {
            println!(
                "  [{}] {} endpoint={} machine_id={}",
                index + 1,
                peer.display_name,
                peer.endpoint,
                short_machine_id(&peer.machine_id)
            );
        }
        println!("Type an index/machine_id/display-name prefix or `manual`.");
        let selector = prompt_required("Peer selector")?;
        if selector.eq_ignore_ascii_case("manual") {
            let host = prompt_required("Peer host/IP")?;
            let port = prompt_u16_with_default("Peer nearby pairing port", 15200)?;
            (host, port, None)
        } else {
            let selected = resolve_discovered_peer_record(&discovered, &selector)?;
            let (host, pairing_port) = host_and_pairing_port_from_discovery_endpoint(
                &selected.endpoint,
            )
            .with_context(|| format!("invalid discovered endpoint {}", selected.endpoint))?;
            (host, pairing_port, Some(selected.display_name.clone()))
        }
    };

    println!("On the peer PC, run `boundlessctl pair create-code --ttl 120` and copy the code.");
    let code = prompt_pairing_code()?;
    if code.trim().is_empty() {
        bail!("pairing code must not be empty");
    }

    let alias = prompt_optional_with_default("Alias for this peer", default_alias.as_deref())?;
    pair_nearby_join(endpoint, code, host, pairing_port, 120, alias.clone()).await?;

    let updated_peers = list_peer_records(endpoint).await?;
    let new_peer = find_new_peer_record(&existing_peers, &updated_peers).or_else(|| {
        alias.as_deref().and_then(|candidate_alias| {
            updated_peers
                .iter()
                .find(|peer| peer.display_name.eq_ignore_ascii_case(candidate_alias))
                .cloned()
        })
    });

    if existing_peers.is_empty() {
        if let Some(peer) = new_peer {
            println!(
                "Where is `{}` relative to this PC? [left/right/up/down/skip]",
                peer.display_name
            );
            let side = prompt_optional("Orientation")?;
            if let Some(side) = side {
                let normalized = side.to_ascii_lowercase();
                match normalized.as_str() {
                    "left" | "l" => {
                        layout_orient(endpoint, Some(peer.peer_id), None, None, None).await?
                    }
                    "right" | "r" => {
                        layout_orient(endpoint, None, Some(peer.peer_id), None, None).await?
                    }
                    "up" | "u" | "top" => {
                        layout_orient(endpoint, None, None, Some(peer.peer_id), None).await?
                    }
                    "down" | "d" | "bottom" => {
                        layout_orient(endpoint, None, None, None, Some(peer.peer_id)).await?
                    }
                    "skip" | "s" | "" => {
                        println!("layout unchanged; run `boundlessctl layout wizard` later");
                    }
                    _ => {
                        println!(
                            "unrecognized orientation `{side}`; layout unchanged (run `boundlessctl layout wizard`)"
                        );
                    }
                }
            }
        }
    } else {
        println!("layout was not auto-updated because peers already existed");
        println!("run `boundlessctl layout wizard` to adjust orientation");
    }

    println!("Setup complete.");
    println!("Next steps:");
    println!(
        "  - run `boundlessctl pair pending` on remote PC to approve pending requests, if needed"
    );
    println!("  - run `boundlessctl layout preview` to verify orientation");
    Ok(())
}

pub(super) async fn pair_join(
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

pub(super) async fn pair_nearby_join(
    endpoint: &str,
    code: String,
    host: String,
    port: u16,
    timeout_seconds: u64,
    alias: Option<String>,
) -> Result<()> {
    let peer_machine_id =
        pair_nearby_join_inner(endpoint, code, host, port, timeout_seconds, alias).await?;
    println!("accepted=true peer_machine_id={peer_machine_id} message=nearby pairing complete");
    Ok(())
}

enum NearbyRequestCodeStart {
    CodeRequired {
        request_id: String,
        expires_at: String,
    },
    Unsupported {
        reason: String,
    },
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
            expires_at,
            ..
        } => Ok(NearbyRequestCodeStart::CodeRequired {
            request_id,
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
    host: String,
    port: u16,
    timeout_seconds: u64,
    alias: Option<String>,
) -> Result<()> {
    let target = format_host_port(&host, port);
    let response = send_nearby_pairing_request(
        &target,
        NearbyJoinWireRequest::NearbySubmitCode {
            request_id: request_id.clone(),
            code,
            requester_alias: None,
        },
    )
    .await?;
    let responder_bundle =
        wait_for_nearby_pairing_approval(&target, response, timeout_seconds, &request_id).await?;
    let peer_machine_id =
        import_nearby_responder_bundle(endpoint, responder_bundle, &host, alias).await?;
    println!("accepted=true peer_machine_id={peer_machine_id} message=nearby pairing complete");
    Ok(())
}

async fn pair_nearby_join_inner(
    endpoint: &str,
    code: String,
    host: String,
    port: u16,
    timeout_seconds: u64,
    alias: Option<String>,
) -> Result<String> {
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
    let initial_response = send_nearby_pairing_request(
        &target,
        NearbyJoinWireRequest::NearbyJoin {
            code,
            requester_bundle,
            requester_alias: None,
        },
    )
    .await?;
    let responder_bundle =
        wait_for_nearby_pairing_approval(&target, initial_response, timeout_seconds, "").await?;
    import_nearby_responder_bundle(endpoint, responder_bundle, &host, alias).await
}

async fn wait_for_nearby_pairing_approval(
    target: &str,
    initial_response: NearbyJoinWireResponse,
    timeout_seconds: u64,
    expected_request_id: &str,
) -> Result<StoredTrustBundle> {
    match initial_response {
        NearbyJoinWireResponse::Approved {
            request_id,
            responder_bundle,
            ..
        } => {
            if !expected_request_id.is_empty() && !request_id.eq(expected_request_id) {
                bail!("nearby pairing request id mismatch");
            }
            Ok(responder_bundle)
        }
        NearbyJoinWireResponse::Pending {
            request_id,
            message,
        } => {
            println!("pending=true request_id={} message={}", request_id, message);
            if !expected_request_id.is_empty() && !request_id.eq(expected_request_id) {
                bail!("nearby pairing request id mismatch");
            }

            let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds.max(5));
            let mut poll_count = 0_u64;
            loop {
                if std::time::Instant::now() >= deadline {
                    bail!("timed out waiting for nearby pairing approval request_id={request_id}");
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
                poll_count += 1;
                if poll_count.is_multiple_of(5) {
                    println!(
                        "pending=true request_id={} waited={}s",
                        request_id, poll_count
                    );
                }
                let status_response = send_nearby_pairing_request(
                    target,
                    NearbyJoinWireRequest::CheckNearbyJoin {
                        request_id: request_id.clone(),
                    },
                )
                .await?;
                match status_response {
                    NearbyJoinWireResponse::Pending { .. } => continue,
                    NearbyJoinWireResponse::Approved {
                        request_id: approved_request_id,
                        responder_bundle,
                        ..
                    } => {
                        if !request_id.eq(&approved_request_id) {
                            bail!("nearby pairing request id mismatch");
                        }
                        return Ok(responder_bundle);
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
                }
            }
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
    }
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

pub(super) async fn pair_pending(endpoint: &str) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .list_nearby_pairing_requests(Empty {})
        .await?
        .into_inner();

    if response.requests.is_empty() {
        println!("no pending nearby pairing requests");
        return Ok(());
    }

    for request in response.requests {
        let requires_code = request.requires_verification_code;
        let has_visible_code = requires_code && !request.verification_code.trim().is_empty();
        println!(
            "request_id={} requester_machine_id={} requester_display_name={} created_at={} flow={} verification_code={} verification_expires_at={}",
            request.request_id,
            request.requester_machine_id,
            request.requester_display_name,
            request.created_at,
            if requires_code {
                "code_confirmation"
            } else {
                "manual_approval"
            },
            if has_visible_code {
                request.verification_code.as_str()
            } else if requires_code {
                "(hidden)"
            } else {
                "-"
            },
            if has_visible_code {
                request.verification_expires_at.as_str()
            } else if requires_code {
                "(hidden)"
            } else {
                "-"
            }
        );
    }
    Ok(())
}

pub(super) async fn pair_approve(
    endpoint: &str,
    request_id: String,
    alias: Option<String>,
) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .approve_nearby_pairing_request(NearbyPairingDecisionRequest {
            request_id,
            alias: alias.unwrap_or_default(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn pair_reject(endpoint: &str, request_id: String) -> Result<()> {
    let mut client = PairingServiceClient::new(channel(endpoint).await?);
    let response = client
        .reject_nearby_pairing_request(NearbyPairingDecisionRequest {
            request_id,
            alias: String::new(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn pair_export_trust(endpoint: &str, output: Option<String>) -> Result<()> {
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

pub(super) async fn pair_import_trust(
    endpoint: &str,
    input: String,
    alias: Option<String>,
) -> Result<()> {
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

pub(super) async fn peer_list(endpoint: &str) -> Result<()> {
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

pub(super) async fn peer_remove(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client
        .remove_peer(RemovePeerRequest { peer_id })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn layout_show(endpoint: &str) -> Result<()> {
    let mut client = TopologyServiceClient::new(channel(endpoint).await?);
    let response = client.layout_show(Empty {}).await?.into_inner();
    println!("{}", response.matrix_spec);
    Ok(())
}

pub(super) async fn layout_set(endpoint: &str, matrix: String) -> Result<()> {
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

pub(super) async fn layout_preview(endpoint: &str) -> Result<()> {
    let matrix = fetch_layout_spec(endpoint).await?;
    let peers = list_peer_records(endpoint).await?;
    let local_tokens = fetch_local_layout_tokens(endpoint).await?;
    let grid = parse_layout_matrix(&matrix);

    println!("layout_matrix={matrix}");
    if grid.is_empty() {
        println!("layout grid is empty");
        return Ok(());
    }

    for row in grid {
        let labels = row
            .into_iter()
            .map(|token| preview_label_for_token(&token, &peers, &local_tokens))
            .collect::<Vec<_>>();
        println!("  {}", labels.join(" | "));
    }

    println!("tip: run `boundlessctl layout orient --left <peer> --right <peer>` for quick edits");
    Ok(())
}

pub(super) async fn layout_orient(
    endpoint: &str,
    left: Option<String>,
    right: Option<String>,
    up: Option<String>,
    down: Option<String>,
) -> Result<()> {
    let peers = list_peer_records(endpoint).await?;
    if peers.is_empty() {
        bail!("no peers configured");
    }

    let existing_matrix = fetch_layout_spec(endpoint).await?;
    let local_tokens = fetch_local_layout_tokens(endpoint).await?;
    let existing = extract_orientation_slots(&existing_matrix, &peers, &local_tokens)?;
    ensure_orientation_safe_to_edit(&existing_matrix, &existing, &peers, &local_tokens)?;

    let left_peer = match left {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.left,
    };
    let right_peer = match right {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.right,
    };
    let up_peer = match up {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.up,
    };
    let down_peer = match down {
        Some(selector) => resolve_peer_selector_opt(&peers, Some(selector.as_str()))?,
        None => existing.down,
    };

    if left_peer.is_none() && right_peer.is_none() && up_peer.is_none() && down_peer.is_none() {
        bail!("no orientation peers selected; provide at least one side");
    }

    let mut unique = std::collections::HashSet::<String>::new();
    for peer_id in [
        left_peer.as_deref(),
        right_peer.as_deref(),
        up_peer.as_deref(),
        down_peer.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !unique.insert(peer_id.to_string()) {
            bail!("each peer can only appear once across left/right/up/down");
        }
    }

    let matrix = build_orientation_matrix(
        left_peer.as_deref(),
        right_peer.as_deref(),
        up_peer.as_deref(),
        down_peer.as_deref(),
    );
    layout_set(endpoint, matrix.clone()).await?;
    println!("layout_matrix={matrix}");
    layout_preview(endpoint).await
}

pub(super) async fn layout_wizard(endpoint: &str) -> Result<()> {
    let peers = list_peer_records(endpoint).await?;
    if peers.is_empty() {
        bail!("no peers configured");
    }

    println!("Layout wizard");
    println!("Peers:");
    for (index, peer) in peers.iter().enumerate() {
        println!(
            "  [{}] name={} peer_id={} connected={}",
            index + 1,
            peer.display_name,
            short_machine_id(&peer.peer_id),
            peer.connected
        );
    }
    println!("Enter index/peer_id/name prefix for each side, or leave blank.");

    let left = prompt_optional("Left peer")?;
    let right = prompt_optional("Right peer")?;
    let up = prompt_optional("Up peer")?;
    let down = prompt_optional("Down peer")?;

    layout_orient(endpoint, left, right, up, down).await
}

pub(super) async fn feature_list(endpoint: &str) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel(endpoint).await?);
    let response = client.list_features(Empty {}).await?.into_inner();

    let mut features = response.features.into_iter().collect::<Vec<_>>();
    features.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, enabled) in features {
        println!("{name}={enabled}");
    }

    Ok(())
}

pub(super) async fn feature_set(endpoint: &str, name: String, value: ToggleValue) -> Result<()> {
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

pub(super) async fn hotkey_set(endpoint: &str, action: String, combo: String) -> Result<()> {
    let mut client = FeatureServiceClient::new(channel(endpoint).await?);
    let response = client
        .set_hotkey(HotkeySetRequest { action, combo })
        .await?
        .into_inner();
    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_text(
    endpoint: &str,
    peer_id: String,
    text: String,
) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_clipboard_text(SendClipboardTextRequest { peer_id, text })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_image(
    endpoint: &str,
    peer_id: String,
    path: String,
) -> Result<()> {
    let image_bmp = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    validate_bmp_payload(&image_bmp).with_context(|| format!("invalid BMP payload at {path}"))?;

    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_clipboard_image(SendClipboardImageRequest { peer_id, image_bmp })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn transport_send_file(
    endpoint: &str,
    peer_id: String,
    path: String,
) -> Result<()> {
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

pub(super) async fn transport_events(endpoint: &str, limit: usize) -> Result<()> {
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

pub(super) async fn input_owner(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client.get_input_owner(Empty {}).await?.into_inner();
    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

pub(super) async fn input_capture_target(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .get_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_capture_start(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .set_input_capture_target(InputCaptureTargetRequest { peer_id })
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_capture_stop(endpoint: &str) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .clear_input_capture_target(Empty {})
        .await?
        .into_inner();
    let target = if response.peer_id.is_empty() {
        "none".to_string()
    } else {
        response.peer_id
    };

    println!(
        "ok={} target={} message={}",
        response.ok, target, response.message
    );
    Ok(())
}

pub(super) async fn input_send_move(
    endpoint: &str,
    peer_id: String,
    dx: i32,
    dy: i32,
) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_input_move(SendInputMoveRequest { peer_id, dx, dy })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn input_send_key(
    endpoint: &str,
    peer_id: String,
    scan_code: u16,
    state: InputKeyState,
) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .send_input_key(SendInputKeyRequest {
            peer_id,
            scan_code: scan_code as u32,
            key_down: state.is_down(),
        })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn input_claim(endpoint: &str, peer_id: String, force: bool) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .claim_input_owner(InputOwnerRequest { peer_id, force })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

pub(super) async fn input_release(endpoint: &str, peer_id: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .release_input_owner(InputOwnerRequest {
            peer_id,
            force: false,
        })
        .await?
        .into_inner();

    let owner = if response.owner_peer_id.is_empty() {
        "none".to_string()
    } else {
        response.owner_peer_id
    };

    println!(
        "ok={} owner={} message={}",
        response.ok, owner, response.message
    );
    Ok(())
}

pub(super) async fn diagnostics_dump(endpoint: &str, output: Option<String>) -> Result<()> {
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

pub(super) async fn diagnostics_run_action(endpoint: &str, action: String) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .trigger_hotkey_action(HotkeyTriggerRequest { action })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

pub(super) async fn safe_reset(endpoint: &str, network_only: bool, all: bool) -> Result<()> {
    let mut client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let response = client
        .safe_reset(SafeResetRequest { network_only, all })
        .await?
        .into_inner();

    println!("ok={} message={}", response.ok, response.message);
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct UiSnapshot {
    generated_at: String,
    daemon_online: bool,
    machine_id: String,
    layout_matrix: String,
    discovered_peers: Vec<UiDiscoveredPeer>,
    paired_peers: Vec<UiPairedPeer>,
    pending_requests: Vec<UiPendingRequest>,
}

#[derive(Debug, Clone, Serialize)]
struct UiDiscoveredPeer {
    machine_id: String,
    display_name: String,
    endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
struct UiPairedPeer {
    peer_id: String,
    display_name: String,
    address: String,
    connected: bool,
}

#[derive(Debug, Clone, Serialize)]
struct UiPendingRequest {
    request_id: String,
    requester_machine_id: String,
    requester_display_name: String,
    created_at: String,
    verification_code: String,
    verification_expires_at: String,
    requires_verification_code: bool,
}

pub(super) async fn ui_snapshot(endpoint: &str, start_daemon: bool) -> Result<()> {
    if start_daemon {
        ensure_daemon_available(endpoint, true).await?;
    }

    let mut daemon_client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = daemon_client
        .get_status(StatusRequest {})
        .await?
        .into_inner();

    let mut topology_client = TopologyServiceClient::new(channel(endpoint).await?);
    let paired_peers = topology_client
        .list_peers(Empty {})
        .await?
        .into_inner()
        .peers
        .into_iter()
        .map(|peer| UiPairedPeer {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            address: peer.address,
            connected: peer.connected,
        })
        .collect::<Vec<_>>();

    let mut diagnostics_client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let discovery = diagnostics_client
        .list_discovery_peers(Empty {})
        .await?
        .into_inner();
    let discovered_peers = discovery
        .peers
        .into_iter()
        .map(|peer| UiDiscoveredPeer {
            machine_id: peer.machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint,
        })
        .collect::<Vec<_>>();

    let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
    let pending_requests = pairing_client
        .list_nearby_pairing_requests(Empty {})
        .await?
        .into_inner()
        .requests
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

    let layout_matrix = fetch_layout_spec(endpoint).await?;
    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let snapshot = UiSnapshot {
        generated_at,
        daemon_online: status.running,
        machine_id: status.machine_id,
        layout_matrix,
        discovered_peers,
        paired_peers,
        pending_requests,
    };

    println!(
        "{}",
        serde_json::to_string(&snapshot).context("serialize ui snapshot")?
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct DiscoveredPeerRecord {
    machine_id: String,
    display_name: String,
    endpoint: String,
}

#[derive(Debug, Clone)]
struct PeerRecord {
    peer_id: String,
    display_name: String,
    connected: bool,
}

#[derive(Debug, Clone)]
struct LocalLayoutTokens {
    machine_id: String,
    display_name: String,
}

async fn list_discovered_peer_records(endpoint: &str) -> Result<Vec<DiscoveredPeerRecord>> {
    let mut diagnostics_client = DiagnosticsServiceClient::new(channel(endpoint).await?);
    let mut peers = diagnostics_client
        .list_discovery_peers(Empty {})
        .await?
        .into_inner()
        .peers
        .into_iter()
        .map(|peer| DiscoveredPeerRecord {
            machine_id: peer.machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint,
        })
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.machine_id.cmp(&b.machine_id))
    });
    Ok(peers)
}

async fn list_peer_records(endpoint: &str) -> Result<Vec<PeerRecord>> {
    let mut topology_client = TopologyServiceClient::new(channel(endpoint).await?);
    let mut peers = topology_client
        .list_peers(Empty {})
        .await?
        .into_inner()
        .peers
        .into_iter()
        .map(|peer| PeerRecord {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            connected: peer.connected,
        })
        .collect::<Vec<_>>();
    peers.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| {
                a.display_name
                    .to_ascii_lowercase()
                    .cmp(&b.display_name.to_ascii_lowercase())
            })
            .then_with(|| a.peer_id.cmp(&b.peer_id))
    });
    Ok(peers)
}

async fn fetch_layout_spec(endpoint: &str) -> Result<String> {
    let mut topology_client = TopologyServiceClient::new(channel(endpoint).await?);
    let layout = topology_client.layout_show(Empty {}).await?.into_inner();
    Ok(layout.matrix_spec)
}

async fn fetch_local_layout_tokens(endpoint: &str) -> Result<LocalLayoutTokens> {
    let mut daemon_client = DaemonServiceClient::new(channel(endpoint).await?);
    let status = daemon_client
        .get_status(StatusRequest {})
        .await?
        .into_inner();

    let mut pairing_client = PairingServiceClient::new(channel(endpoint).await?);
    let bundle = pairing_client
        .export_trust_bundle(Empty {})
        .await?
        .into_inner();

    Ok(LocalLayoutTokens {
        machine_id: status.machine_id,
        display_name: bundle.display_name,
    })
}

fn resolve_discovered_peer_record<'a>(
    peers: &'a [DiscoveredPeerRecord],
    selector: &str,
) -> Result<&'a DiscoveredPeerRecord> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 {
            bail!("selector index must start at 1");
        }
        return peers
            .get(index - 1)
            .ok_or_else(|| anyhow::anyhow!("no discovered peer at index {index}"));
    }

    let normalized = selector.trim();
    if normalized.is_empty() {
        bail!("selector must not be empty");
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
        bail!("multiple discovered peers match `{selector}`; use an index");
    }
    Ok(matches[0])
}

fn resolve_peer_selector_opt(
    peers: &[PeerRecord],
    selector: Option<&str>,
) -> Result<Option<String>> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let peer = resolve_peer_selector(peers, trimmed)?;
    Ok(Some(peer.peer_id.clone()))
}

fn resolve_peer_selector<'a>(peers: &'a [PeerRecord], selector: &str) -> Result<&'a PeerRecord> {
    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 {
            bail!("peer index must start at 1");
        }
        return peers
            .get(index - 1)
            .ok_or_else(|| anyhow::anyhow!("no peer at index {index}"));
    }

    let selector_lower = selector.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(selector)
                || peer
                    .peer_id
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
                || peer.display_name.eq_ignore_ascii_case(selector)
                || peer
                    .display_name
                    .to_ascii_lowercase()
                    .starts_with(&selector_lower)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("no peer matching `{selector}`");
    }
    if matches.len() > 1 {
        bail!("multiple peers match `{selector}`; use index or full peer_id");
    }
    Ok(matches[0])
}

#[derive(Debug, Default)]
struct OrientationSlots {
    left: Option<String>,
    right: Option<String>,
    up: Option<String>,
    down: Option<String>,
}

fn extract_orientation_slots(
    matrix: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<OrientationSlots> {
    let grid = parse_layout_matrix(matrix);
    if grid.is_empty() {
        return Ok(OrientationSlots::default());
    }

    let mut local_cell: Option<(usize, usize)> = None;
    for (row_index, row) in grid.iter().enumerate() {
        for (column_index, token) in row.iter().enumerate() {
            if is_local_layout_token(token, local_tokens) {
                if local_cell.is_some() {
                    bail!("layout has multiple local cells; cannot safely orient");
                }
                local_cell = Some((row_index, column_index));
            }
        }
    }
    let Some((row, column)) = local_cell else {
        bail!("layout has no local cell; cannot safely orient");
    };

    let token_at = |row_index: usize, column_index: usize| -> Option<&str> {
        grid.get(row_index)
            .and_then(|tokens| tokens.get(column_index))
            .map(String::as_str)
    };

    let mut slots = OrientationSlots::default();
    for next_column in (0..column).rev() {
        let Some(token) = token_at(row, next_column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.left = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    let width = grid.get(row).map(|tokens| tokens.len()).unwrap_or(0);
    for next_column in (column + 1)..width {
        let Some(token) = token_at(row, next_column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.right = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    for next_row in (0..row).rev() {
        let Some(token) = token_at(next_row, column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.up = Some(peer_id);
                break;
            }
            None => continue,
        }
    }
    for next_row in (row + 1)..grid.len() {
        let Some(token) = token_at(next_row, column) else {
            continue;
        };
        match resolve_matrix_peer_token(token, peers, local_tokens)? {
            Some(peer_id) => {
                slots.down = Some(peer_id);
                break;
            }
            None => continue,
        }
    }

    Ok(slots)
}

fn ensure_orientation_safe_to_edit(
    matrix: &str,
    slots: &OrientationSlots,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<()> {
    let matrix_peer_ids = collect_matrix_peer_ids(matrix, peers, local_tokens)?;
    let slot_peer_ids = [
        slots.left.as_deref(),
        slots.right.as_deref(),
        slots.up.as_deref(),
        slots.down.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(ToString::to_string)
    .collect::<std::collections::HashSet<_>>();

    let hidden = matrix_peer_ids
        .difference(&slot_peer_ids)
        .cloned()
        .collect::<Vec<_>>();
    if !hidden.is_empty() {
        bail!(
            "current layout contains peers beyond immediate left/right/up/down; `layout orient` would drop them. Use `layout set` for complex topologies."
        );
    }
    Ok(())
}

fn collect_matrix_peer_ids(
    matrix: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<std::collections::HashSet<String>> {
    let mut ids = std::collections::HashSet::<String>::new();
    for row in parse_layout_matrix(matrix) {
        for token in row {
            if let Some(peer_id) = resolve_matrix_peer_token(&token, peers, local_tokens)? {
                ids.insert(peer_id);
            }
        }
    }
    Ok(ids)
}

fn is_local_layout_token(token: &str, local_tokens: &LocalLayoutTokens) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }
    matches!(
        token.to_ascii_lowercase().as_str(),
        "self" | "local" | "this" | "me"
    ) || token.eq_ignore_ascii_case(&local_tokens.machine_id)
        || token.eq_ignore_ascii_case(&local_tokens.display_name)
}

fn resolve_matrix_peer_token(
    token: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> Result<Option<String>> {
    let trimmed = token.trim();
    if trimmed.is_empty() || is_local_layout_token(trimmed, local_tokens) {
        return Ok(None);
    }

    let token_lower = trimmed.to_ascii_lowercase();
    let matches = peers
        .iter()
        .filter(|peer| {
            peer.peer_id.eq_ignore_ascii_case(trimmed)
                || peer.peer_id.to_ascii_lowercase().starts_with(&token_lower)
                || peer.display_name.eq_ignore_ascii_case(trimmed)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("layout token `{trimmed}` does not resolve to a known peer");
    }
    if matches.len() > 1 {
        bail!("layout token `{trimmed}` is ambiguous across peers");
    }
    Ok(Some(matches[0].peer_id.clone()))
}

pub(super) fn host_and_pairing_port_from_discovery_endpoint(
    endpoint: &str,
) -> Result<(String, u16)> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        bail!("discovery endpoint is empty");
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return Ok((socket.ip().to_string(), nearby_pairing_port(socket.port())));
    }

    if let Some(host) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(host, _)| host.to_string())
    {
        let port = extract_port_from_network_address(trimmed)?;
        return Ok((host, nearby_pairing_port(port)));
    }

    if let Some((host, _)) = trimmed.rsplit_once(':') {
        let host = host.trim();
        if host.is_empty() {
            bail!("discovery endpoint is missing host");
        }
        let port = extract_port_from_network_address(trimmed)?;
        return Ok((host.to_string(), nearby_pairing_port(port)));
    }

    bail!("discovery endpoint must include host and port")
}

fn build_orientation_matrix(
    left: Option<&str>,
    right: Option<&str>,
    up: Option<&str>,
    down: Option<&str>,
) -> String {
    let center = format!("{},{},{}", left.unwrap_or(""), "self", right.unwrap_or(""));
    let mut rows = Vec::<String>::new();
    if let Some(up) = up {
        rows.push(format!(",{},", up));
    }
    rows.push(center);
    if let Some(down) = down {
        rows.push(format!(",{},", down));
    }
    rows.join(";")
}

fn parse_layout_matrix(matrix: &str) -> Vec<Vec<String>> {
    matrix
        .split(';')
        .map(|row| {
            row.split(',')
                .map(|token| token.trim().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn preview_label_for_token(
    token: &str,
    peers: &[PeerRecord],
    local_tokens: &LocalLayoutTokens,
) -> String {
    if token.trim().is_empty() {
        return ".".to_string();
    }
    if is_local_layout_token(token, local_tokens) {
        return "THIS-PC".to_string();
    }

    if let Some(peer) = peers.iter().find(|peer| {
        peer.peer_id.eq_ignore_ascii_case(token) || peer.display_name.eq_ignore_ascii_case(token)
    }) {
        return format!(
            "{}{}",
            peer.display_name,
            if peer.connected { "" } else { " (offline)" }
        );
    }

    token.to_string()
}

fn find_new_peer_record(before: &[PeerRecord], after: &[PeerRecord]) -> Option<PeerRecord> {
    let before_ids = before
        .iter()
        .map(|peer| peer.peer_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    after
        .iter()
        .find(|peer| !before_ids.contains(peer.peer_id.as_str()))
        .cloned()
}

fn prompt_required(label: &str) -> Result<String> {
    loop {
        print!("{label}: ");
        io::stdout().flush().context("flush stdout")?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label}"))?;
        let trimmed = line.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
        println!("{label} is required");
    }
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    print!("{label}: ");
    io::stdout().flush().context("flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .with_context(|| format!("read {label}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn prompt_optional_with_default(label: &str, default: Option<&str>) -> Result<Option<String>> {
    if let Some(default) = default {
        print!("{label} [{default}]: ");
    } else {
        print!("{label}: ");
    }
    io::stdout().flush().context("flush stdout")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .with_context(|| format!("read {label}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default.map(|value| value.to_string()));
    }
    Ok(Some(trimmed.to_string()))
}

fn prompt_u16_with_default(label: &str, default: u16) -> Result<u16> {
    loop {
        print!("{label} [{default}]: ");
        io::stdout().flush().context("flush stdout")?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        let value = trimmed
            .parse::<u16>()
            .with_context(|| format!("{label} must be a valid port in range 0..=65535"))?;
        if value == 0 {
            println!("{label} must be greater than 0");
            continue;
        }
        return Ok(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_tokens() -> LocalLayoutTokens {
        LocalLayoutTokens {
            machine_id: "local-machine".to_string(),
            display_name: "local-device".to_string(),
        }
    }

    #[test]
    fn build_orientation_matrix_builds_cross_layout() {
        let matrix = build_orientation_matrix(
            Some("peer-left"),
            Some("peer-right"),
            Some("peer-up"),
            Some("peer-down"),
        );
        assert_eq!(matrix, ",peer-up,;peer-left,self,peer-right;,peer-down,");
    }

    #[test]
    fn resolve_discovered_peer_record_supports_display_name_prefix() {
        let peers = vec![
            DiscoveredPeerRecord {
                machine_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                display_name: "office-desktop".to_string(),
                endpoint: "10.0.0.10:15100".to_string(),
            },
            DiscoveredPeerRecord {
                machine_id: "11111111-2222-3333-4444-555555555555".to_string(),
                display_name: "laptop".to_string(),
                endpoint: "10.0.0.11:15100".to_string(),
            },
        ];

        let selected = resolve_discovered_peer_record(&peers, "office").expect("resolve prefix");
        assert_eq!(selected.display_name, "office-desktop");
    }

    #[test]
    fn host_and_pairing_port_parses_hostname_endpoint() {
        let (host, port) = host_and_pairing_port_from_discovery_endpoint("DESKTOP-ABC:15100")
            .expect("parse endpoint");
        assert_eq!(host, "DESKTOP-ABC");
        assert_eq!(port, 15200);
    }

    #[test]
    fn ensure_orientation_safe_to_edit_rejects_hidden_peer_chain() {
        let peers = vec![
            PeerRecord {
                peer_id: "left-a".to_string(),
                display_name: "left-a".to_string(),
                connected: true,
            },
            PeerRecord {
                peer_id: "left-b".to_string(),
                display_name: "left-b".to_string(),
                connected: true,
            },
        ];
        let local = local_tokens();
        let slots =
            extract_orientation_slots("left-a,left-b,self", &peers, &local).expect("parse slots");
        let err = ensure_orientation_safe_to_edit("left-a,left-b,self", &slots, &peers, &local)
            .expect_err("must reject hidden peer");
        assert!(err.to_string().contains("would drop"));
    }

    #[test]
    fn extract_orientation_slots_recognizes_machine_id_local_token() {
        let peers = vec![PeerRecord {
            peer_id: "peer-right".to_string(),
            display_name: "peer-right".to_string(),
            connected: true,
        }];
        let local = local_tokens();
        let slots = extract_orientation_slots("local-machine,peer-right", &peers, &local)
            .expect("parse slots");
        assert_eq!(slots.right.as_deref(), Some("peer-right"));
    }
}
