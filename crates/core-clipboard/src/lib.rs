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

#[derive(Debug, Error)]
pub enum BmpValidationError {
    #[error("bmp payload is too small")]
    TooSmall,
    #[error("bmp payload must start with BM header")]
    MissingSignature,
    #[error("bmp payload declared file size is too small")]
    DeclaredSizeTooSmall,
    #[error("bmp payload is truncated relative to declared file size")]
    TruncatedByDeclaredSize,
    #[error("bmp payload has invalid pixel offset")]
    InvalidPixelOffset,
    #[error("bmp payload DIB header is too small")]
    DibHeaderTooSmall,
    #[error("bmp payload is truncated before full DIB header")]
    TruncatedDibHeader,
    #[error("bmp payload is truncated before declared pixel data")]
    TruncatedPixelData,
    #[error("bmp payload has no pixel data")]
    MissingPixelData,
}

pub fn payload_hash_hex(payload: &ClipboardPayload) -> String {
    match payload {
        ClipboardPayload::Text(text) => hash_bytes_hex(0x01, text.as_bytes()),
        ClipboardPayload::Image(bytes) => image_hash_hex(bytes),
    }
}

pub fn text_hash_hex(text: &str) -> String {
    hash_bytes_hex(0x01, text.as_bytes())
}

pub fn image_hash_hex(bytes: &[u8]) -> String {
    hash_bytes_hex(0x02, bytes)
}

/// Reduces clipboard event detail to a small, explicitly allowed metadata vocabulary.
///
/// Clipboard contents and content-derived identifiers must not cross diagnostic boundaries.
/// Callers should still construct metadata-only events at the source; this helper is the
/// defense-in-depth boundary for retained events, APIs, CLIs, and diagnostic exports.
pub fn sanitize_clipboard_event_detail(kind: &str, detail: &str) -> String {
    sanitize_clipboard_event_detail_with_policy(kind, detail, false)
}

/// Applies the clipboard metadata boundary to an event being returned to an operator.
/// Aggregate count/time fields are accepted here only after the retained-event store has
/// stripped producer-supplied aggregate fields and generated its own summary metadata.
pub fn sanitize_clipboard_event_output_detail(kind: &str, detail: &str) -> String {
    sanitize_clipboard_event_detail_with_policy(kind, detail, true)
}

fn sanitize_clipboard_event_detail_with_policy(
    kind: &str,
    detail: &str,
    allow_aggregate_metadata: bool,
) -> String {
    if !kind.starts_with("clipboard") {
        return detail.to_string();
    }

    let safe_tokens = detail
        .split_whitespace()
        .filter_map(|token| sanitize_clipboard_metadata_token(token, allow_aggregate_metadata))
        .collect::<Vec<_>>();

    if safe_tokens.is_empty() {
        "metadata_only=true".to_string()
    } else {
        safe_tokens.join(" ")
    }
}

fn sanitize_clipboard_metadata_token(
    token: &str,
    allow_aggregate_metadata: bool,
) -> Option<String> {
    let (key, value) = token.split_once('=')?;
    let allowed = match key {
        "payload_type" => matches!(value, "text" | "bmp" | "image" | "unknown"),
        "disposition" => matches!(
            value,
            "sent"
                | "received"
                | "rejected"
                | "disabled"
                | "deduped"
                | "replayed"
                | "apply_failed"
                | "unmatched_apply_report"
        ),
        "reason" => matches!(
            value,
            "policy_or_validation"
                | "feature_disabled"
                | "duplicate"
                | "replay"
                | "apply_failed"
                | "unmatched"
                | "too_many_transfers"
                | "duplicate_transfer"
                | "payload_too_large"
                | "size_overflow"
                | "chunk_exceeds_total"
                | "size_mismatch"
                | "hash_mismatch"
                | "unknown"
        ),
        "applied" | "metadata_only" => matches!(value, "true" | "false"),
        "active_transfers"
        | "transfer_limit"
        | "configured_limit_bytes"
        | "announced_bytes"
        | "attempted_bytes"
        | "expected_bytes"
        | "received_bytes" => value.parse::<u64>().is_ok(),
        "sample_count" => allow_aggregate_metadata && value.parse::<u64>().is_ok(),
        "first_seen" | "last_seen" => allow_aggregate_metadata && is_safe_event_timestamp(value),
        _ => false,
    };

    allowed.then(|| format!("{key}={value}"))
}

fn is_safe_event_timestamp(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.chars().all(|character| {
            character.is_ascii_digit()
                || matches!(character, '-' | ':' | '.' | 'T' | 'Z' | '+' | 't' | 'z')
        })
}

fn hash_bytes_hex(kind: u8, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update([kind]);
    hasher.update(bytes);
    bytes_to_hex(&hasher.finalize())
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

pub fn validate_bmp_payload(bytes: &[u8]) -> Result<(), BmpValidationError> {
    const BMP_FILE_HEADER_BYTES: usize = 14;
    const BMP_INFO_HEADER_BYTES: usize = 40;
    const BMP_MIN_BYTES: usize = BMP_FILE_HEADER_BYTES + BMP_INFO_HEADER_BYTES;

    if bytes.len() < BMP_MIN_BYTES {
        return Err(BmpValidationError::TooSmall);
    }
    if bytes[0] != b'B' || bytes[1] != b'M' {
        return Err(BmpValidationError::MissingSignature);
    }

    let declared_file_size = read_u32_le(bytes, 2) as usize;
    if declared_file_size < BMP_MIN_BYTES {
        return Err(BmpValidationError::DeclaredSizeTooSmall);
    }
    if declared_file_size > bytes.len() {
        return Err(BmpValidationError::TruncatedByDeclaredSize);
    }

    let pixel_offset = read_u32_le(bytes, 10) as usize;
    if pixel_offset < BMP_FILE_HEADER_BYTES || pixel_offset >= bytes.len() {
        return Err(BmpValidationError::InvalidPixelOffset);
    }

    let dib_header_size = read_u32_le(bytes, 14) as usize;
    if dib_header_size < BMP_INFO_HEADER_BYTES {
        return Err(BmpValidationError::DibHeaderTooSmall);
    }
    if BMP_FILE_HEADER_BYTES + dib_header_size > bytes.len() {
        return Err(BmpValidationError::TruncatedDibHeader);
    }

    let declared_pixel_size = read_u32_le(bytes, 34) as usize;
    let available_pixel_size = bytes.len().saturating_sub(pixel_offset);
    if declared_pixel_size == 0 {
        if available_pixel_size == 0 {
            return Err(BmpValidationError::MissingPixelData);
        }
    } else if available_pixel_size < declared_pixel_size {
        return Err(BmpValidationError::TruncatedPixelData);
    }

    Ok(())
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(raw)
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
    fn rejects_truncated_bmp() {
        let payload = [
            b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 100, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 255, 0,
        ];

        let err = validate_bmp_payload(&payload).expect_err("must reject");
        assert!(matches!(err, BmpValidationError::TruncatedPixelData));
    }

    #[test]
    fn accepts_minimal_bmp() {
        let payload = [
            b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0,
            1, 0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 255, 0,
        ];
        validate_bmp_payload(&payload).expect("must accept");
    }

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

    #[test]
    fn direct_hash_helpers_match_payload_hashes() {
        assert_eq!(
            text_hash_hex("abc"),
            payload_hash_hex(&ClipboardPayload::Text("abc".to_string()))
        );
        assert_eq!(
            image_hash_hex(b"abc"),
            payload_hash_hex(&ClipboardPayload::Image(b"abc".to_vec()))
        );
    }

    #[test]
    fn clipboard_event_detail_allows_only_explicit_metadata() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_7b15fce0";
        let detail = format!(
            "payload_type=bmp disposition=rejected reason=hash_mismatch applied=false expected={SECRET} actual={SECRET}"
        );

        let sanitized = sanitize_clipboard_event_detail("clipboard_text", &detail);

        assert_eq!(
            sanitized,
            "payload_type=bmp disposition=rejected reason=hash_mismatch applied=false"
        );
        assert!(!sanitized.contains(SECRET));
        assert!(!sanitized.contains("expected="));
        assert!(!sanitized.contains("actual="));
    }

    #[test]
    fn malformed_clipboard_detail_fails_closed() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_c9ce0a8e";

        assert_eq!(
            sanitize_clipboard_event_detail("clipboard_text", SECRET),
            "metadata_only=true"
        );
        assert_eq!(
            sanitize_clipboard_event_detail("input_frame", SECRET),
            SECRET
        );
    }

    #[test]
    fn clipboard_rejection_numeric_metadata_is_strictly_bounded() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_numeric_9dbe7dbd";
        let detail = format!(
            "payload_type=bmp disposition=rejected reason=size_mismatch expected_bytes=64 received_bytes=32 announced_bytes={SECRET} attempted_bytes=-1 configured_limit_bytes=8MB"
        );

        let sanitized = sanitize_clipboard_event_detail("clipboard_image_rejected", &detail);

        assert_eq!(
            sanitized,
            "payload_type=bmp disposition=rejected reason=size_mismatch expected_bytes=64 received_bytes=32"
        );
        assert!(!sanitized.contains(SECRET));
    }

    #[test]
    fn clipboard_output_accepts_only_store_generated_summary_shape() {
        const SECRET: &str = "BOUNDLESS_SECRET_SENTINEL_summary_fea6f173";
        let producer_detail = format!(
            "payload_type=bmp disposition=rejected reason=hash_mismatch sample_count=999 first_seen=1999-01-01T00:00:00Z expected_hash={SECRET}"
        );
        assert_eq!(
            sanitize_clipboard_event_detail("clipboard_image_rejected", &producer_detail),
            "payload_type=bmp disposition=rejected reason=hash_mismatch"
        );

        let output_detail = format!(
            "payload_type=bmp disposition=rejected reason=hash_mismatch sample_count=4 first_seen=2026-07-10T00:00:00Z last_seen=2026-07-10T00:00:01Z expected_hash={SECRET}"
        );
        let sanitized =
            sanitize_clipboard_event_output_detail("clipboard_image_rejected", &output_detail);

        assert!(sanitized.contains("sample_count=4"));
        assert!(sanitized.contains("first_seen=2026-07-10T00:00:00Z"));
        assert!(sanitized.contains("last_seen=2026-07-10T00:00:01Z"));
        assert!(!sanitized.contains(SECRET));
    }
}
