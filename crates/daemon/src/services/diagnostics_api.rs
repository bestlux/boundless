use super::*;

#[derive(Clone)]
pub(crate) struct DiagnosticsApi(pub(super) AppState);

#[tonic::async_trait]
impl DiagnosticsService for DiagnosticsApi {
    async fn dump(
        &self,
        request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        let output = request.into_inner().output_path;
        let output = if output.is_empty() {
            None
        } else {
            Some(output)
        };

        let bundle_path = self
            .0
            .diagnostics_dump(output)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

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
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "safe reset complete".to_string(),
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

    async fn send_clipboard_text(
        &self,
        request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .queue_clipboard_text(&request.peer_id, request.text)
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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

    async fn list_discovery_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DiscoveryPeersReply>, Status> {
        let peers = self
            .0
            .discovered_endpoints()
            .await
            .into_iter()
            .map(|(machine_id, record)| DiscoveredPeerInfo {
                machine_id,
                display_name: record.display_name,
                endpoint: record.endpoint.to_string(),
            })
            .collect();

        Ok(Response::new(DiscoveryPeersReply {
            mdns_active: self.0.mdns_active().await,
            peers,
        }))
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
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

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
}


