use super::*;

#[derive(Clone)]
pub struct DiagnosticsApi(pub(super) AppState);

#[tonic::async_trait]
impl DiagnosticsService for DiagnosticsApi {
    async fn dump(
        &self,
        request: Request<DiagnosticsDumpRequest>,
    ) -> Result<Response<DiagnosticsDumpReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .dump_diagnostics(request)
            .await
    }

    async fn safe_reset(
        &self,
        request: Request<SafeResetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone()).safe_reset(request).await
    }

    async fn trigger_hotkey_action(
        &self,
        request: Request<HotkeyTriggerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .trigger_hotkey_action(request)
            .await
    }

    async fn send_clipboard_text(
        &self,
        request: Request<SendClipboardTextRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .send_clipboard_text(request)
            .await
    }

    async fn send_clipboard_image(
        &self,
        request: Request<SendClipboardImageRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .send_clipboard_image(request)
            .await
    }

    async fn send_file(
        &self,
        request: Request<SendFileRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone()).send_file(request).await
    }

    async fn send_input_move(
        &self,
        request: Request<SendInputMoveRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .send_input_move(request)
            .await
    }

    async fn send_input_key(
        &self,
        request: Request<SendInputKeyRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .send_input_key(request)
            .await
    }

    async fn list_transport_events(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<TransportEventsReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .list_transport_events(request)
            .await
    }

    async fn list_discovery_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<DiscoveryPeersReply>, Status> {
        let snapshot = ControlPlaneApi(self.0.clone())
            .get_console_snapshot(Request::new(Empty {}))
            .await?
            .into_inner();

        Ok(Response::new(DiscoveryPeersReply {
            mdns_active: snapshot.mdns_active,
            peers: snapshot.discovered_peers,
        }))
    }

    async fn get_input_owner(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .get_input_owner(request)
            .await
    }

    async fn claim_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .claim_input_owner(request)
            .await
    }

    async fn release_input_owner(
        &self,
        request: Request<InputOwnerRequest>,
    ) -> Result<Response<InputOwnerReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .release_input_owner(request)
            .await
    }

    async fn get_input_capture_target(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .get_input_capture_target(request)
            .await
    }

    async fn set_input_capture_target(
        &self,
        request: Request<InputCaptureTargetRequest>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .set_input_capture_target(request)
            .await
    }

    async fn clear_input_capture_target(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<InputCaptureTargetReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .clear_input_capture_target(request)
            .await
    }
}
