use std::path::{Path, PathBuf};

use thiserror::Error;

pub const MAX_TRANSFER_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum TransferPolicyError {
    #[error("file exceeds transfer limit: {size} > {limit}")]
    FileTooLarge { size: u64, limit: u64 },
}

pub fn validate_transfer_size(size: u64) -> Result<(), TransferPolicyError> {
    if size > MAX_TRANSFER_BYTES {
        return Err(TransferPolicyError::FileTooLarge {
            size,
            limit: MAX_TRANSFER_BYTES,
        });
    }

    Ok(())
}

pub fn resolve_conflict_path(target_dir: &Path, file_name: &str) -> PathBuf {
    let candidate = target_dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let source = Path::new(file_name);
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let extension = source.extension().map(|e| e.to_string_lossy().to_string());

    for i in 1..=9_999u32 {
        let mut candidate_name = format!("{} ({i})", stem);
        if let Some(ext) = &extension {
            candidate_name.push('.');
            candidate_name.push_str(ext);
        }

        let candidate = target_dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    target_dir.join(format!("{} ({})", stem, uuid_fallback()))
}

fn uuid_fallback() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("fallback-{now}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_large_files() {
        let err = validate_transfer_size(MAX_TRANSFER_BYTES + 1).expect_err("must fail");
        assert!(matches!(err, TransferPolicyError::FileTooLarge { .. }));
    }

    #[test]
    fn resolves_conflicting_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("hello.txt");
        std::fs::write(&first, "a").expect("seed file");

        let next = resolve_conflict_path(temp.path(), "hello.txt");
        assert!(next.ends_with("hello (1).txt"));
    }
}
