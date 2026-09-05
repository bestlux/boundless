use super::*;
use platform_windows::user_io::UserIoLease;

impl AppState {
    pub(crate) async fn user_io_lease(&self) -> Result<UserIoLease> {
        let allowed_sid = if self.clipboard_uses_broker() {
            Some(
                self.input_broker
                    .allowed_user_sid()
                    .context("service user I/O has no configured allowed user")?,
            )
        } else {
            None
        };
        UserIoLease::capture(allowed_sid).await
    }

    pub(crate) async fn ensure_file_transfer_enabled(&self) -> Result<()> {
        if !self
            .config
            .read()
            .await
            .features
            .get("transfer_file")
            .copied()
            .unwrap_or(false)
        {
            anyhow::bail!("file transfer is disabled");
        }
        Ok(())
    }

    pub(crate) async fn user_receive_dir(
        &self,
        lease: &UserIoLease,
        configured: String,
    ) -> Result<PathBuf> {
        #[cfg(windows)]
        if self.clipboard_uses_broker() && configured == crate::config::default_file_receive_dir() {
            // Old service configs persisted SYSTEM's known-folder default. Map
            // only that default to the fixed allowed user's folder; never move
            // existing files or rewrite user-selected destinations silently.
            return lease.default_receive_dir().await;
        }
        let _ = lease;
        Ok(PathBuf::from(configured))
    }
}
