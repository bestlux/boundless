use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

// Configurations contain a handful of peers/settings, never clipboard/file data.
// This also caps the memory and disk amplification of a migration backup.
pub(super) const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn validate_path(path: &Path) -> Result<()> {
    for ancestor in path.ancestors().filter(|item| !item.as_os_str().is_empty()) {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if is_link(&metadata) {
                    bail!("configuration paths must not contain links or reparse points");
                }
                if ancestor == path && !metadata.is_file() {
                    bail!("configuration must be an ordinary file");
                }
                if ancestor != path && !metadata.is_dir() {
                    bail!("configuration parent must be an ordinary directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect configuration path"),
        }
    }
    Ok(())
}

pub(super) fn read_bounded_config(path: &Path) -> Result<Vec<u8>> {
    validate_path(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Do not follow a replaced leaf or allow its contents to change mid-read.
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(path).context("open configuration file")?;
    let metadata = file.metadata().context("inspect opened configuration")?;
    if !metadata.is_file() || is_link(&metadata) {
        bail!("configuration must be an ordinary file");
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!("configuration exceeds the 4 MiB size limit");
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read bounded configuration")?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("configuration exceeds the 4 MiB size limit");
    }
    Ok(bytes)
}

pub(super) fn backup_path(path: &Path) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .context("configuration needs a file name")?
        .to_os_string();
    name.push(".pre-v7.bak");
    Ok(path.with_file_name(name))
}

pub(super) fn create_once(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        bail!("configuration backup exceeds the 4 MiB size limit");
    }
    let backup = backup_path(path)?;
    validate_path(&backup)?;
    if backup.exists() {
        // An earlier complete backup remains the rollback point. Never replace it.
        let existing = read_bounded_config(&backup)?;
        let _: serde_json::Value = serde_json::from_slice(&existing)
            .context("existing pre-upgrade configuration backup is invalid; preserve or restore it before retrying")?;
        return Ok(());
    }
    // One fixed staging name also bounds leftovers across crashes. A partial
    // backup requires explicit recovery instead of another growing set of copies.
    let mut pending_name = backup
        .file_name()
        .context("backup needs a file name")?
        .to_os_string();
    pending_name.push(".pending");
    let temp = backup.with_file_name(pending_name);
    validate_path(&temp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .context("create configuration backup staging file; a prior .pre-v7.bak.pending file must be preserved or removed before retrying")?;
    let written = (|| -> Result<()> {
        file.write_all(bytes)
            .context("write configuration backup")?;
        file.sync_all().context("sync configuration backup")?;
        Ok(())
    })();
    drop(file);
    let published = written.and_then(|()| {
        // Atomic create-if-absent publication: a crash cannot leave a partial
        // create-once backup and an existing rollback file is never overwritten.
        fs::hard_link(&temp, &backup).context("publish pre-upgrade configuration backup")
    });
    let cleanup = fs::remove_file(&temp).context("remove configuration backup staging file");
    published?;
    cleanup?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;

    #[test]
    fn migration_rejects_junction_ancestry_without_touching_the_target() {
        let root = std::env::temp_dir().join(format!(
            "boundless-config-junction-{}",
            uuid::Uuid::new_v4()
        ));
        let target = root.join("target");
        let link = root.join("redirected");
        let target_config = target.join("config.json");
        let config = crate::config::RuntimeConfig {
            config_version: "6".into(),
            network_port: 15100,
            ..Default::default()
        };
        crate::config::save_config_at(&target_config, &config).unwrap();
        let original = fs::read(&target_config).unwrap();
        let created = std::process::Command::new("powershell.exe")
            .creation_flags(0x0800_0000)
            .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:BOUNDLESS_FIXTURE_LINK -Target $env:BOUNDLESS_FIXTURE_TARGET | Out-Null"])
            .env("BOUNDLESS_FIXTURE_LINK", &link)
            .env("BOUNDLESS_FIXTURE_TARGET", &target)
            .output().expect("create local junction fixture");
        assert!(
            created.status.success(),
            "junction fixture: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let error = crate::config::load_or_create_config_at(&link.join("config.json")).unwrap_err();
        assert!(error.to_string().contains("reparse points"));
        assert_eq!(fs::read(&target_config).unwrap(), original);
        assert!(!backup_path(&target_config).unwrap().exists());
        // Remove only the junction itself before disposing of the unique fixture.
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
