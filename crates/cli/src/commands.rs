use super::*;

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

    let mut responder_bundle = match initial_response {
        NearbyJoinWireResponse::Approved {
            responder_bundle, ..
        } => responder_bundle,
        NearbyJoinWireResponse::Pending {
            request_id,
            message,
        } => {
            println!("pending=true request_id={} message={}", request_id, message);
            let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds.max(5));
            loop {
                if std::time::Instant::now() >= deadline {
                    bail!("timed out waiting for nearby pairing approval request_id={request_id}");
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
                let status_response = send_nearby_pairing_request(
                    &target,
                    NearbyJoinWireRequest::CheckNearbyJoin {
                        request_id: request_id.clone(),
                    },
                )
                .await?;
                match status_response {
                    NearbyJoinWireResponse::Pending { .. } => continue,
                    NearbyJoinWireResponse::Approved {
                        responder_bundle, ..
                    } => break responder_bundle,
                    NearbyJoinWireResponse::Rejected { message, .. } => {
                        bail!("nearby pairing rejected: {message}");
                    }
                    NearbyJoinWireResponse::Error { message } => {
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
    };
    normalize_bundle_address_for_host(&mut responder_bundle, &host)?;

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

    println!(
        "accepted=true peer_machine_id={} message=nearby pairing complete",
        responder_bundle.machine_id
    );
    Ok(())
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
        println!(
            "request_id={} requester_machine_id={} requester_display_name={} created_at={}",
            request.request_id,
            request.requester_machine_id,
            request.requester_display_name,
            request.created_at
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
