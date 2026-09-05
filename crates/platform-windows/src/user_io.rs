//! User-selected filesystem operations must never inherit a service token.
//!
//! A lease pins one Windows logon token. Name resolution runs synchronously on
//! a blocking worker while impersonating that token, and reverts before the
//! worker returns (including unwinding). Opened handles may cross async code:
//! their granted access was checked against the user, not against SYSTEM.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Synchronous publication with atomic no-overwrite semantics. Call inside
/// `UserIoLease::run_sync`; the operation does not change thread authority.
pub fn publish_without_replace(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let source = source
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let destination = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        // No MOVEFILE_REPLACE_EXISTING: a concurrently-created destination wins.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("publish received user file");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // Reserved part and final file are in the same directory/filesystem.
        std::fs::hard_link(source, destination)
            .context("publish received user file without replacement")?;
        std::fs::remove_file(source).context("remove published user part")
    }
}

#[derive(Clone, Debug)]
pub struct UserIoLease {
    #[cfg(windows)]
    inner: std::sync::Arc<windows::Lease>,
}

impl UserIoLease {
    /// `Some` is the installed service's fixed allowed SID. It never means
    /// "whichever user happens to be logged on". Missing console authority is
    /// an error, not a reason to fall back to the process token.
    pub async fn capture(allowed_service_sid: Option<String>) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            #[cfg(windows)]
            return windows::capture(allowed_service_sid.as_deref());
            #[cfg(not(windows))]
            {
                if allowed_service_sid.is_some() {
                    anyhow::bail!("Windows service user I/O is unavailable on this platform");
                }
                Ok(Self {})
            }
        })
        .await
        .context("join user I/O authority capture")?
    }

    pub async fn validate(&self) -> Result<()> {
        self.run_sync(|| Ok(())).await
    }

    /// The closure must complete all name-based I/O synchronously. Never return
    /// a future which will perform I/O later under the process token.
    pub async fn run_sync<F, T>(&self, operation: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(windows)]
        let lease = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(windows)]
            let _impersonation = lease.enter()?;
            operation()
        })
        .await
        .context("join user I/O operation")?
    }

    pub async fn default_receive_dir(&self) -> Result<PathBuf> {
        #[cfg(windows)]
        {
            let lease = self.inner.clone();
            self.run_sync(move || lease.known_folder(false).map(|path| path.join("Boundless")))
                .await
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("user known folders are only resolved here on Windows")
        }
    }

    pub async fn default_diagnostics_dir(&self) -> Result<PathBuf> {
        #[cfg(windows)]
        {
            let lease = self.inner.clone();
            self.run_sync(move || {
                lease
                    .known_folder(true)
                    .map(|path| path.join("Boundless").join("diagnostics"))
            })
            .await
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("user known folders are only resolved here on Windows")
        }
    }

    pub async fn remove_file(&self, path: PathBuf) -> Result<()> {
        self.run_sync(move || std::fs::remove_file(path).context("remove user file"))
            .await
    }
}

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, mem, ptr};

    use anyhow::bail;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_NO_TOKEN, HANDLE},
        Security::{
            DuplicateTokenEx, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf,
            SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
            TOKEN_STATISTICS, TokenElevation, TokenImpersonation, TokenSessionId, TokenStatistics,
        },
        System::{
            Com::CoTaskMemFree,
            RemoteDesktop::WTSQueryUserToken,
            Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken},
        },
        UI::Shell::{FOLDERID_Downloads, FOLDERID_LocalAppData, SHGetKnownFolderPath},
    };

    use super::*;
    use crate::runtime::{active_console_session_id, process_handle_user_sid_string};

    #[derive(Debug)]
    pub(super) struct Token(HANDLE);

    // Windows token handles are process-wide and immutable here. Each operation
    // sets only its own worker thread's impersonation token.
    unsafe impl Send for Token {}
    unsafe impl Sync for Token {}

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct LogonIdentity {
        session: u32,
        authentication_id: (u32, i32),
    }

    #[derive(Debug)]
    pub(super) struct Lease {
        token: Token,
        service_binding: Option<(String, LogonIdentity)>,
        revoked: std::sync::atomic::AtomicBool,
    }

    pub(super) fn capture(allowed_service_sid: Option<&str>) -> Result<UserIoLease> {
        let lease = if let Some(sid) = allowed_service_sid {
            let (token, identity) = console_user_token(sid)?;
            Lease {
                token: impersonation_token(&token)?,
                service_binding: Some((sid.to_string(), identity)),
                revoked: std::sync::atomic::AtomicBool::new(false),
            }
        } else {
            let process = unsafe { GetCurrentProcess() };
            if process_handle_user_sid_string(process)? == "S-1-5-18" {
                bail!("service user I/O requires a configured console-user authority");
            }
            let mut token = ptr::null_mut();
            if unsafe { OpenProcessToken(process, TOKEN_DUPLICATE | TOKEN_QUERY, &mut token) } == 0
            {
                return Err(std::io::Error::last_os_error()).context("open user process token");
            }
            Lease {
                token: impersonation_token(&Token(token))?,
                service_binding: None,
                revoked: std::sync::atomic::AtomicBool::new(false),
            }
        };
        Ok(UserIoLease {
            inner: std::sync::Arc::new(lease),
        })
    }

    fn token_value<T: Copy>(token: HANDLE, class: i32) -> Result<T> {
        let mut value = mem::MaybeUninit::<T>::uninit();
        let mut returned = 0;
        if unsafe {
            GetTokenInformation(
                token,
                class,
                value.as_mut_ptr().cast::<c_void>(),
                mem::size_of::<T>() as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("query user token");
        }
        if returned != mem::size_of::<T>() as u32 {
            bail!("unexpected user token information size");
        }
        Ok(unsafe { value.assume_init() })
    }

    fn console_user_token(allowed_sid: &str) -> Result<(Token, LogonIdentity)> {
        let session = active_console_session_id().context("no active console user for file I/O")?;
        let mut token = ptr::null_mut();
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("resolve configured console user token");
        }
        let token = Token(token);
        if crate::runtime::token_user_sid_string(token.0)? != allowed_sid {
            bail!("console user does not match the installed allowed user");
        }
        if token_value::<u32>(token.0, TokenSessionId)? != session
            || active_console_session_id() != Some(session)
        {
            bail!("console session changed while resolving user I/O authority");
        }
        if token_value::<windows_sys::Win32::Security::TOKEN_ELEVATION>(token.0, TokenElevation)?
            .TokenIsElevated
            != 0
        {
            bail!("service file I/O requires the unelevated desktop token");
        }
        let statistics = token_value::<TOKEN_STATISTICS>(token.0, TokenStatistics)?;
        Ok((
            token,
            LogonIdentity {
                session,
                authentication_id: (
                    statistics.AuthenticationId.LowPart,
                    statistics.AuthenticationId.HighPart,
                ),
            },
        ))
    }

    fn impersonation_token(token: &Token) -> Result<Token> {
        let mut duplicate = ptr::null_mut();
        if unsafe {
            DuplicateTokenEx(
                token.0,
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                ptr::null(),
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicate,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("duplicate user I/O token");
        }
        Ok(Token(duplicate))
    }

    impl Lease {
        pub(super) fn enter(&self) -> Result<Impersonation> {
            use std::sync::atomic::Ordering;
            if self.revoked.load(Ordering::Acquire) {
                bail!("user I/O authority has been revoked");
            }
            if let Some((sid, expected)) = &self.service_binding {
                let actual = console_user_token(sid);
                if !actual.as_ref().is_ok_and(|(_, actual)| actual == expected) {
                    self.revoked.store(true, Ordering::Release);
                    bail!("user I/O authority expired after Windows logon/session change");
                }
            }
            enter_token(&self.token)
        }

        pub(super) fn known_folder(&self, local_data: bool) -> Result<PathBuf> {
            use std::os::windows::ffi::OsStringExt;
            let mut value = ptr::null_mut();
            let folder = if local_data {
                &FOLDERID_LocalAppData
            } else {
                &FOLDERID_Downloads
            };
            let result = unsafe { SHGetKnownFolderPath(folder, 0, self.token.0, &mut value) };
            if result < 0 {
                bail!(
                    "resolve user known folder failed: HRESULT 0x{:08x}",
                    result as u32
                );
            }
            if value.is_null() {
                bail!("user known folder was empty");
            }
            let mut len = 0;
            while unsafe { *value.add(len) } != 0 {
                len += 1;
            }
            let path = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
                std::slice::from_raw_parts(value, len)
            }));
            unsafe { CoTaskMemFree(value.cast()) };
            Ok(path)
        }
    }

    fn enter_token(token: &Token) -> Result<Impersonation> {
        // A reusable worker must begin under its process token. Never overwrite
        // another component's thread impersonation or leave one behind.
        let mut prior = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut prior) } != 0 {
            drop(Token(prior));
            bail!("user I/O worker already has an impersonation token");
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
            return Err(std::io::Error::last_os_error()).context("check user I/O worker token");
        }
        if unsafe { ImpersonateLoggedOnUser(token.0) } == 0 {
            return Err(std::io::Error::last_os_error()).context("enter user I/O authority");
        }
        Ok(Impersonation)
    }

    pub(super) struct Impersonation;

    impl Drop for Impersonation {
        fn drop(&mut self) {
            if unsafe { RevertToSelf() } == 0 {
                // Continuing would lend this user's token to unrelated work on
                // the reused thread. A failed revert cannot safely recover.
                std::process::abort();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::{
                Authorization::{
                    ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT,
                    SetNamedSecurityInfoW,
                },
                CreateRestrictedToken, CreateWellKnownSid, DACL_SECURITY_INFORMATION,
                DISABLE_MAX_PRIVILEGE, GetSecurityDescriptorDacl,
                PROTECTED_DACL_SECURITY_INFORMATION, SID_AND_ATTRIBUTES, WinRestrictedCodeSid,
            },
        };

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let path = std::env::temp_dir()
                    .join(format!("boundless-user-io-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir(&path).expect("create disposable fixture");
                set_fixture_acl(&path, true);
                Self(path)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn set_fixture_acl(path: &Path, restricted_allowed: bool) {
            let sid = crate::runtime::current_user_sid_string().expect("fixture owner");
            let restricted = if restricted_allowed {
                "(A;OICI;FA;;;RC)"
            } else {
                ""
            };
            let sddl = format!("D:P(A;OICI;FA;;;{sid}){restricted}");
            let wide = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            let mut descriptor = ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        wide.as_ptr(),
                        1,
                        &mut descriptor,
                        ptr::null_mut(),
                    )
                },
                0
            );
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl = ptr::null_mut();
            assert_ne!(
                unsafe {
                    GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
                },
                0
            );
            let name = path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let result = unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                )
            };
            unsafe { LocalFree(descriptor) };
            assert_eq!(result, 0, "set fixture-only DACL");
        }

        fn restricted_lease() -> UserIoLease {
            let mut process_token = ptr::null_mut();
            assert_ne!(
                unsafe {
                    OpenProcessToken(
                        GetCurrentProcess(),
                        TOKEN_DUPLICATE | TOKEN_QUERY,
                        &mut process_token,
                    )
                },
                0
            );
            let process_token = Token(process_token);
            let mut sid = [0u64; 12];
            let mut sid_len = mem::size_of_val(&sid) as u32;
            assert_ne!(
                unsafe {
                    CreateWellKnownSid(
                        WinRestrictedCodeSid,
                        ptr::null_mut(),
                        sid.as_mut_ptr().cast(),
                        &mut sid_len,
                    )
                },
                0
            );
            let restriction = SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: 0,
            };
            let mut restricted = ptr::null_mut();
            assert_ne!(
                unsafe {
                    CreateRestrictedToken(
                        process_token.0,
                        DISABLE_MAX_PRIVILEGE,
                        0,
                        ptr::null(),
                        0,
                        ptr::null(),
                        1,
                        &restriction,
                        &mut restricted,
                    )
                },
                0
            );
            let restricted = Token(restricted);
            UserIoLease {
                inner: std::sync::Arc::new(Lease {
                    token: impersonation_token(&restricted).expect("duplicate restricted token"),
                    service_binding: None,
                    revoked: std::sync::atomic::AtomicBool::new(false),
                }),
            }
        }

        fn thread_is_impersonating() -> bool {
            let mut token = ptr::null_mut();
            let result = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
            if result != 0 {
                drop(Token(token));
                true
            } else {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(ERROR_NO_TOKEN as i32)
                );
                false
            }
        }

        #[tokio::test]
        async fn restricted_user_io_does_not_borrow_host_file_rights() {
            let fixture = Fixture::new();
            let protected = fixture.0.join("host-only");
            std::fs::create_dir(&protected).expect("protected fixture directory");
            set_fixture_acl(&protected, false);
            let denied = protected.join("private-fixture.txt");
            std::fs::write(&denied, b"harmless protected fixture").expect("host writes fixture");
            let lease = restricted_lease();
            let path = denied.clone();
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::read(path)?))
                    .await
                    .is_err()
            );
            let path = protected.join("export.txt");
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::write(path, b"export")?))
                    .await
                    .is_err()
            );
            let path = protected.join("receive");
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::create_dir_all(path)?))
                    .await
                    .is_err()
            );
            assert!(lease.remove_file(denied.clone()).await.is_err());
            assert_eq!(
                std::fs::read(denied).expect("host still reads original"),
                b"harmless protected fixture"
            );

            let allowed = fixture.0.join("received.part");
            let path = allowed.clone();
            lease
                .run_sync(move || Ok(std::fs::write(path, b"user content")?))
                .await
                .expect("allowed user write");
            let source = allowed.clone();
            let destination = protected.join("received.txt");
            assert!(
                lease
                    .run_sync(move || publish_without_replace(&source, &destination))
                    .await
                    .is_err()
            );
            let source = allowed.clone();
            let destination = fixture.0.join("received.txt");
            lease
                .run_sync(move || publish_without_replace(&source, &destination))
                .await
                .expect("allowed publication");
        }

        #[tokio::test]
        async fn opened_user_handle_is_not_reopened_after_source_path_replacement() {
            use std::io::Read;
            let fixture = Fixture::new();
            let source = fixture.0.join("source.txt");
            std::fs::write(&source, b"original user data").expect("source fixture");
            let lease = restricted_lease();
            let path = source.clone();
            let mut opened = lease
                .run_sync(move || Ok(std::fs::File::open(path)?))
                .await
                .expect("user opens handle");
            std::fs::rename(&source, fixture.0.join("renamed.txt")).expect("rename source");
            std::fs::write(&source, b"replacement").expect("replacement fixture");
            set_fixture_acl(&source, false);
            let mut content = String::new();
            opened
                .read_to_string(&mut content)
                .expect("read retained handle");
            assert_eq!(content, "original user data");
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::read(source)?))
                    .await
                    .is_err()
            );
        }

        fn fixture_junction(link: &Path, target: &Path) {
            use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
            use windows_sys::Win32::System::IO::DeviceIoControl;
            std::fs::create_dir(link).expect("junction fixture directory");
            let target = std::fs::canonicalize(target).expect("fixture target");
            let target = target.to_string_lossy();
            let print = target.strip_prefix(r"\\?\").unwrap_or(&target);
            let substitute = format!(r"\??\{print}").encode_utf16().collect::<Vec<_>>();
            let print = print.encode_utf16().collect::<Vec<_>>();
            let mut data = Vec::new();
            data.extend_from_slice(&0xa0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
            data.extend_from_slice(
                &(8u16 + ((substitute.len() + print.len() + 2) * 2) as u16).to_le_bytes(),
            );
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
            data.extend_from_slice(&(((substitute.len() + 1) * 2) as u16).to_le_bytes());
            data.extend_from_slice(&((print.len() * 2) as u16).to_le_bytes());
            for unit in substitute
                .into_iter()
                .chain(Some(0))
                .chain(print)
                .chain(Some(0))
            {
                data.extend_from_slice(&unit.to_le_bytes());
            }
            let handle = std::fs::OpenOptions::new()
                .access_mode(0x40000000)
                .custom_flags(0x02000000 | 0x00200000)
                .open(link)
                .expect("open fixture junction");
            let mut returned = 0;
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        handle.as_raw_handle(),
                        0x000900a4,
                        data.as_ptr().cast(),
                        data.len() as u32,
                        ptr::null_mut(),
                        0,
                        &mut returned,
                        ptr::null_mut(),
                    )
                },
                0,
                "create fixture junction: {}",
                std::io::Error::last_os_error()
            );
        }

        #[tokio::test]
        async fn junction_parent_resolution_still_uses_user_acl_for_read_write_and_publish() {
            let fixture = Fixture::new();
            let protected = fixture.0.join("protected");
            std::fs::create_dir(&protected).expect("protected fixture");
            std::fs::write(protected.join("source.txt"), b"harmless fixture")
                .expect("fixture source");
            set_fixture_acl(&protected, false);
            set_fixture_acl(&protected.join("source.txt"), false);
            let junction = fixture.0.join("redirected");
            fixture_junction(&junction, &protected);
            let lease = restricted_lease();
            let path = junction.join("source.txt");
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::read(path)?))
                    .await
                    .is_err()
            );
            let path = junction.join("new.txt");
            assert!(
                lease
                    .run_sync(move || Ok(std::fs::write(path, b"denied")?))
                    .await
                    .is_err()
            );
            let source = fixture.0.join("source.part");
            std::fs::write(&source, b"incoming fixture").expect("part fixture");
            let destination = junction.join("final.txt");
            assert!(
                lease
                    .run_sync(move || publish_without_replace(&source, &destination))
                    .await
                    .is_err()
            );
            assert!(!protected.join("new.txt").exists());
            assert!(!protected.join("final.txt").exists());
            std::fs::remove_dir(junction).expect("remove fixture junction itself");
        }

        #[test]
        fn impersonation_reverts_on_error_and_panic_on_the_same_worker_thread() {
            std::thread::spawn(|| {
                let lease = restricted_lease();
                assert!(!thread_is_impersonating());
                let error = (|| -> Result<()> {
                    let _guard = lease.inner.enter()?;
                    assert!(thread_is_impersonating());
                    bail!("fixture operation failed")
                })();
                assert!(error.is_err());
                assert!(!thread_is_impersonating());
                let panicked = std::panic::catch_unwind(|| {
                    let _guard = lease.inner.enter().expect("enter fixture token");
                    assert!(thread_is_impersonating());
                    panic!("fixture unwind");
                });
                assert!(panicked.is_err());
                assert!(!thread_is_impersonating());
            })
            .join()
            .expect("worker completes");
        }

        #[tokio::test]
        async fn missing_service_user_authority_never_falls_back_to_process_rights() {
            assert!(
                UserIoLease::capture(Some("S-1-5-21-999999-999999-999999-999999".to_string()))
                    .await
                    .is_err()
            );
        }

        #[tokio::test]
        async fn expired_service_authority_revokes_all_clones_before_operations_run() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let mut lease = restricted_lease();
            std::sync::Arc::get_mut(&mut lease.inner)
                .unwrap()
                .service_binding = Some((
                "S-1-5-21-999999-999999-999999-999999".to_string(),
                LogonIdentity {
                    session: 7,
                    authentication_id: (0, 0),
                },
            ));
            let clone = lease.clone();
            assert!(lease.validate().await.is_err());
            assert!(clone.inner.revoked.load(Ordering::Acquire));
            let ran = std::sync::Arc::new(AtomicBool::new(false));
            let observed = ran.clone();
            assert!(
                clone
                    .run_sync(move || {
                        observed.store(true, Ordering::Release);
                        Ok(())
                    })
                    .await
                    .is_err()
            );
            assert!(!ran.load(Ordering::Acquire));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_publication_never_replaces_a_concurrently_created_file() {
        let root = std::env::temp_dir().join(format!("boundless-publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).expect("fixture");
        let source = root.join("source.part");
        let destination = root.join("destination.txt");
        std::fs::write(&source, b"incoming").expect("incoming fixture");
        std::fs::write(&destination, b"existing").expect("existing fixture");
        assert!(publish_without_replace(&source, &destination).is_err());
        assert_eq!(
            std::fs::read(&destination).expect("destination"),
            b"existing"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
