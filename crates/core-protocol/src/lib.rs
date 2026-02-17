use std::collections::BTreeSet;

use base64::Engine;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_NAME: &str = "boundless";
pub const PROTOCOL_CURRENT: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 2,
    patch: 0,
};
pub const PROTOCOL_CLIPBOARD_IMAGE_MIN: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 1,
    patch: 0,
};
pub const PROTOCOL_INPUT_ANCHOR_MIN: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 2,
    patch: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    pub const fn as_tuple(self) -> (u16, u16, u16) {
        (self.major, self.minor, self.patch)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportedProtocolRange {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

impl Default for SupportedProtocolRange {
    fn default() -> Self {
        Self {
            min: PROTOCOL_CURRENT,
            max: PROTOCOL_CURRENT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedSession {
    pub protocol: ProtocolVersion,
    pub capabilities: BTreeSet<Capability>,
}

#[derive(Debug, Error)]
pub enum NegotiationError {
    #[error("protocol major mismatch: local={local_major}, remote={remote_major}")]
    MajorMismatch { local_major: u16, remote_major: u16 },
    #[error("no overlapping supported protocol range")]
    NoOverlap,
}

pub fn negotiate(
    local_range: SupportedProtocolRange,
    remote_range: SupportedProtocolRange,
    local_caps: &BTreeSet<Capability>,
    remote_caps: &BTreeSet<Capability>,
) -> Result<NegotiatedSession, NegotiationError> {
    if local_range.max.major != remote_range.max.major {
        return Err(NegotiationError::MajorMismatch {
            local_major: local_range.max.major,
            remote_major: remote_range.max.major,
        });
    }

    let lower_bound = std::cmp::max(local_range.min.as_tuple(), remote_range.min.as_tuple());
    let upper_bound = std::cmp::min(local_range.max.as_tuple(), remote_range.max.as_tuple());

    if lower_bound > upper_bound {
        return Err(NegotiationError::NoOverlap);
    }

    let negotiated = ProtocolVersion {
        major: upper_bound.0,
        minor: upper_bound.1,
        patch: upper_bound.2,
    };

    let capabilities = local_caps
        .intersection(remote_caps)
        .copied()
        .collect::<BTreeSet<_>>();

    Ok(NegotiatedSession {
        protocol: negotiated,
        capabilities,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    ClipboardText {
        machine_id: String,
        text: String,
    },
    ClipboardImage {
        machine_id: String,
        data_b64: String,
    },
    FileStart {
        machine_id: String,
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        data_b64: String,
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
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireKeyState {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireMouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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
    #[error("json serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
}

pub fn encode_line(message: &WireMessage) -> Result<String, WireCodecError> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

pub fn decode_line(line: &str) -> Result<WireMessage, WireCodecError> {
    Ok(serde_json::from_str(line.trim())?)
}

pub fn encode_bytes_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_bytes_b64(data_b64: &str) -> Result<Vec<u8>, WireCodecError> {
    base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(WireCodecError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_common_version_and_caps() {
        let local = SupportedProtocolRange {
            min: ProtocolVersion {
                major: 1,
                minor: 0,
                patch: 0,
            },
            max: ProtocolVersion {
                major: 1,
                minor: 2,
                patch: 0,
            },
        };
        let remote = SupportedProtocolRange {
            min: ProtocolVersion {
                major: 1,
                minor: 1,
                patch: 0,
            },
            max: ProtocolVersion {
                major: 1,
                minor: 1,
                patch: 5,
            },
        };

        let local_caps = default_capabilities();
        let remote_caps = [Capability::ClipboardText, Capability::Diagnostics]
            .into_iter()
            .collect::<BTreeSet<_>>();

        let negotiated =
            negotiate(local, remote, &local_caps, &remote_caps).expect("must negotiate");
        assert_eq!(negotiated.protocol.to_string(), "1.1.5");
        assert_eq!(negotiated.capabilities.len(), 2);
    }

    #[test]
    fn fails_on_major_mismatch() {
        let err = negotiate(
            SupportedProtocolRange {
                min: ProtocolVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                },
                max: ProtocolVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                },
            },
            SupportedProtocolRange {
                min: ProtocolVersion {
                    major: 2,
                    minor: 0,
                    patch: 0,
                },
                max: ProtocolVersion {
                    major: 2,
                    minor: 0,
                    patch: 0,
                },
            },
            &default_capabilities(),
            &default_capabilities(),
        )
        .expect_err("must reject");

        assert!(matches!(err, NegotiationError::MajorMismatch { .. }));
    }

    #[test]
    fn wire_message_round_trip() {
        let original = WireMessage::Heartbeat {
            machine_id: "abc".to_string(),
            timestamp_unix_ms: 123,
        };

        let encoded = encode_line(&original).expect("encode");
        let decoded = decode_line(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn base64_round_trip() {
        let payload = vec![1u8, 2, 3, 4, 5, 255];
        let encoded = encode_bytes_b64(&payload);
        let decoded = decode_bytes_b64(&encoded).expect("decode");
        assert_eq!(decoded, payload);
    }

    #[test]
    fn rejects_invalid_base64() {
        let err = decode_bytes_b64("not-base64!").expect_err("must fail");
        assert!(matches!(err, WireCodecError::Base64(_)));
    }

    #[test]
    fn wire_message_file_chunk_round_trip() {
        let original = WireMessage::FileChunk {
            transfer_id: "xfer-1".to_string(),
            data_b64: encode_bytes_b64(&[10u8, 20, 30]),
        };

        let encoded = encode_line(&original).expect("encode");
        let decoded = decode_line(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn wire_message_clipboard_image_round_trip() {
        let original = WireMessage::ClipboardImage {
            machine_id: "machine-a".to_string(),
            data_b64: encode_bytes_b64(&[1u8, 2, 3, 4]),
        };

        let encoded = encode_line(&original).expect("encode");
        let decoded = decode_line(&encoded).expect("decode");
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

        let encoded = encode_line(&original).expect("encode");
        let decoded = decode_line(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }
}
