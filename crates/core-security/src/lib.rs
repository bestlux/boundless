use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::Context;
use base64::Engine;
use chrono::{DateTime, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustRecord {
    pub machine_id: String,
    pub ca_cert_pem: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustBundle {
    pub machine_id: String,
    pub display_name: String,
    pub network_address: String,
    pub ca_cert_pem: String,
}

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub machine_id: String,
    pub display_name: String,
    pub ca_cert_pem: String,
    pub device_cert_pem: String,
    pub device_key_pem: String,
}

#[derive(Debug, Clone)]
pub struct SecurityPaths {
    pub root: PathBuf,
    pub device_secret: PathBuf,
    pub trust_store: PathBuf,
    pub ca_cert_pem: PathBuf,
    pub ca_key_pem: PathBuf,
    pub device_cert_pem: PathBuf,
    pub device_key_pem: PathBuf,
}

impl SecurityPaths {
    pub fn for_root(root: PathBuf) -> Self {
        Self {
            device_secret: root.join("device.secret"),
            trust_store: root.join("trust-store.json"),
            ca_cert_pem: root.join("ca-cert.pem"),
            ca_key_pem: root.join("ca-key.pem"),
            device_cert_pem: root.join("device-cert.pem"),
            device_key_pem: root.join("device-key.pem"),
            root,
        }
    }
}

pub fn default_security_root() -> PathBuf {
    if let Ok(path) = std::env::var("BOUNDLESS_SECURITY_ROOT") {
        return PathBuf::from(path);
    }

    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Boundless")
        .join("security")
}

pub fn generate_pairing_code(ttl: Duration) -> PairingCode {
    let value = format!("{:06}", rand::random_range(0..1_000_000));

    PairingCode {
        value,
        expires_at: Utc::now() + chrono::TimeDelta::from_std(ttl).unwrap_or_default(),
    }
}

pub fn atomic_write_file(path: &Path, contents: impl AsRef<[u8]>) -> anyhow::Result<()> {
    let contents = contents.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .context("atomic write target must include a file name")?;

    for _ in 0..16 {
        let counter = ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.boundless-tmp-{}-{counter}",
            file_name.to_string_lossy(),
            std::process::id()
        ));

        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create temp {}", temp_path.display()));
            }
        };

        let write_result = (|| -> anyhow::Result<()> {
            file.write_all(contents)
                .with_context(|| format!("write temp {}", temp_path.display()))?;
            file.flush()
                .with_context(|| format!("flush temp {}", temp_path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temp {}", temp_path.display()))?;
            Ok(())
        })();
        drop(file);

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        if let Err(error) = replace_file(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!("replace {} with {}", path.display(), temp_path.display())
            });
        }

        sync_parent_dir(parent);
        return Ok(());
    }

    anyhow::bail!("create temp file for {}", path.display());
}

#[cfg(windows)]
fn replace_file(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let temp_path_wide = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // SAFETY: both buffers are null-terminated UTF-16 paths and live for the call.
    let moved = unsafe {
        MoveFileExW(
            temp_path_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temp_path, path)
}

fn sync_parent_dir(parent: &Path) {
    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    #[cfg(not(unix))]
    let _ = parent;
}

pub fn load_or_create_device_secret(paths: &SecurityPaths) -> anyhow::Result<String> {
    fs::create_dir_all(&paths.root).with_context(|| format!("create {}", paths.root.display()))?;

    if paths.device_secret.exists() {
        return fs::read_to_string(&paths.device_secret)
            .map(|s| s.trim().to_string())
            .with_context(|| format!("read {}", paths.device_secret.display()));
    }

    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let value = base64::engine::general_purpose::STANDARD.encode(bytes);
    atomic_write_file(&paths.device_secret, &value)
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
    fs::create_dir_all(&paths.root).with_context(|| format!("create {}", paths.root.display()))?;

    if !paths.trust_store.exists() {
        atomic_write_file(&paths.trust_store, "[]")
            .with_context(|| format!("write {}", paths.trust_store.display()))?;
    }
    Ok(())
}

pub fn load_trust_records(paths: &SecurityPaths) -> anyhow::Result<Vec<TrustRecord>> {
    ensure_trust_store(paths)?;
    let raw = fs::read_to_string(&paths.trust_store)
        .with_context(|| format!("read {}", paths.trust_store.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse trust store {}", paths.trust_store.display()))
}

pub fn upsert_trust_record(paths: &SecurityPaths, record: TrustRecord) -> anyhow::Result<()> {
    let mut records = load_trust_records(paths)?;

    if let Some(existing) = records
        .iter_mut()
        .find(|it| it.machine_id == record.machine_id)
    {
        *existing = record;
    } else {
        records.push(record);
    }

    let payload = serde_json::to_string_pretty(&records).context("serialize trust store")?;
    atomic_write_file(&paths.trust_store, payload)
        .with_context(|| format!("write {}", paths.trust_store.display()))?;
    Ok(())
}

pub fn remove_trust_record(paths: &SecurityPaths, machine_id: &str) -> anyhow::Result<bool> {
    let mut records = load_trust_records(paths)?;
    let before = records.len();
    records.retain(|record| record.machine_id != machine_id);
    let removed = before != records.len();
    if !removed {
        return Ok(false);
    }

    let payload = serde_json::to_string_pretty(&records).context("serialize trust store")?;
    atomic_write_file(&paths.trust_store, payload)
        .with_context(|| format!("write {}", paths.trust_store.display()))?;
    Ok(true)
}

pub fn ensure_device_identity(
    paths: &SecurityPaths,
    machine_id: &str,
    display_name: &str,
    advertised_host: Option<&str>,
) -> anyhow::Result<DeviceIdentity> {
    fs::create_dir_all(&paths.root).with_context(|| format!("create {}", paths.root.display()))?;

    if paths.ca_cert_pem.exists()
        && paths.ca_key_pem.exists()
        && paths.device_cert_pem.exists()
        && paths.device_key_pem.exists()
    {
        return Ok(DeviceIdentity {
            machine_id: machine_id.to_string(),
            display_name: display_name.to_string(),
            ca_cert_pem: fs::read_to_string(&paths.ca_cert_pem)
                .with_context(|| format!("read {}", paths.ca_cert_pem.display()))?,
            device_cert_pem: fs::read_to_string(&paths.device_cert_pem)
                .with_context(|| format!("read {}", paths.device_cert_pem.display()))?,
            device_key_pem: fs::read_to_string(&paths.device_key_pem)
                .with_context(|| format!("read {}", paths.device_key_pem.display()))?,
        });
    }

    let ca_key = KeyPair::generate().context("generate CA key")?;

    let mut ca_params = CertificateParams::new(vec![]).context("create CA params")?;
    ca_params.distinguished_name.push(
        DnType::CommonName,
        format!("Boundless {machine_id} Root CA"),
    );
    ca_params
        .distinguished_name
        .push(DnType::OrganizationName, "Boundless");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];

    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("create self-signed CA cert")?;

    let mut subject_alt_names = vec![
        display_name.to_string(),
        machine_id.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ];
    if let Some(host) = advertised_host
        && !host.trim().is_empty()
    {
        subject_alt_names.push(host.trim().to_string());
    }

    subject_alt_names.sort();
    subject_alt_names.dedup();

    let mut leaf_params =
        CertificateParams::new(subject_alt_names).context("create leaf params")?;
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, display_name.to_string());
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);
    leaf_params
        .key_usages
        .push(KeyUsagePurpose::DigitalSignature);

    let leaf_key = KeyPair::generate().context("generate leaf key")?;
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &issuer)
        .context("create leaf cert signed by CA")?;

    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = issuer.key().serialize_pem();
    let device_cert_pem = leaf_cert.pem();
    let device_key_pem = leaf_key.serialize_pem();

    atomic_write_file(&paths.ca_cert_pem, &ca_cert_pem)
        .with_context(|| format!("write {}", paths.ca_cert_pem.display()))?;
    atomic_write_file(&paths.ca_key_pem, &ca_key_pem)
        .with_context(|| format!("write {}", paths.ca_key_pem.display()))?;
    atomic_write_file(&paths.device_cert_pem, &device_cert_pem)
        .with_context(|| format!("write {}", paths.device_cert_pem.display()))?;
    atomic_write_file(&paths.device_key_pem, &device_key_pem)
        .with_context(|| format!("write {}", paths.device_key_pem.display()))?;

    Ok(DeviceIdentity {
        machine_id: machine_id.to_string(),
        display_name: display_name.to_string(),
        ca_cert_pem,
        device_cert_pem,
        device_key_pem,
    })
}

pub fn rotate_device_identity(
    paths: &SecurityPaths,
    machine_id: &str,
    display_name: &str,
    advertised_host: Option<&str>,
) -> anyhow::Result<DeviceIdentity> {
    fs::create_dir_all(&paths.root).with_context(|| format!("create {}", paths.root.display()))?;
    for path in [
        &paths.device_secret,
        &paths.ca_cert_pem,
        &paths.ca_key_pem,
        &paths.device_cert_pem,
        &paths.device_key_pem,
    ] {
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    atomic_write_file(&paths.trust_store, "[]")
        .with_context(|| format!("write {}", paths.trust_store.display()))?;
    let _ = load_or_create_device_secret(paths)?;
    ensure_device_identity(paths, machine_id, display_name, advertised_host)
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
        assert_eq!(code.value.len(), 6);
        assert!(code.value.chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(
            fingerprint("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn atomic_write_file_replaces_existing_file_without_temp_leftovers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");

        atomic_write_file(&path, "one").expect("initial write");
        atomic_write_file(&path, "two").expect("replacement write");

        assert_eq!(std::fs::read_to_string(&path).expect("read state"), "two");
        let leftovers = std::fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("boundless-tmp")
            })
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "atomic writes should not leave temp files after success"
        );
    }

    #[test]
    fn atomic_write_file_cleans_temp_file_when_replace_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target");
        std::fs::create_dir(&path).expect("create target directory");

        let error = atomic_write_file(&path, "payload").expect_err("replace over directory fails");
        assert!(
            error.to_string().contains("replace"),
            "unexpected error: {error:#}"
        );

        let leftovers = std::fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("boundless-tmp")
            })
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "atomic write should clean temp files after replace failure"
        );
    }

    #[test]
    fn generates_identity_and_trust_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SecurityPaths::for_root(dir.path().join("security"));

        ensure_trust_store(&paths).expect("trust store");
        let identity =
            ensure_device_identity(&paths, "machine-1", "Machine One", Some("127.0.0.1"))
                .expect("identity");

        assert!(identity.ca_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(identity.device_key_pem.contains("BEGIN"));
        assert!(paths.trust_store.exists());
    }

    #[test]
    fn remove_trust_record_removes_matching_machine_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SecurityPaths::for_root(dir.path().join("security"));
        ensure_trust_store(&paths).expect("trust store");

        upsert_trust_record(
            &paths,
            TrustRecord {
                machine_id: "peer-a".to_string(),
                ca_cert_pem: "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----"
                    .to_string(),
                added_at: Utc::now(),
            },
        )
        .expect("insert peer-a");

        let removed = remove_trust_record(&paths, "peer-a").expect("remove peer-a");
        assert!(removed, "record should be removed");

        let records = load_trust_records(&paths).expect("read trust records");
        assert!(
            records.iter().all(|record| record.machine_id != "peer-a"),
            "removed machine id should not be present"
        );
    }

    #[test]
    fn rotate_device_identity_clears_trust_and_changes_ca() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SecurityPaths::for_root(dir.path().join("security"));
        let first = ensure_device_identity(&paths, "machine-1", "Machine One", Some("127.0.0.1"))
            .expect("identity");
        upsert_trust_record(
            &paths,
            TrustRecord {
                machine_id: "peer-a".to_string(),
                ca_cert_pem: "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----"
                    .to_string(),
                added_at: Utc::now(),
            },
        )
        .expect("insert trust");

        let second = rotate_device_identity(&paths, "machine-1", "Machine One", Some("127.0.0.1"))
            .expect("rotate identity");

        assert_ne!(first.ca_cert_pem, second.ca_cert_pem);
        assert!(
            load_trust_records(&paths)
                .expect("trust records")
                .is_empty()
        );
    }
}
