use super::*;

#[derive(Clone)]
pub(crate) struct DaemonApi(pub(super) AppState);

#[tonic::async_trait]
impl DaemonService for DaemonApi {
    async fn get_status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusReply>, Status> {
        let snapshot = self.0.snapshot().await;
        let (input_locked, input_lock_supported) = self.0.input_lock_runtime().await;
        let capture_target_peer_id = self
            .0
            .active_input_capture_target()
            .await
            .unwrap_or_default();
        let effective_api_transport = snapshot.api_transport.effective();
        let api_pipe_name = if matches!(
            effective_api_transport,
            crate::config::ApiTransport::NamedPipe
        ) {
            snapshot.api_pipe_name
        } else {
            String::new()
        };

        Ok(Response::new(StatusReply {
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
        }))
    }
}


