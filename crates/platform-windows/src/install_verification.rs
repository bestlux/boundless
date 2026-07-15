use anyhow::{Context, Result};
use std::path::PathBuf;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HWND, INVALID_HANDLE_VALUE, LPARAM,
    },
    System::{
        ApplicationInstallationAndServicing::{
            INSTALLPROPERTY_INSTALLLOCATION, INSTALLPROPERTY_VERSIONSTRING, INSTALLSTATE_DEFAULT,
            MsiEnumRelatedProductsW, MsiGetProductInfoW, MsiQueryProductStateW,
        },
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL,
    },
};

const BOUNDLESS_UPGRADE_CODE: &str = "{5A3406B6-73F7-4137-BF4D-4D4E4B75FC37}";

#[derive(Debug, Clone, Default)]
pub struct WindowsInstallSnapshot {
    pub product_codes: Vec<String>,
    pub display_version: String,
    pub install_root: PathBuf,
    pub tray_count: usize,
    pub tray_path_matches: bool,
    pub tray_responding: bool,
}

pub fn collect_windows_install_snapshot() -> Result<WindowsInstallSnapshot> {
    let product_codes = related_product_codes()?;
    let display_version = product_codes
        .first()
        .and_then(|code| product_property(code, INSTALLPROPERTY_VERSIONSTRING).ok())
        .unwrap_or_default();
    let install_root = product_codes
        .first()
        .and_then(|code| product_property(code, INSTALLPROPERTY_INSTALLLOCATION).ok())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_install_root);
    let expected_tray = install_root.join("boundlesstray.exe");
    let (tray_count, tray_path_matches, tray_responding) = tray_snapshot(&expected_tray)?;
    Ok(WindowsInstallSnapshot {
        product_codes,
        display_version,
        install_root,
        tray_count,
        tray_path_matches,
        tray_responding,
    })
}

fn related_product_codes() -> Result<Vec<String>> {
    let upgrade_code = wide_null(BOUNDLESS_UPGRADE_CODE);
    let mut products = Vec::new();
    for index in 0_u32.. {
        let mut buffer = [0_u16; 39];
        let result = unsafe {
            MsiEnumRelatedProductsW(upgrade_code.as_ptr(), 0, index, buffer.as_mut_ptr())
        };
        if result == ERROR_NO_MORE_ITEMS {
            break;
        }
        if result != 0 {
            anyhow::bail!("MsiEnumRelatedProductsW failed with code {result}");
        }
        let product_code = from_wide_null(&buffer);
        let product_code_wide = wide_null(&product_code);
        if unsafe { MsiQueryProductStateW(product_code_wide.as_ptr()) } == INSTALLSTATE_DEFAULT {
            products.push(product_code);
        }
    }
    Ok(products)
}

fn product_property(product_code: &str, property: windows_sys::core::PCWSTR) -> Result<String> {
    let product_code = wide_null(product_code);
    let mut buffer = vec![0_u16; 512];
    let mut length = buffer.len() as u32;
    let mut result = unsafe {
        MsiGetProductInfoW(
            product_code.as_ptr(),
            property,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if result == ERROR_MORE_DATA {
        buffer.resize(length as usize + 1, 0);
        length = buffer.len() as u32;
        result = unsafe {
            MsiGetProductInfoW(
                product_code.as_ptr(),
                property,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
    }
    if result != 0 {
        anyhow::bail!("MsiGetProductInfoW failed with code {result}");
    }
    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn default_install_root() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("Boundless")
}

fn tray_snapshot(expected_path: &std::path::Path) -> Result<(usize, bool, bool)> {
    let session_id = crate::input::current_process_session_id()?;
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).context("snapshot tray processes");
    }
    let snapshot = OwnedHandle(snapshot);
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut matches = Vec::new();
    let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while has_entry {
        let name = from_wide_null(&entry.szExeFile);
        let mut process_session = 0_u32;
        if name.eq_ignore_ascii_case("boundlesstray.exe")
            && unsafe { ProcessIdToSessionId(entry.th32ProcessID, &mut process_session) } != 0
            && process_session == session_id
        {
            matches.push((entry.th32ProcessID, process_path(entry.th32ProcessID)));
        }
        has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    let path_matches = matches.len() == 1
        && matches[0]
            .1
            .as_ref()
            .is_some_and(|path| paths_equal(path, expected_path));
    let responding = matches.len() == 1 && process_has_responsive_window(matches[0].0);
    Ok((matches.len(), path_matches, responding))
}

fn process_path(process_id: u32) -> Option<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let process = OwnedHandle(process);
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    (unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) } != 0)
        .then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])))
}

struct WindowProbe {
    process_id: u32,
    responsive: bool,
}

fn process_has_responsive_window(process_id: u32) -> bool {
    let mut probe = WindowProbe {
        process_id,
        responsive: false,
    };
    unsafe {
        EnumWindows(
            Some(probe_window),
            (&mut probe as *mut WindowProbe) as LPARAM,
        );
    }
    probe.responsive
}

unsafe extern "system" fn probe_window(hwnd: HWND, lparam: LPARAM) -> i32 {
    let probe = unsafe { &mut *(lparam as *mut WindowProbe) };
    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    if process_id != probe.process_id {
        return 1;
    }
    let mut result = 0_usize;
    if unsafe { SendMessageTimeoutW(hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 1_000, &mut result) }
        != 0
    {
        probe.responsive = true;
        return 0;
    }
    1
}

fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(right.to_string_lossy().trim_end_matches(['\\', '/']))
}

struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide_null(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_comparison_is_exact_and_case_insensitive() {
        assert!(paths_equal(
            std::path::Path::new(r"C:\Program Files\Boundless\boundless-service.exe"),
            std::path::Path::new(r"c:\program files\boundless\BOUNDLESS-SERVICE.EXE")
        ));
        assert!(!paths_equal(
            std::path::Path::new(r"C:\Program Files\Boundless\boundless-service.exe.evil"),
            std::path::Path::new(r"C:\Program Files\Boundless\boundless-service.exe")
        ));
    }
}
