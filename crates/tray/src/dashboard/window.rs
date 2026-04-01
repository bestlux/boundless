use super::*;

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
