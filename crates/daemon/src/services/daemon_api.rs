use super::*;

#[derive(Clone)]
pub struct DaemonApi(pub(super) AppState);

#[tonic::async_trait]
impl DaemonService for DaemonApi {
    async fn get_status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusReply>, Status> {
        control_plane_api(self.0.clone()).get_status(request).await
    }
}
