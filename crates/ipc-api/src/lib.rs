pub mod boundless {
    pub mod v1 {
        tonic::include_proto!("boundless.v1");
    }
}

pub mod broker_events;
pub mod client_identity;

/// The clipboard policy permits 8 MiB bitmap payloads. Reserve another MiB
/// for the broker token, protobuf framing, and future envelope fields so a
/// policy-valid image cannot trip tonic's 4 MiB unary default.
pub const CONTROL_PLANE_MAX_MESSAGE_BYTES: usize = 9 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundless::v1::{
        ClipboardBrokerExchangeReply, ClipboardBrokerExchangeRequest, ClipboardBrokerPayload,
        clipboard_broker_payload,
    };
    use prost::Message;

    #[test]
    fn policy_max_bitmap_fits_broker_request_and_reply_envelopes() {
        const POLICY_MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
        let payload = ClipboardBrokerPayload {
            payload: Some(clipboard_broker_payload::Payload::ImageBmp(vec![
                0;
                POLICY_MAX_IMAGE_BYTES
            ])),
        };
        let request = ClipboardBrokerExchangeRequest {
            broker_token: "00000000-0000-0000-0000-000000000000".to_string(),
            local_payload: Some(payload.clone()),
            apply_report: None,
            local_sequence_valid: true,
            local_sequence: 1,
        };
        let reply = ClipboardBrokerExchangeReply {
            accepted: true,
            remote_payload: Some(payload),
            remote_source_peer_id: "peer-a".to_string(),
            remote_hash: "sha256".to_string(),
            ..Default::default()
        };

        assert!(request.encoded_len() < CONTROL_PLANE_MAX_MESSAGE_BYTES);
        assert!(reply.encoded_len() < CONTROL_PLANE_MAX_MESSAGE_BYTES);
    }
}
