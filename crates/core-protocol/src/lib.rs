use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_NAME: &str = "boundless";
pub const PROTOCOL_CURRENT: ProtocolVersion = ProtocolVersion {
    major: 4,
    minor: 2,
    patch: 0,
};
pub const MAX_WIRE_PAYLOAD_BYTES: usize = 256 * 1024;
pub const WIRE_FRAME_LENGTH_PREFIX_BYTES: usize = std::mem::size_of::<u32>();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    ClipboardText,
    ClipboardImage,
    FileTransfer,
    EdgeSwitch,
    Hotkeys,
    Diagnostics,
    SafeReset,
}

pub fn default_capabilities() -> BTreeSet<Capability> {
    [
        Capability::ClipboardText,
        Capability::ClipboardImage,
        Capability::FileTransfer,
        Capability::EdgeSwitch,
        Capability::Hotkeys,
        Capability::Diagnostics,
        Capability::SafeReset,
    ]
    .into_iter()
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireMessage {
    Hello {
        machine_id: String,
        display_name: String,
        protocol: ProtocolVersion,
        capability_count: usize,
    },
    HelloAck {
        machine_id: String,
        accepted: bool,
    },
    Heartbeat {
        machine_id: String,
        timestamp_unix_ms: i64,
    },
    AntiIdlePulse {
        keep_display_on: bool,
    },
    ClipboardText {
        machine_id: String,
        text: String,
    },
    ClipboardImage {
        machine_id: String,
        data: Vec<u8>,
    },
    ClipboardImageStart {
        machine_id: String,
        transfer_id: String,
        total_bytes: u64,
        hash_hex: String,
    },
    ClipboardImageChunk {
        transfer_id: String,
        data: Vec<u8>,
    },
    ClipboardImageEnd {
        transfer_id: String,
    },
    FileStart {
        machine_id: String,
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        data: Vec<u8>,
    },
    FileEnd {
        transfer_id: String,
    },
    InputFrame {
        machine_id: String,
        sequence: u64,
        timestamp_unix_ms: i64,
        events: Vec<WireInputEvent>,
    },
    LayoutMatrix {
        machine_id: String,
        matrix_spec: String,
    },
    Error {
        message: String,
    },
    FileChunkCredit {
        transfer_id: String,
        chunk_credits: u32,
    },
    FileTransferRejected {
        transfer_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireKeyState {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireInputEvent {
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbsolute {
        x_norm: u16,
        y_norm: u16,
    },
    MouseButton {
        button: WireMouseButton,
        state: WireKeyState,
    },
    MouseWheel {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        scan_code: u16,
        state: WireKeyState,
    },
}

#[derive(Debug, Error)]
pub enum WireCodecError {
    #[error("frame header too short: {size_bytes} bytes")]
    FrameHeaderTooShort { size_bytes: usize },
    #[error("frame too large to encode: {size_bytes} bytes")]
    FrameTooLargeToEncode { size_bytes: usize },
    #[error("frame length mismatch: declared={declared_bytes}, actual={actual_bytes}")]
    FrameLengthMismatch {
        declared_bytes: usize,
        actual_bytes: usize,
    },
    #[error(
        "frame payload decode did not consume all bytes: consumed={consumed_bytes}, payload={payload_bytes}"
    )]
    TrailingPayloadBytes {
        consumed_bytes: usize,
        payload_bytes: usize,
    },
    #[error("binary serialization error: {0}")]
    Serialize(#[source] bincode::error::EncodeError),
    #[error("binary deserialization error: {0}")]
    Deserialize(#[source] bincode::error::DecodeError),
}

pub fn encode_frame(message: &WireMessage) -> Result<Vec<u8>, WireCodecError> {
    let mut frame = Vec::new();
    encode_frame_to_vec(message, &mut frame)?;
    Ok(frame)
}

pub fn encode_frame_to_vec(
    message: &WireMessage,
    frame_buffer: &mut Vec<u8>,
) -> Result<(), WireCodecError> {
    let payload = bincode::serde::encode_to_vec(message, bincode::config::standard())
        .map_err(WireCodecError::Serialize)?;
    let payload_len = payload.len();
    if payload_len > MAX_WIRE_PAYLOAD_BYTES {
        return Err(WireCodecError::FrameTooLargeToEncode {
            size_bytes: payload_len,
        });
    }
    let Ok(payload_len_u32) = u32::try_from(payload_len) else {
        return Err(WireCodecError::FrameTooLargeToEncode {
            size_bytes: payload_len,
        });
    };

    frame_buffer.clear();
    frame_buffer.reserve(WIRE_FRAME_LENGTH_PREFIX_BYTES + payload_len);
    frame_buffer.extend_from_slice(&payload_len_u32.to_be_bytes());
    frame_buffer.extend_from_slice(&payload);
    Ok(())
}

pub fn decode_frame(frame: &[u8]) -> Result<WireMessage, WireCodecError> {
    if frame.len() < WIRE_FRAME_LENGTH_PREFIX_BYTES {
        return Err(WireCodecError::FrameHeaderTooShort {
            size_bytes: frame.len(),
        });
    }

    let declared_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    let payload = &frame[WIRE_FRAME_LENGTH_PREFIX_BYTES..];
    if payload.len() != declared_len {
        return Err(WireCodecError::FrameLengthMismatch {
            declared_bytes: declared_len,
            actual_bytes: payload.len(),
        });
    }
    decode_frame_payload(payload)
}

pub fn decode_frame_payload(payload: &[u8]) -> Result<WireMessage, WireCodecError> {
    let config = bincode::config::standard().with_limit::<MAX_WIRE_PAYLOAD_BYTES>();
    let (message, consumed) =
        bincode::serde::decode_from_slice(payload, config).map_err(WireCodecError::Deserialize)?;
    if consumed != payload.len() {
        return Err(WireCodecError::TrailingPayloadBytes {
            consumed_bytes: consumed,
            payload_bytes: payload.len(),
        });
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_message_round_trip() {
        let original = WireMessage::Heartbeat {
            machine_id: "abc".to_string(),
            timestamp_unix_ms: 123,
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_round_trip_with_reused_buffer() {
        let original = WireMessage::Heartbeat {
            machine_id: "abc".to_string(),
            timestamp_unix_ms: 123,
        };

        let mut frame = Vec::with_capacity(64);
        encode_frame_to_vec(&original, &mut frame).expect("encode");
        let payload = &frame[WIRE_FRAME_LENGTH_PREFIX_BYTES..];
        let decoded = decode_frame_payload(payload).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_file_chunk_round_trip() {
        let original = WireMessage::FileChunk {
            transfer_id: "xfer-1".to_string(),
            data: vec![10u8, 20, 30],
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_file_chunk_credit_round_trip() {
        let original = WireMessage::FileChunkCredit {
            transfer_id: "xfer-1".to_string(),
            chunk_credits: 8,
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_file_transfer_rejected_round_trip() {
        let original = WireMessage::FileTransferRejected {
            transfer_id: "xfer-1".to_string(),
            reason: "receive_policy_denied".to_string(),
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_clipboard_image_round_trip() {
        let original = WireMessage::ClipboardImage {
            machine_id: "machine-a".to_string(),
            data: vec![1u8, 2, 3, 4],
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_clipboard_image_chunk_round_trip() {
        let original = WireMessage::ClipboardImageStart {
            machine_id: "machine-a".to_string(),
            transfer_id: "clip-1".to_string(),
            total_bytes: 4,
            hash_hex: "abcd".to_string(),
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_clipboard_image_chunk_data_round_trip() {
        let original = WireMessage::ClipboardImageChunk {
            transfer_id: "clip-1".to_string(),
            data: vec![1u8, 2, 3, 4],
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_input_frame_round_trip() {
        let original = WireMessage::InputFrame {
            machine_id: "machine-a".to_string(),
            sequence: 42,
            timestamp_unix_ms: 1000,
            events: vec![
                WireInputEvent::MouseMove { dx: 5, dy: -2 },
                WireInputEvent::Key {
                    scan_code: 30,
                    state: WireKeyState::Down,
                },
            ],
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_layout_matrix_round_trip() {
        let original = WireMessage::LayoutMatrix {
            machine_id: "machine-a".to_string(),
            matrix_spec: "peer-b,self".to_string(),
        };

        let encoded = encode_frame(&original).expect("encode");
        let decoded = decode_frame(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_frame_rejects_mismatched_length() {
        let mut encoded = encode_frame(&WireMessage::Error {
            message: "oops".to_string(),
        })
        .expect("encode");
        encoded[0] = 0;
        encoded[1] = 0;
        encoded[2] = 0;
        encoded[3] = 1;

        let err = decode_frame(&encoded).expect_err("must fail");
        assert!(matches!(err, WireCodecError::FrameLengthMismatch { .. }));
    }

    #[test]
    fn encode_frame_rejects_payload_over_limit() {
        let oversized = WireMessage::ClipboardImageChunk {
            transfer_id: "clip-1".to_string(),
            data: vec![0u8; MAX_WIRE_PAYLOAD_BYTES + 1],
        };

        let err = encode_frame(&oversized).expect_err("must fail");
        assert!(matches!(err, WireCodecError::FrameTooLargeToEncode { .. }));
    }
}
