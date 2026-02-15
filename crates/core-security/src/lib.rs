use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingCode {
    pub value: String,
    pub expires_at: DateTime<Utc>,
}

impl PairingCode {
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }
}

#[derive(Debug, Clone)]
pub struct SecurityPaths {
    pub root: PathBuf,
    pub device_secret: PathBuf,
    pub trust_store: PathBuf,
}

impl SecurityPaths {
    pub fn for_root(root: PathBuf) -> Self {
        Self {
            device_secret: root.join("device.secret"),
            trust_store: root.join("trust-store.json"),
            root,
        }
    }
}

pub fn default_security_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("security")
}

pub fn generate_pairing_code(ttl: Duration) -> PairingCode {
    let rng = rand::rng();
    let raw: String = rng
        .sample_iter(&Alphanumeric)
        .map(char::from)
        .take(16)
        .collect::<String>()
        .to_uppercase();

    let groups = [0, 4, 8, 12]
        .iter()
        .map(|offset| &raw[*offset..offset + 4])
        .collect::<Vec<_>>()
        .join("-");

    PairingCode {
        value: groups,
        expires_at: Utc::now() + chrono::TimeDelta::from_std(ttl).unwrap_or_default(),
    }
}

pub fn load_or_create_device_secret(paths: &SecurityPaths) -> anyhow::Result<String> {
    fs::create_dir_all(&paths.root).with_context(|| format!("create {}", paths.root.display()))?;

    if paths.device_secret.exists() {
        return fs::read_to_string(&paths.device_secret)
            .map(|s| s.trim().to_string())
            .with_context(|| format!("read {}", paths.device_secret.display()));
    }

    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let value = base64::engine::general_purpose::STANDARD.encode(bytes);
    fs::write(&paths.device_secret, &value)
        .with_context(|| format!("write {}", paths.device_secret.display()))?;
    Ok(value)
}

pub fn fingerprint(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    hex_encode(digest)
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        output.push_str(&format!("{b:02x}"));
    }
    output
}

pub fn ensure_trust_store(paths: &SecurityPaths) -> anyhow::Result<()> {
    if !paths.trust_store.exists() {
        fs::write(&paths.trust_store, "[]")
            .with_context(|| format!("write {}", paths.trust_store.display()))?;
    }
    Ok(())
}

pub fn is_within_root(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_has_expected_shape() {
        let code = generate_pairing_code(Duration::from_secs(300));
        assert_eq!(code.value.len(), 19);
        assert!(code.value.contains('-'));
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(
            fingerprint("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
