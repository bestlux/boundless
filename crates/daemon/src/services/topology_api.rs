use super::*;

#[derive(Clone)]
pub(crate) struct TopologyApi(pub(super) AppState);

#[tonic::async_trait]
impl TopologyService for TopologyApi {
    async fn list_peers(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<PeerListReply>, Status> {
        let peers = self
            .0
            .list_peers()
            .await
            .into_iter()
            .map(|peer| PeerInfo {
                peer_id: peer.peer_id,
                display_name: peer.display_name,
                address: peer.address,
                connected: peer.connected,
            })
            .collect();

        Ok(Response::new(PeerListReply { peers }))
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
            .map_err(|e| Status::internal(e.to_string()))?;

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
        let matrix_spec = self.0.layout().await;
        Ok(Response::new(LayoutReply { matrix_spec }))
    }

    async fn layout_set(
        &self,
        request: Request<LayoutSetRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let matrix = request.into_inner().matrix_spec;
        self.0
            .set_layout(matrix)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "Layout updated".to_string(),
        }))
    }
}


