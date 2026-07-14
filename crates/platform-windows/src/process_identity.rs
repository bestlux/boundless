use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, MAX_PATH, TRUST_E_NOSIGNATURE},
    Security::{
        Authorization::ConvertSidToStringSidW,
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_ELEVATION,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenIntegrityLevel,
        TokenSessionId, TokenUser,
        WinTrust::{
            WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
            WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_IGNORE, WTD_UI_NONE,
            WinVerifyTrustEx,
        },
    },
    System::{
        SystemServices::{SECURITY_MANDATORY_HIGH_RID, SECURITY_MANDATORY_MEDIUM_RID},
        Threading::{
            GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
    },
    UI::Shell::{CSIDL_PROGRAM_FILES, SHGFP_TYPE_CURRENT, SHGetFolderPathW},
};
use windows_sys::core::PCWSTR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageTrustState {
    Valid,
    UnsignedDogfood,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsProcessIdentity {
    pub process_id: u32,
    pub user_sid: String,
    pub session_id: u32,
    pub integrity_rid: u32,
    pub elevated: bool,
    pub image_path: PathBuf,
    pub image_trust: ImageTrustState,
}

impl WindowsProcessIdentity {
    pub fn is_medium_unelevated(&self) -> bool {
        !self.elevated && self.integrity_rid == SECURITY_MANDATORY_MEDIUM_RID as u32
    }

    pub fn is_high_elevated(&self) -> bool {
        self.elevated
            && self.integrity_rid == SECURITY_MANDATORY_HIGH_RID as u32
            && self.user_sid != "S-1-5-18"
    }
}

pub fn current_process_identity() -> Result<WindowsProcessIdentity> {
    process_identity(unsafe { GetCurrentProcessId() })
}

pub fn process_identity(process_id: u32) -> Result<WindowsProcessIdentity> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error()).context("open process for identity");
    }
    let process = OwnedHandle(process);
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("open process token for identity");
    }
    let token = OwnedHandle(token);
    let image_path = process_image_path(process.0)?;
    Ok(WindowsProcessIdentity {
        process_id,
        user_sid: token_user_sid(token.0)?,
        session_id: token_scalar::<u32>(token.0, TokenSessionId)?,
        integrity_rid: token_integrity_rid(token.0)?,
        elevated: token_scalar::<TOKEN_ELEVATION>(token.0, TokenElevation)?.TokenIsElevated != 0,
        image_trust: authenticode_trust(&image_path),
        image_path,
    })
}

pub fn expected_boundless_image(file_name: &str) -> Result<PathBuf> {
    if file_name.is_empty() || file_name.contains(['/', '\\']) {
        bail!("installed image name must be one file name");
    }
    let mut buffer = vec![0u16; MAX_PATH as usize];
    let result = unsafe {
        SHGetFolderPathW(
            ptr::null_mut(),
            CSIDL_PROGRAM_FILES as i32,
            ptr::null_mut(),
            SHGFP_TYPE_CURRENT as u32,
            buffer.as_mut_ptr(),
        )
    };
    if result < 0 {
        bail!(
            "resolve Program Files failed with HRESULT 0x{:08x}",
            result as u32
        );
    }
    let len = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len]))
        .join("Boundless")
        .join(file_name))
}

pub fn canonical_paths_equal(left: &Path, right: &Path) -> Result<bool> {
    let left =
        std::fs::canonicalize(left).with_context(|| format!("canonicalize {}", left.display()))?;
    let right = std::fs::canonicalize(right)
        .with_context(|| format!("canonicalize {}", right.display()))?;
    Ok(normalized_path(&left) == normalized_path(&right))
}

pub fn validate_injector_pair(
    tray: &WindowsProcessIdentity,
    helper: &WindowsProcessIdentity,
    expected_tray_pid: u32,
) -> Result<()> {
    validate_injector_identity_fields(tray, helper, expected_tray_pid)?;
    if !canonical_paths_equal(
        &tray.image_path,
        &expected_boundless_image("boundlesstray.exe")?,
    )? {
        bail!("tray image was not the canonical Program Files installation");
    }
    if !canonical_paths_equal(
        &helper.image_path,
        &expected_boundless_image("boundless-input-injector.exe")?,
    )? {
        bail!("injector image was not the canonical Program Files installation");
    }
    Ok(())
}

fn validate_injector_identity_fields(
    tray: &WindowsProcessIdentity,
    helper: &WindowsProcessIdentity,
    expected_tray_pid: u32,
) -> Result<()> {
    if tray.process_id != expected_tray_pid {
        bail!("tray process id did not match the launch origin");
    }
    if tray.session_id == 0 || helper.session_id == 0 || tray.session_id != helper.session_id {
        bail!("injector requires one matching interactive Windows session");
    }
    if tray.user_sid != helper.user_sid {
        bail!("credential elevation to another administrator is unsupported");
    }
    if !tray.is_medium_unelevated() {
        bail!("injector origin must be an unelevated tray");
    }
    if !helper.is_high_elevated() {
        bail!("injector helper must have a same-user high-integrity token");
    }
    if tray.image_trust == ImageTrustState::Invalid
        || helper.image_trust == ImageTrustState::Invalid
    {
        bail!("invalid Authenticode state is never accepted");
    }
    Ok(())
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn process_image_path(process: HANDLE) -> Result<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let mut len = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut len) } == 0 {
        return Err(std::io::Error::last_os_error()).context("query process image path");
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..len as usize],
    )))
}

fn token_buffer(token: HANDLE, class: i32) -> Result<Vec<u8>> {
    let mut len = 0u32;
    unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(std::io::Error::last_os_error()).context("size token information");
    }
    let mut buffer = vec![0u8; len as usize];
    if unsafe { GetTokenInformation(token, class, buffer.as_mut_ptr().cast(), len, &mut len) } == 0
    {
        return Err(std::io::Error::last_os_error()).context("read token information");
    }
    Ok(buffer)
}

fn token_scalar<T: Copy>(token: HANDLE, class: i32) -> Result<T> {
    let buffer = token_buffer(token, class)?;
    if buffer.len() < std::mem::size_of::<T>() {
        bail!("token information was shorter than expected");
    }
    Ok(unsafe { buffer.as_ptr().cast::<T>().read_unaligned() })
}

fn token_user_sid(token: HANDLE) -> Result<String> {
    let buffer = token_buffer(token, TokenUser)?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(std::io::Error::last_os_error()).context("format process user SID");
    }
    let sid_guard = LocalString(sid);
    let mut len = 0usize;
    while unsafe { *sid.add(len) } != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(sid_guard.0, len)
    }))
}

fn token_integrity_rid(token: HANDLE) -> Result<u32> {
    let buffer = token_buffer(token, TokenIntegrityLevel)?;
    let label = unsafe { &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) } as u32;
    if count == 0 {
        bail!("integrity SID did not contain a RID");
    }
    Ok(unsafe { *GetSidSubAuthority(label.Label.Sid, count - 1) })
}

pub fn authenticode_trust(path: &Path) -> ImageTrustState {
    let wide = OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr() as PCWSTR,
        hFile: ptr::null_mut(),
        pgKnownSubject: ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: ptr::null_mut(),
        pSIPClientData: ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_IGNORE,
        hWVTStateData: ptr::null_mut(),
        pwszURLReference: ptr::null_mut(),
        dwProvFlags: 0,
        dwUIContext: 0,
        pSignatureSettings: ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe { WinVerifyTrustEx(ptr::null_mut(), &mut action, &mut data) };
    if status == 0 {
        ImageTrustState::Valid
    } else if status == TRUST_E_NOSIGNATURE {
        ImageTrustState::UnsignedDogfood
    } else {
        ImageTrustState::Invalid
    }
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct LocalString(*mut u16);
impl Drop for LocalString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(self.0.cast());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_classification_rejects_medium_helper_and_system() {
        let identity = |rid, elevated, sid: &str| WindowsProcessIdentity {
            process_id: 1,
            user_sid: sid.to_string(),
            session_id: 1,
            integrity_rid: rid,
            elevated,
            image_path: PathBuf::from("unused"),
            image_trust: ImageTrustState::UnsignedDogfood,
        };
        assert!(identity(8192, false, "S-1-5-21-1").is_medium_unelevated());
        assert!(!identity(4096, false, "S-1-5-21-1").is_medium_unelevated());
        assert!(!identity(8192, false, "S-1-5-21-1").is_high_elevated());
        assert!(identity(12288, true, "S-1-5-21-1").is_high_elevated());
        assert!(!identity(8192, true, "S-1-5-21-1").is_high_elevated());
        assert!(!identity(16384, true, "S-1-5-18").is_high_elevated());
    }

    #[test]
    fn injector_identity_requires_same_user_session_and_split_token_levels() {
        let identity = |pid, sid: &str, session, rid, elevated, trust| WindowsProcessIdentity {
            process_id: pid,
            user_sid: sid.to_string(),
            session_id: session,
            integrity_rid: rid,
            elevated,
            image_path: PathBuf::from("unused"),
            image_trust: trust,
        };
        let tray = identity(
            42,
            "S-1-5-21-1",
            3,
            8192,
            false,
            ImageTrustState::UnsignedDogfood,
        );
        let helper = identity(
            84,
            "S-1-5-21-1",
            3,
            12288,
            true,
            ImageTrustState::UnsignedDogfood,
        );
        validate_injector_identity_fields(&tray, &helper, 42).expect("valid split-token pair");

        let mut rejected = helper.clone();
        rejected.user_sid = "S-1-5-21-2".to_string();
        assert!(validate_injector_identity_fields(&tray, &rejected, 42).is_err());
        rejected = helper.clone();
        rejected.session_id = 4;
        assert!(validate_injector_identity_fields(&tray, &rejected, 42).is_err());
        rejected = helper.clone();
        rejected.integrity_rid = 8192;
        assert!(validate_injector_identity_fields(&tray, &rejected, 42).is_err());
        rejected = helper.clone();
        rejected.image_trust = ImageTrustState::Invalid;
        assert!(validate_injector_identity_fields(&tray, &rejected, 42).is_err());
        assert!(validate_injector_identity_fields(&tray, &helper, 41).is_err());
    }
}
