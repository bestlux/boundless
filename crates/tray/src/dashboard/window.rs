use super::*;

pub(super) const DASHBOARD_WINDOW_TITLE: &str = "Boundless Dashboard";
const TRAY_EXECUTABLE_NAME: &str = "boundlesstray.exe";

pub(super) fn choose_file_to_send(owner: Option<isize>) -> Result<Option<String>> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut filename = vec![0_u16; 32_768];
    let filter: Vec<u16> = "All files\0*.*\0\0".encode_utf16().collect();
    let title: Vec<u16> = "Choose a file to send\0".encode_utf16().collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner
            .map(|value| value as HWND)
            .unwrap_or(std::ptr::null_mut()),
        lpstrFilter: filter.as_ptr(),
        lpstrFile: filename.as_mut_ptr(),
        nMaxFile: filename.len() as u32,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        if error == 0 {
            return Ok(None);
        }
        bail!("Windows could not open the file picker (0x{error:08x})");
    }
    let length = filename
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(filename.len());
    Ok(Some(
        String::from_utf16(&filename[..length])
            .context("The selected filename is not valid Unicode")?,
    ))
}

struct DashboardWindowSearch {
    session_id: u32,
    user_sid: String,
    hwnd: Option<HWND>,
}

pub(super) fn find_existing_dashboard_window(
    session_id: u32,
    user_sid: &str,
) -> Result<Option<HWND>> {
    let mut search = DashboardWindowSearch {
        session_id,
        user_sid: user_sid.to_string(),
        hwnd: None,
    };
    unsafe {
        EnumWindows(
            Some(find_dashboard_window_callback),
            (&mut search as *mut DashboardWindowSearch) as LPARAM,
        );
    }
    Ok(search.hwnd)
}

unsafe extern "system" fn find_dashboard_window_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam as *mut DashboardWindowSearch) };
    if !window_title_matches(hwnd) {
        return 1;
    }

    let mut process_id = 0_u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process_id);
    }
    if process_id == 0 || !process_matches_tray(process_id, search.session_id, &search.user_sid) {
        return 1;
    }

    search.hwnd = Some(hwnd);
    0
}

fn window_title_matches(hwnd: HWND) -> bool {
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len <= 0 {
        return false;
    }
    let mut title = vec![0_u16; title_len as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
    copied > 0 && String::from_utf16_lossy(&title[..copied as usize]) == DASHBOARD_WINDOW_TITLE
}

fn process_matches_tray(
    process_id: u32,
    expected_session_id: u32,
    expected_user_sid: &str,
) -> bool {
    let mut session_id = 0_u32;
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0
        || session_id != expected_session_id
    {
        return false;
    }
    if !process_id_user_sid_string(process_id).is_ok_and(|user_sid| user_sid == expected_user_sid) {
        return false;
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return false;
    }
    let mut path = vec![0_u16; 32_768];
    let mut path_len = path.len() as u32;
    let queried =
        unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut path_len) };
    unsafe {
        CloseHandle(process);
    }
    if queried == 0 {
        return false;
    }

    std::path::Path::new(&String::from_utf16_lossy(&path[..path_len as usize]))
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(TRAY_EXECUTABLE_NAME))
}

pub(super) fn activate_existing_dashboard_window(hwnd: HWND) -> bool {
    let was_visible = unsafe { IsWindowVisible(hwnd) != 0 };
    let hwnd_value = hwnd as isize;
    let (shown_tx, shown_rx) = mpsc::channel();
    std::thread::spawn(move || {
        unsafe {
            ShowWindow(hwnd_value as HWND, SW_SHOW);
            ShowWindow(hwnd_value as HWND, SW_RESTORE);
        }
        let _ = shown_tx.send(());
    });
    if shown_rx.recv_timeout(Duration::from_secs(1)).is_err() {
        return false;
    }
    let foreground_requested = unsafe { SetForegroundWindow(hwnd) != 0 };

    for _ in 0..20 {
        let visible = unsafe { IsWindowVisible(hwnd) != 0 };
        let foreground = unsafe { GetForegroundWindow() == hwnd };
        if foreground_requested || foreground || (!was_visible && visible) {
            return true;
        }
        if was_visible && visible {
            let flash = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            unsafe {
                FlashWindowEx(&flash);
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

pub(super) fn show_tray_startup_error(message: &str) {
    let message = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let title = "Boundless"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

#[cfg(windows)]
pub(super) fn native_window_handle_from_creation_context(
    cc: &eframe::CreationContext<'_>,
) -> Option<isize> {
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
pub(super) fn native_window_handle_from_creation_context(
    _cc: &eframe::CreationContext<'_>,
) -> Option<isize> {
    None
}

pub(super) fn show_dashboard_window(native_window_handle: Option<isize>, ctx: &egui::Context) {
    #[cfg(windows)]
    if let Some(hwnd) = native_window_handle {
        unsafe {
            ShowWindow(hwnd as HWND, SW_SHOW);
            ShowWindow(hwnd as HWND, SW_RESTORE);
            SetForegroundWindow(hwnd as HWND);
        }
        return;
    }

    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

pub(super) fn hide_dashboard_window(native_window_handle: Option<isize>, ctx: &egui::Context) {
    #[cfg(windows)]
    if let Some(hwnd) = native_window_handle {
        unsafe {
            ShowWindow(hwnd as HWND, SW_HIDE);
        }
        return;
    }

    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
}

pub(super) fn request_dashboard_exit(
    native_window_handle: Option<isize>,
    ctx: &egui::Context,
    exit_requested: &Arc<AtomicBool>,
) {
    exit_requested.store(true, Ordering::SeqCst);

    #[cfg(windows)]
    if let Some(hwnd) = native_window_handle {
        unsafe {
            PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
        }
        return;
    }

    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}
