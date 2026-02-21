use super::*;

#[derive(Clone)]
pub(crate) struct PairingApi(pub(super) AppState);

#[tonic::async_trait]
impl PairingService for PairingApi {
    async fn create_code(
        &self,
        request: Request<PairCreateCodeRequest>,
    ) -> Result<Response<PairCreateCodeReply>, Status> {
        let ttl = request.into_inner().ttl_seconds.max(30);
        let (code, expires_at) = self.0.create_pairing_code(ttl as u64).await;

        Ok(Response::new(PairCreateCodeReply {
            code,
            expires_at: expires_at.to_rfc3339(),
        }))
    }

    async fn join(
        &self,
        request: Request<PairJoinRequest>,
    ) -> Result<Response<PairJoinReply>, Status> {
        let request = request.into_inner();
        let peer_id = self
            .0
            .join_peer(
                request.code,
                request.host,
                if request.alias.is_empty() {
                    None
                } else {
                    Some(request.alias)
                },
            )
            .await
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        Ok(Response::new(PairJoinReply {
            accepted: true,
            peer_id,
            message: "Pairing request accepted".to_string(),
        }))
    }

    async fn list_nearby_pairing_requests(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<NearbyPairingRequestsReply>, Status> {
        let snapshot = self.0.snapshot().await;
        let expose_verification_code =
            should_expose_verification_code(snapshot.api_transport.as_str(), &snapshot.api_bind);
        let requests = self
            .0
            .list_pending_nearby_pairing_requests()
            .await
            .into_iter()
            .map(|request| {
                let has_verification_code = request.verification_code.is_some();
                NearbyPairingRequestInfo {
                    request_id: request.request_id,
                    requester_machine_id: request.requester_machine_id,
                    requester_display_name: request.requester_display_name,
                    created_at: request.created_at.to_rfc3339(),
                    verification_code: if expose_verification_code {
                        request.verification_code.unwrap_or_default()
                    } else {
                        String::new()
                    },
                    verification_expires_at: if expose_verification_code {
                        request
                            .verification_expires_at
                            .map(|value| value.to_rfc3339())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    },
                    requires_verification_code: has_verification_code,
                }
            })
            .collect();

        Ok(Response::new(NearbyPairingRequestsReply { requests }))
    }

    async fn approve_nearby_pairing_request(
        &self,
        request: Request<NearbyPairingDecisionRequest>,
    ) -> Result<Response<OperationReply>, Status> {
        let request = request.into_inner();
        self.0
            .approve_nearby_pairing_request(
                &request.request_id,
                if request.alias.is_empty() {
                    None
                } else {
                    Some(request.alias)
                },
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
                if request.alias.is_empty() {
                    None
                } else {
                    Some(request.alias)
                },
            )
            .await
            .map_err(|error| Status::internal(error.to_string()))?;

        Ok(Response::new(OperationReply {
            ok: true,
            message: "trust bundle imported".to_string(),
        }))
    }
}

fn should_expose_verification_code(api_transport: &str, api_bind: &str) -> bool {
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
}
