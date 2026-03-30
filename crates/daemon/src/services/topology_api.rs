use super::*;

#[derive(Clone)]
pub struct TopologyApi(pub(super) AppState);

#[tonic::async_trait]
impl TopologyService for TopologyApi {
    async fn list_peers(&self, request: Request<Empty>) -> Result<Response<PeerListReply>, Status> {
        ControlPlaneApi(self.0.clone()).list_peers(request).await
    }

    async fn remove_peer(
        &self,
        request: Request<RemovePeerRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone()).remove_peer(request).await
    }

    async fn layout_show(&self, request: Request<Empty>) -> Result<Response<LayoutReply>, Status> {
        ControlPlaneApi(self.0.clone()).layout_show(request).await
    }

    async fn layout_set(
        &self,
        request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone()).layout_set(request).await
    }
}
