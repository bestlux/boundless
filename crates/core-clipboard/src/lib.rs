use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text(String),
    Image(Vec<u8>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardPolicy {
    pub enabled: bool,
    pub max_image_bytes: usize,
}

impl Default for ClipboardPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub enum ClipboardPolicyError {
    #[error("clipboard sync is disabled")]
    Disabled,
    #[error("clipboard image payload too large: {size} > {limit}")]
    ImageTooLarge { size: usize, limit: usize },
}

pub fn validate_payload(
    policy: ClipboardPolicy,
    payload: &ClipboardPayload,
) -> Result<(), ClipboardPolicyError> {
    if !policy.enabled {
        return Err(ClipboardPolicyError::Disabled);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_large_image() {
        let payload = ClipboardPayload::Image(vec![0u8; 2]);
        let err = validate_payload(
            ClipboardPolicy {
                enabled: true,
                max_image_bytes: 1,
            },
            &payload,
        )
        .expect_err("must reject");

        assert!(matches!(err, ClipboardPolicyError::ImageTooLarge { .. }));
    }
}
