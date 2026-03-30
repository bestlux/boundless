use super::*;

#[derive(Clone)]
pub struct PairingApi(pub(super) AppState);

#[tonic::async_trait]
impl PairingService for PairingApi {
    async fn create_code(
        &self,
        request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .create_pairing_code(request)
            .await
    }

    async fn join(
        &self,
        request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .join_with_pairing_code(request)
            .await
    }

    async fn list_nearby_pairing_requests(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<NearbyPairingRequestsReply>, Status> {
        let remote_addr = request.remote_addr();
        let snapshot = self.0.snapshot().await;
        let expose_verification_code = should_expose_verification_code(
            snapshot.api_transport.as_str(),
            &snapshot.api_bind,
            remote_addr,
        );
        let snapshot = ControlPlaneApi(self.0.clone())
            .get_console_snapshot(Request::new(Empty {}))
            .await?
            .into_inner();
        let requests = snapshot
            .pending_requests
            .into_iter()
            .map(|mut request| {
                if !expose_verification_code {
                    request.verification_code.clear();
                    request.verification_expires_at.clear();
                }
                request
            })
            .collect();

        Ok(Response::new(NearbyPairingRequestsReply { requests }))
    }

    async fn approve_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .approve_nearby_pairing_request(request)
            .await
    }

    async fn reject_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .reject_nearby_pairing_request(request)
            .await
    }

    async fn export_trust_bundle(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<TrustBundleReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .export_trust_bundle(request)
            .await
    }

    async fn import_trust_bundle(
        &self,
        request: Request<ImportTrustBundleRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        ControlPlaneApi(self.0.clone())
            .import_trust_bundle(request)
            .await
    }
}

fn should_expose_verification_code(
    api_transport: &str,
    api_bind: &str,
    remote_addr: Option<std::net::SocketAddr>,
) -> bool {
    if api_transport.eq_ignore_ascii_case("npipe")
        || api_transport.eq_ignore_ascii_case("named_pipe")
    {
        return true;
    }

    let trimmed = api_bind.trim();
    if trimmed.eq_ignore_ascii_case("localhost") || trimmed.starts_with("localhost:") {
        return true;
    }

    trimmed
        .parse::<std::net::SocketAddr>()
        .map(|address| address.ip().is_loopback())
        .unwrap_or(false)
        || remote_addr.is_some_and(|address| address.ip().is_loopback())
}

#[cfg(test)]
mod tests {
    use super::should_expose_verification_code;

    #[test]
    fn should_expose_verification_code_allows_npipe_transport() {
        assert!(should_expose_verification_code(
            "npipe",
            "0.0.0.0:50051",
            None
        ));
    }

    #[test]
    fn should_expose_verification_code_allows_loopback_bind() {
        assert!(should_expose_verification_code(
            "tcp",
            "127.0.0.1:50051",
            None
        ));
    }

    #[test]
    fn should_expose_verification_code_allows_loopback_client_on_wildcard_bind() {
        let remote = "127.0.0.1:53429"
            .parse()
            .expect("parse loopback client socket");
        assert!(should_expose_verification_code(
            "tcp",
            "0.0.0.0:50051",
            Some(remote)
        ));
        assert!(!should_expose_verification_code(
            "tcp",
            "0.0.0.0:50051",
            None
        ));
    }
}
