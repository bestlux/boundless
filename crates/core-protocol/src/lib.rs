use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_NAME: &str = "boundless";
pub const PROTOCOL_CURRENT: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 0,
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
    Error {
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum WireCodecError {
    #[error("json serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub fn encode_line(message: &WireMessage) -> Result<String, WireCodecError> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    Ok(line)
}

pub fn decode_line(line: &str) -> Result<WireMessage, WireCodecError> {
    Ok(serde_json::from_str(line.trim())?)
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
}
