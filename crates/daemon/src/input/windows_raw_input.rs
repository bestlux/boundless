use super::*;

pub(super) fn spawn_raw_input_thread() -> Result<(u32, JoinHandle<()>)> {
    let (startup_tx, startup_rx) = mpsc::channel::<Result<u32>>();
    let thread = thread::spawn(move || {
        let thread_id = unsafe { GetCurrentThreadId() };
        let hwnd = match create_raw_input_window() {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = startup_tx.send(Err(error));
                return;
            }
        };

        if let Err(error) = register_raw_input_mouse_device(hwnd) {
            let _ = startup_tx.send(Err(error));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return;
        }

        let _ = startup_tx.send(Ok(thread_id));
        unsafe {
            if let Err(error) = run_raw_input_message_loop() {
                warn!(error = ?error, "raw input message loop exited with error");
            }
            let _ = DestroyWindow(hwnd);
        }
    });

    let thread_id = match startup_rx.recv() {
        Ok(Ok(thread_id)) => thread_id,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = thread.join();
            return Err(anyhow::anyhow!("raw input startup channel closed"));
        }
    };

    Ok((thread_id, thread))
}

#[cfg(windows)]
fn create_raw_input_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            STATIC_WINDOW_CLASS_NAME.as_ptr(),
            EMPTY_WINDOW_NAME.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(std::io::Error::last_os_error()).context("CreateWindowExW raw input window");
    }
    Ok(hwnd)
}

#[cfg(windows)]
fn register_raw_input_mouse_device(hwnd: HWND) -> Result<()> {
    let devices = [RAWINPUTDEVICE {
        usUsagePage: RAW_INPUT_USAGE_PAGE_GENERIC,
        usUsage: RAW_INPUT_USAGE_MOUSE,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    }];
    let ok = unsafe {
        RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("RegisterRawInputDevices mouse");
    }
    Ok(())
}

#[cfg(windows)]
unsafe fn run_raw_input_message_loop() -> Result<()> {
    let mut warned_once = false;
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg as *mut MSG, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(std::io::Error::last_os_error())
                .context("GetMessageW raw input message loop");
        }
        if result == 0 {
            break;
        }

        if msg.message == WM_INPUT {
            match process_raw_input_message(msg.lParam) {
                Ok(()) => warned_once = false,
                Err(error) => {
                    if !warned_once {
                        warn!(error = ?error, "raw input message processing failed");
                        warned_once = true;
                    }
                }
            }
            continue;
        }

        unsafe {
            TranslateMessage(&msg as *const MSG);
            DispatchMessageW(&msg as *const MSG);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn process_raw_input_message(lparam: LPARAM) -> Result<()> {
    if !is_hook_lock_active() {
        return Ok(());
    }

    let hrawinput = lparam as *mut core::ffi::c_void;
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut raw_size = 0u32;
    let query_size = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            std::ptr::null_mut(),
            &mut raw_size as *mut u32,
            header_size,
        )
    };
    if query_size == u32::MAX {
        return Err(std::io::Error::last_os_error()).context("GetRawInputData query size");
    }
    if raw_size < header_size {
        return Ok(());
    }

    let mut buffer = vec![0u8; raw_size as usize];
    let read_size = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            buffer.as_mut_ptr().cast(),
            &mut raw_size as *mut u32,
            header_size,
        )
    };
    if read_size == u32::MAX {
        return Err(std::io::Error::last_os_error()).context("GetRawInputData read payload");
    }
    if read_size < header_size {
        return Ok(());
    }

    let raw = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<RAWINPUT>()) };
    if raw.header.dwType != RIM_TYPEMOUSE {
        return Ok(());
    }

    let mouse = unsafe { raw.data.mouse };
    if let Some((dx, dy)) = raw_mouse_relative_delta(&mouse) {
        send_hook_event(HookCaptureEvent::MouseDelta { dx, dy }, "raw_input");
    }

    Ok(())
}

#[cfg(windows)]
pub(super) fn raw_mouse_relative_delta(mouse: &RAWMOUSE) -> Option<(i32, i32)> {
    if (mouse.usFlags & MOUSE_MOVE_ABSOLUTE) != 0 {
        return None;
    }
    if mouse.lLastX == 0 && mouse.lLastY == 0 {
        return None;
    }
    Some((mouse.lLastX, mouse.lLastY))
}
