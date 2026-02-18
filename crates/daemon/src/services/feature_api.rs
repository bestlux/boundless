use super::*;

#[derive(Clone)]
pub(crate) struct FeatureApi(pub(super) AppState);

#[tonic::async_trait]
impl FeatureService for FeatureApi {
    async fn set_feature(
        &self,
        request: Request<FeatureSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_feature(request.name.clone(), request.enabled)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("{}={}", request.name, request.enabled),
        }))
    }

    async fn list_features(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<FeatureListReply>, Status> {
        let features = self.0.feature_map().await.into_iter().collect();
        Ok(Response::new(FeatureListReply { features }))
    }

    async fn set_hotkey(
        &self,
        request: Request<HotkeySetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .set_hotkey(request.action.clone(), request.combo.clone())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: format!("hotkey {}={}", request.action, request.combo),
        }))
    }
}
