use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text(String),
    Image(Vec<u8>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardPolicy {
    pub enabled: bool,
    pub max_text_bytes: usize,
    pub max_image_bytes: usize,
}

impl Default for ClipboardPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_text_bytes: 256 * 1024,
            max_image_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum ClipboardPolicyError {
    #[error("clipboard sync is disabled")]
    Disabled,
    #[error("clipboard text payload too large: {size} > {limit}")]
    TextTooLarge { size: usize, limit: usize },
    #[error("clipboard image payload too large: {size} > {limit}")]
    ImageTooLarge { size: usize, limit: usize },
}

pub fn payload_hash_hex(payload: &ClipboardPayload) -> String {
    let mut hasher = Sha256::new();
    match payload {
        ClipboardPayload::Text(text) => {
            hasher.update([0x01]);
            hasher.update(text.as_bytes());
        }
        ClipboardPayload::Image(bytes) => {
            hasher.update([0x02]);
            hasher.update(bytes);
        }
    }
    bytes_to_hex(&hasher.finalize())
}

pub fn text_hash_hex(text: &str) -> String {
    payload_hash_hex(&ClipboardPayload::Text(text.to_string()))
}

pub fn validate_payload(
    policy: ClipboardPolicy,
    payload: &ClipboardPayload,
) -> Result<(), ClipboardPolicyError> {
    if !policy.enabled {
        return Err(ClipboardPolicyError::Disabled);
    }

    if let ClipboardPayload::Text(text) = payload
        && text.len() > policy.max_text_bytes
    {
        return Err(ClipboardPolicyError::TextTooLarge {
            size: text.len(),
            limit: policy.max_text_bytes,
        });
    }

    if let ClipboardPayload::Image(bytes) = payload
        && bytes.len() > policy.max_image_bytes
    {
        return Err(ClipboardPolicyError::ImageTooLarge {
            size: bytes.len(),
            limit: policy.max_image_bytes,
        });
    }

    Ok(())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_large_image() {
        let payload = ClipboardPayload::Image(vec![0u8; 2]);
        let err = validate_payload(
            ClipboardPolicy {
                enabled: true,
                max_text_bytes: usize::MAX,
                max_image_bytes: 1,
            },
            &payload,
        )
        .expect_err("must reject");

        assert!(matches!(err, ClipboardPolicyError::ImageTooLarge { .. }));
    }

    #[test]
    fn rejects_large_text() {
        let payload = ClipboardPayload::Text("ab".to_string());
        let err = validate_payload(
            ClipboardPolicy {
                enabled: true,
                max_text_bytes: 1,
                max_image_bytes: usize::MAX,
            },
            &payload,
        )
        .expect_err("must reject");
        assert!(matches!(err, ClipboardPolicyError::TextTooLarge { .. }));
    }

    #[test]
    fn text_hash_is_stable() {
        let one = text_hash_hex("hello");
        let two = text_hash_hex("hello");
        assert_eq!(one, two);
    }

    #[test]
    fn text_and_image_hash_do_not_collide_for_same_bytes() {
        let text = payload_hash_hex(&ClipboardPayload::Text("abc".to_string()));
        let image = payload_hash_hex(&ClipboardPayload::Image(b"abc".to_vec()));
        assert_ne!(text, image);
    }
}
