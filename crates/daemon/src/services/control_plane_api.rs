use super::*;
use crate::pairing_wire::{self, NearbyRequestCodeStart};

#[derive(Clone)]
pub struct ControlPlaneApi(pub(super) AppState);

#[tonic::async_trait]
impl ControlPlaneService for ControlPlaneApi {
    type WatchUiStream =
        tonic::codegen::tokio_stream::wrappers::ReceiverStream<Result<UiSnapshotReply, Status>>;

    async fn get_ui_snapshot(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<UiSnapshotReply>, Status> {
        Ok(Response::new(build_ui_snapshot(&self.0).await))
    }

    async fn get_console_snapshot(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<ConsoleSnapshotReply>, Status> {
        Ok(Response::new(build_console_snapshot(&self.0).await))
    }

    async fn get_status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusReply>, Status> {
        let snapshot = build_console_snapshot(&self.0).await;
        let status = snapshot
            .status
            .ok_or_else(|| Status::internal("console snapshot missing status payload"))?;
        Ok(Response::new(status))
    }

    async fn create_pairing_code(
        &self,
        request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        let ttl_seconds = request.into_inner().ttl_seconds.max(1) as u64;
        let (code, expires_at) = self.0.create_pairing_code(ttl_seconds).await;
        Ok(Response::new(PairCreateCodeReply {
            code,
            expires_at: expires_at.to_rfc3339(),
        }))
    }

    async fn join_with_pairing_code(
        &self,
        request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        let request = request.into_inner();
        let code = parse_required_field("code", &request.code)?;
        let host = parse_required_field("host", &request.host)?;
        let alias = parse_optional_alias(request.alias);

        let peer_id = self
            .0
            .join_peer(code, host, alias)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(PairJoinReply {
            accepted: true,
            peer_id,
            message: "paired".to_string(),
        }))
    }

    async fn watch_ui(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::WatchUiStream>, Status> {
        let state = self.0.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            if sender
                .send(Ok(build_ui_snapshot(&state).await))
                .await
                .is_err()
            {
                return;
            }

            loop {
                interval.tick().await;
                if sender
                    .send(Ok(build_ui_snapshot(&state).await))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        Ok(Response::new(
            tonic::codegen::tokio_stream::wrappers::ReceiverStream::new(receiver),
        ))
    }

    async fn layout_set(
        &self,
        request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let matrix = request.into_inner().matrix_spec;
        self.0
            .set_layout(matrix)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "Layout updated".to_string(),
        }))
    }

    async fn list_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<PeerListReply>, Status> {
        Ok(Response::new(PeerListReply {
            peers: build_peer_infos(&self.0).await,
        }))
    }

    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let peer_id = request.into_inner().peer_id;
        let removed = self
            .0
            .remove_peer(&peer_id)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: removed,
            message: if removed {
                format!("Removed peer {peer_id}")
            } else {
                format!("Peer {peer_id} not found")
            },
        }))
    }

    async fn layout_show(&self, _request: Request<Empty>) -> Result<Response<LayoutReply>, Status> {
        Ok(Response::new(LayoutReply {
            matrix_spec: self.0.layout().await,
        }))
    }

    async fn list_features(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<FeatureListReply>, Status> {
        Ok(Response::new(FeatureListReply {
            features: self.0.feature_map().await.into_iter().collect(),
        }))
    }

    async fn set_feature(
        &self,
        request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_feature(request.name.clone(), request.enabled)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("{}={}", request.name, request.enabled),
        }))
    }

    async fn set_hotkey(
        &self,
        request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_hotkey(request.action.clone(), request.combo.clone())
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("hotkey {}={}", request.action, request.combo),
        }))
    }

    async fn trigger_hotkey_action(
        &self,
        request: Request<HotkeyTriggerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let action_name = crate::hotkeys::trigger_action_for_diagnostics(&self.0, &request.action)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("hotkey action {action_name} triggered"),
        }))
    }

    async fn export_trust_bundle(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TrustBundleReply>, Status> {
        let bundle = self
            .0
            .export_trust_bundle()
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(TrustBundleReply {
            machine_id: bundle.machine_id,
            display_name: bundle.display_name,
            network_address: bundle.network_address,
            ca_cert_pem: bundle.ca_cert_pem,
        }))
    }

    async fn import_trust_bundle(
        &self,
        request: Request<ImportTrustBundleRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .import_trust_bundle(
                core_security::TrustBundle {
                    machine_id: request.machine_id,
                    display_name: request.display_name,
                    network_address: request.network_address,
                    ca_cert_pem: request.ca_cert_pem,
                },
                parse_optional_alias(request.alias),
            )
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "trust bundle imported".to_string(),
        }))
    }

    async fn dump_diagnostics(
        &self,
        request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        let output_path = parse_optional_alias(request.into_inner().output_path);
        let bundle_path = self
            .0
            .diagnostics_dump(output_path)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(DiagnosticsDumpReply { bundle_path }))
    }

    async fn safe_reset(
        &self,
        request: Request<SafeResetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .safe_reset(request.network_only, request.all)
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "reset complete".to_string(),
        }))
    }

    async fn send_clipboard_text(
        &self,
        request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_clipboard_text(&request.peer_id, request.text)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "clipboard payload queued".to_string(),
        }))
    }

    async fn send_clipboard_image(
        &self,
        request: Request<SendClipboardImageRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_clipboard_image(&request.peer_id, request.image_bmp)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "clipboard image payload queued".to_string(),
        }))
    }

    async fn send_file(
        &self,
        request: Request<SendFileRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_file_from_path(&request.peer_id, Path::new(&request.file_path))
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "file payload queued".to_string(),
        }))
    }

    async fn send_input_move(
        &self,
        request: Request<SendInputMoveRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_input_move(&request.peer_id, request.dx, request.dy)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "input move frame queued".to_string(),
        }))
    }

    async fn send_input_key(
        &self,
        request: Request<SendInputKeyRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let scan_code = u16::try_from(request.scan_code)
            .map_err(|_| Status::invalid_argument("scan_code must be in 0..=65535"))?;
        let key_state = if request.key_down {
            core_input::KeyState::Down
        } else {
            core_input::KeyState::Up
        };

        self.0
            .queue_input_key(&request.peer_id, scan_code, key_state)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "input key frame queued".to_string(),
        }))
    }

    async fn list_transport_events(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<TransportEventsReply>, Status> {
        let events = self
            .0
            .transport_events()
            .await
            .into_iter()
            .map(|event| TransportEvent {
                timestamp: event.timestamp.to_rfc3339(),
                direction: event.direction,
                kind: event.kind,
                peer_id: event.peer_id,
                detail: event.detail,
                size_bytes: event.size_bytes,
            })
            .collect();

        Ok(Response::new(TransportEventsReply { events }))
    }

    async fn get_input_owner(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: true,
            owner_peer_id: owner,
            message: "input owner fetched".to_string(),
        }))
    }

    async fn claim_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let acquired = self
            .0
            .claim_input_owner(&request.peer_id, request.force)
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: acquired,
            owner_peer_id: owner.clone(),
            message: if acquired {
                format!("input owner set to {owner}")
            } else {
                format!("input owner remains {owner}")
            },
        }))
    }

    async fn release_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        let request = request.into_inner();
        let released = self.0.release_input_owner(&request.peer_id).await;
        let owner = self.0.input_owner().await.unwrap_or_default();
        Ok(Response::new(InputOwnerReply {
            ok: released,
            owner_peer_id: owner,
            message: if released {
                "input owner released".to_string()
            } else {
                "peer did not hold input owner".to_string()
            },
        }))
    }

    async fn get_input_capture_target(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        let peer_id = self.0.input_capture_target().await.unwrap_or_default();
        Ok(Response::new(InputCaptureTargetReply {
            ok: true,
            peer_id,
            message: "input capture target fetched".to_string(),
        }))
    }

    async fn set_input_capture_target(
        &self,
        request: Request<InputCaptureTargetRequest>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        let request = request.into_inner();
        let target = self
            .0
            .set_input_capture_target(Some(&request.peer_id))
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        let peer_id = target.unwrap_or_default();

        Ok(Response::new(InputCaptureTargetReply {
            ok: true,
            peer_id: peer_id.clone(),
            message: if peer_id.is_empty() {
                "input capture target cleared".to_string()
            } else {
                format!("input capture target set to {peer_id}")
            },
        }))
    }

    async fn clear_input_capture_target(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        self.0.clear_input_capture_target().await;
        Ok(Response::new(InputCaptureTargetReply {
            ok: true,
            peer_id: String::new(),
            message: "input capture target cleared".to_string(),
        }))
    }

    async fn request_nearby_pairing_code(
        &self,
        request: Request<NearbyRequestCodeStartRequest>,
    ) -> Result<Response<NearbyRequestCodeStartReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;

        let response = pairing_wire::request_nearby_pairing_code(
            &self.0,
            &host,
            port,
            parse_optional_alias(request.alias),
        )
        .await
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

        let reply = match response {
            NearbyRequestCodeStart::CodeRequired {
                request_id,
                verification_nonce,
                expires_at,
            } => NearbyRequestCodeStartReply {
                code_required: true,
                request_id,
                verification_nonce,
                verification_expires_at: expires_at,
                unsupported: false,
                message: "enter code shown on target machine".to_string(),
            },
            NearbyRequestCodeStart::Unsupported { reason } => NearbyRequestCodeStartReply {
                code_required: false,
                request_id: String::new(),
                verification_nonce: String::new(),
                verification_expires_at: String::new(),
                unsupported: true,
                message: reason,
            },
        };

        Ok(Response::new(reply))
    }

    async fn submit_nearby_pairing_code(
        &self,
        request: Request<NearbySubmitCodeRequest>,
    ) -> Result<Response<NearbyPairingCompletionReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let request_id = parse_required_field("request_id", &request.request_id)?;
        let code = parse_required_field("code", &request.code)?;
        let verification_nonce =
            parse_required_field("verification_nonce", &request.verification_nonce)?;

        let peer_machine_id = pairing_wire::submit_nearby_pairing_code(
            &self.0,
            &host,
            port,
            request_id.clone(),
            code,
            verification_nonce,
            parse_optional_alias(request.alias),
        )
        .await
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(NearbyPairingCompletionReply {
            ok: true,
            message: "nearby pairing complete".to_string(),
            request_id,
            peer_machine_id,
        }))
    }

    async fn start_nearby_pairing_join(
        &self,
        request: Request<NearbyJoinStartRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let code = parse_required_field("code", &request.code)?;

        let result = pairing_wire::start_nearby_pairing_join(
            &self.0,
            &host,
            port,
            code,
            parse_optional_alias(request.alias),
        )
        .await
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(NearbyJoinStatusReply {
            request_id: result.request_id,
            status: result.status.as_str().to_string(),
            message: result.message,
            peer_machine_id: result.peer_machine_id.unwrap_or_default(),
        }))
    }

    async fn check_nearby_pairing_join(
        &self,
        request: Request<NearbyJoinStatusRequest>,
    ) -> Result<Response<NearbyJoinStatusReply>, Status> {
        let request = request.into_inner();
        let host = parse_host(&request.host)?;
        let port = parse_port(request.port)?;
        let request_id = parse_required_field("request_id", &request.request_id)?;

        let result = pairing_wire::check_nearby_pairing_join(
            &self.0,
            &host,
            port,
            request_id,
            parse_optional_alias(request.alias),
        )
        .await
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(NearbyJoinStatusReply {
            request_id: result.request_id,
            status: result.status.as_str().to_string(),
            message: result.message,
            peer_machine_id: result.peer_machine_id.unwrap_or_default(),
        }))
    }

    async fn approve_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .approve_nearby_pairing_request(
                &request.request_id,
                parse_optional_alias(request.alias),
            )
            .await
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "nearby pairing request approved".to_string(),
        }))
    }

    async fn reject_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        let rejected = self
            .0
            .reject_nearby_pairing_request(&request.request_id)
            .await;

        Ok(Response::new(OperationReply {
            ok: rejected,
            message: if rejected {
                "nearby pairing request rejected".to_string()
            } else {
                "nearby pairing request not found".to_string()
            },
        }))
    }
}

fn parse_host(value: &str) -> Result<String, Status> {
    let host = value.trim();
    if host.is_empty() {
        return Err(Status::invalid_argument("host must not be empty"));
    }
    Ok(host.to_string())
}

fn parse_port(value: u32) -> Result<u16, Status> {
    if value == 0 || value > u16::MAX as u32 {
        return Err(Status::invalid_argument(
            "port must be in the range 1..=65535",
        ));
    }
    Ok(value as u16)
}

fn parse_optional_alias(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn parse_required_field(name: &str, value: &str) -> Result<String, Status> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{name} must not be empty"
        )));
    }
    Ok(trimmed.to_string())
}

async fn build_ui_snapshot(state: &AppState) -> UiSnapshotReply {
    let paired_peers = build_peer_infos(state).await;
    let discovered_peers = build_discovered_peer_infos(state, &paired_peers).await;
    let pending_requests = build_pending_request_infos(state).await;
    let machine_id = state.snapshot().await.machine_id;

    UiSnapshotReply {
        generated_at: chrono::Utc::now().to_rfc3339(),
        daemon_online: true,
        machine_id,
        layout_matrix: state.layout().await,
        discovered_peers,
        paired_peers,
        pending_requests,
    }
}

async fn build_console_snapshot(state: &AppState) -> ConsoleSnapshotReply {
    let snapshot = state.snapshot().await;
    let paired_peers = build_peer_infos(state).await;
    let discovered_peers = build_discovered_peer_infos(state, &paired_peers).await;
    let pending_requests = build_pending_request_infos(state).await;
    let (input_locked, input_lock_supported) = state.input_lock_runtime().await;
    let capture_target_peer_id = state
        .active_input_capture_target()
        .await
        .unwrap_or_default();
    let effective_api_transport = snapshot.api_transport.effective();
    let api_pipe_name = if matches!(
        effective_api_transport,
        crate::config::ApiTransport::NamedPipe
    ) {
        snapshot.api_pipe_name.clone()
    } else {
        String::new()
    };

    ConsoleSnapshotReply {
        status: Some(StatusReply {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            running: true,
            machine_id: snapshot.machine_id,
            peer_count: snapshot.peers.len() as u32,
            protocol_version: snapshot.protocol_version,
            api_bind: snapshot.api_bind,
            api_transport: effective_api_transport.as_str().to_string(),
            api_pipe_name,
            input_locked,
            input_lock_supported,
            capture_target_peer_id,
        }),
        peers: paired_peers,
        features: state.feature_map().await.into_iter().collect(),
        discovered_peers,
        pending_requests,
        input_owner_peer_id: state.input_owner().await.unwrap_or_default(),
        input_capture_target_peer_id: state.input_capture_target().await.unwrap_or_default(),
        mdns_active: state.mdns_active().await,
        local_display_name: snapshot.device_name,
    }
}

async fn build_peer_infos(state: &AppState) -> Vec<PeerInfo> {
    state
        .list_peers()
        .await
        .into_iter()
        .map(|peer| PeerInfo {
            peer_id: peer.peer_id,
            display_name: peer.display_name,
            address: peer.address,
            connected: peer.connected,
        })
        .collect()
}

async fn build_discovered_peer_infos(
    state: &AppState,
    paired_peers: &[PeerInfo],
) -> Vec<DiscoveredPeerInfo> {
    let local_machine_id = state.snapshot().await.machine_id;
    let paired_peer_ids = paired_peers
        .iter()
        .map(|peer| peer.peer_id.clone())
        .collect::<Vec<_>>();

    let mut discovered_peers = state
        .discovered_endpoints()
        .await
        .into_iter()
        .filter(|(machine_id, _)| {
            machine_id != &local_machine_id
                && !paired_peer_ids.iter().any(|peer_id| peer_id == machine_id)
        })
        .map(|(machine_id, peer)| DiscoveredPeerInfo {
            machine_id,
            display_name: peer.display_name,
            endpoint: peer.endpoint.to_string(),
        })
        .collect::<Vec<_>>();
    discovered_peers.sort_by(|a, b| {
        a.display_name
            .to_ascii_lowercase()
            .cmp(&b.display_name.to_ascii_lowercase())
            .then_with(|| a.machine_id.cmp(&b.machine_id))
    });
    discovered_peers
}

async fn build_pending_request_infos(state: &AppState) -> Vec<NearbyPairingRequestInfo> {
    let mut pending_requests = state
        .list_pending_nearby_pairing_requests()
        .await
        .into_iter()
        .map(|request| {
            let requires_verification_code = request.verification_code.is_some();
            NearbyPairingRequestInfo {
                request_id: request.request_id,
                requester_machine_id: request.requester_machine_id,
                requester_display_name: request.requester_display_name,
                created_at: request.created_at.to_rfc3339(),
                verification_code: request.verification_code.unwrap_or_default(),
                verification_expires_at: request
                    .verification_expires_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_default(),
                requires_verification_code,
            }
        })
        .collect::<Vec<_>>();
    pending_requests.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    pending_requests
}
