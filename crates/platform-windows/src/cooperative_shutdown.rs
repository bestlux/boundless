use anyhow::{Context, Result, bail};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::watch;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::{
    Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GWLP_USERDATA, GetMessageW, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
        SetWindowLongPtrW, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_ENDSESSION, WM_NCCREATE,
        WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
    },
};

pub const ENDSESSION_CLOSEAPP: isize = 0x0000_0001;
pub const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

struct ExitWatchdog {
    started: AtomicBool,
    completed: Arc<AtomicBool>,
}

impl ExitWatchdog {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            completed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let completed = self.completed.clone();
        thread::Builder::new()
            .name("boundless-exit-deadline".to_string())
            .spawn(move || {
                thread::sleep(GRACEFUL_EXIT_TIMEOUT);
                if !completed.load(Ordering::SeqCst) {
                    std::process::exit(0);
                }
            })
            .ok();
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooperativeCloseReason {
    RestartManager,
    SystemShutdown,
    ApplicationClose,
}

pub fn classify_windows_message(
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<CooperativeCloseReason> {
    match message {
        WM_QUERYENDSESSION if lparam & ENDSESSION_CLOSEAPP != 0 => {
            Some(CooperativeCloseReason::RestartManager)
        }
        WM_QUERYENDSESSION => Some(CooperativeCloseReason::SystemShutdown),
        WM_ENDSESSION if wparam != 0 && lparam & ENDSESSION_CLOSEAPP != 0 => {
            Some(CooperativeCloseReason::RestartManager)
        }
        WM_ENDSESSION if wparam != 0 => Some(CooperativeCloseReason::SystemShutdown),
        _ => None,
    }
}

const TRAY_SHUTDOWN_SUBCLASS_ID: usize = 0x424E_4453;

pub struct TrayShutdownSubclass {
    hwnd: isize,
    state: Arc<TrayShutdownState>,
}

struct TrayShutdownState {
    exit_requested: Arc<AtomicBool>,
    watchdog: ExitWatchdog,
}

impl TrayShutdownSubclass {
    pub fn attach(hwnd: isize, exit_requested: Arc<AtomicBool>) -> Result<Self> {
        if hwnd == 0 {
            bail!("tray shutdown subclass requires a window handle");
        }
        let state = Arc::new(TrayShutdownState {
            exit_requested,
            watchdog: ExitWatchdog::new(),
        });
        let installed = unsafe {
            SetWindowSubclass(
                hwnd as HWND,
                Some(tray_shutdown_subclass_proc),
                TRAY_SHUTDOWN_SUBCLASS_ID,
                Arc::as_ptr(&state) as usize,
            )
        };
        if installed == 0 {
            return Err(std::io::Error::last_os_error())
                .context("install tray cooperative shutdown subclass");
        }
        Ok(Self { hwnd, state })
    }
}

impl Drop for TrayShutdownSubclass {
    fn drop(&mut self) {
        unsafe {
            RemoveWindowSubclass(
                self.hwnd as HWND,
                Some(tray_shutdown_subclass_proc),
                TRAY_SHUTDOWN_SUBCLASS_ID,
            );
        }
        self.state.watchdog.complete();
    }
}

unsafe extern "system" fn tray_shutdown_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    reference_data: usize,
) -> LRESULT {
    if classify_windows_message(message, wparam, lparam).is_some() {
        let state = unsafe { &*(reference_data as *const TrayShutdownState) };
        state.exit_requested.store(true, Ordering::SeqCst);
        state.watchdog.start();
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub struct CooperativeShutdownWindow {
    hwnd: isize,
    join: Option<JoinHandle<()>>,
    watchdog: Arc<ExitWatchdog>,
}

impl CooperativeShutdownWindow {
    pub fn start(shutdown_tx: watch::Sender<bool>) -> Result<Self> {
        let watchdog = Arc::new(ExitWatchdog::new());
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let window_watchdog = watchdog.clone();
        let join = thread::Builder::new()
            .name("boundless-shutdown-window".to_string())
            .spawn(move || shutdown_window_thread(shutdown_tx, window_watchdog, ready_tx))
            .context("start cooperative shutdown window thread")?;
        let hwnd = ready_rx
            .recv()
            .context("cooperative shutdown window thread exited before initialization")??;
        Ok(Self {
            hwnd,
            join: Some(join),
            watchdog,
        })
    }
}

impl Drop for CooperativeShutdownWindow {
    fn drop(&mut self) {
        if self.hwnd != 0 {
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.watchdog.complete();
    }
}

struct ShutdownWindowState {
    shutdown_tx: watch::Sender<bool>,
    watchdog: Arc<ExitWatchdog>,
}

fn shutdown_window_thread(
    shutdown_tx: watch::Sender<bool>,
    watchdog: Arc<ExitWatchdog>,
    ready_tx: mpsc::SyncSender<Result<isize>>,
) {
    let hwnd = match create_shutdown_window(shutdown_tx, watchdog) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    if ready_tx.send(Ok(hwnd as isize)).is_err() {
        unsafe {
            DestroyWindow(hwnd);
        }
        return;
    }

    let mut message = unsafe { std::mem::zeroed::<MSG>() };
    loop {
        let result = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn create_shutdown_window(
    shutdown_tx: watch::Sender<bool>,
    watchdog: Arc<ExitWatchdog>,
) -> Result<HWND> {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        return Err(std::io::Error::last_os_error()).context("resolve injector module handle");
    }
    let class_name = wide_null("Boundless.CooperativeShutdown.v1");
    let window_name = wide_null("Boundless Input Injector");
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(shutdown_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: ptr::null_mut(),
        hCursor: ptr::null_mut(),
        hbrBackground: ptr::null_mut(),
        lpszMenuName: ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: ptr::null_mut(),
    };
    if unsafe { RegisterClassExW(&class) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return Err(std::io::Error::last_os_error())
            .context("register injector cooperative shutdown window class");
    }

    let state = Box::new(ShutdownWindowState {
        shutdown_tx,
        watchdog,
    });
    let state = Box::into_raw(state);
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            state.cast::<c_void>(),
        )
    };
    if hwnd.is_null() {
        unsafe {
            drop(Box::from_raw(state));
        }
        return Err(std::io::Error::last_os_error())
            .context("create injector cooperative shutdown window");
    }
    Ok(hwnd)
}

unsafe extern "system" fn shutdown_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }

    let state = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, GWLP_USERDATA)
            as *mut ShutdownWindowState
    };
    let signal = || {
        if !state.is_null() {
            unsafe {
                (*state).shutdown_tx.send_replace(true);
                (*state).watchdog.start();
            }
        }
    };

    match message {
        WM_QUERYENDSESSION => {
            signal();
            1
        }
        WM_ENDSESSION if wparam != 0 => {
            signal();
            0
        }
        WM_CLOSE => {
            signal();
            unsafe {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            if !state.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(state));
                }
            }
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_restart_manager_and_system_shutdown_messages() {
        assert_eq!(
            classify_windows_message(WM_QUERYENDSESSION, 0, ENDSESSION_CLOSEAPP),
            Some(CooperativeCloseReason::RestartManager)
        );
        assert_eq!(
            classify_windows_message(WM_QUERYENDSESSION, 0, 0),
            Some(CooperativeCloseReason::SystemShutdown)
        );
        assert_eq!(classify_windows_message(WM_ENDSESSION, 0, 0), None);
        assert_eq!(
            classify_windows_message(WM_ENDSESSION, 1, ENDSESSION_CLOSEAPP),
            Some(CooperativeCloseReason::RestartManager)
        );
        assert_eq!(classify_windows_message(WM_CLOSE, 0, 0), None);
    }

    #[test]
    fn hidden_window_accepts_shutdown_and_signals_runtime() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let window = CooperativeShutdownWindow::start(shutdown_tx).expect("start window");
        let accepted = unsafe {
            SendMessageW(
                window.hwnd as HWND,
                WM_QUERYENDSESSION,
                0,
                ENDSESSION_CLOSEAPP,
            )
        };
        assert_eq!(accepted, 1);
        assert!(*shutdown_rx.borrow());
    }

    #[test]
    fn tray_subclass_observes_synchronous_shutdown_messages() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;

        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let watchdog = Arc::new(ExitWatchdog::new());
        let hwnd = create_shutdown_window(shutdown_tx, watchdog.clone())
            .expect("create window on test thread");
        let exit_requested = Arc::new(AtomicBool::new(false));
        let subclass = TrayShutdownSubclass::attach(hwnd as isize, exit_requested.clone())
            .expect("attach subclass");
        let accepted = unsafe { SendMessageW(hwnd, WM_QUERYENDSESSION, 0, ENDSESSION_CLOSEAPP) };
        assert_eq!(accepted, 1);
        assert!(exit_requested.load(Ordering::SeqCst));
        drop(subclass);
        unsafe {
            DestroyWindow(hwnd);
        }
        watchdog.complete();
    }
}
