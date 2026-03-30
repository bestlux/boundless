use super::*;

#[derive(Clone)]
pub struct FeatureApi(pub(super) AppState);

#[tonic::async_trait]
impl FeatureService for FeatureApi {
    async fn set_feature(
        &self,
        request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        control_plane_api(self.0.clone()).set_feature(request).await
    }

    async fn list_features(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<FeatureListReply>, Status> {
        control_plane_api(self.0.clone())
            .list_features(request)
            .await
    }

    async fn set_hotkey(
        &self,
        request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        control_plane_api(self.0.clone()).set_hotkey(request).await
    }
}
