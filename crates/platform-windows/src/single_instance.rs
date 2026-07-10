use crate::runtime::validate_allowed_user_sid_shape;
use anyhow::{Context, Result, bail};
use std::{
    ffi::c_void,
    mem, ptr,
    sync::Arc,
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, LocalFree, SetLastError,
        WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    System::Threading::{
        CreateEventW, CreateMutexW, INFINITE, ReleaseMutex, SetEvent, WaitForMultipleObjects,
        WaitForSingleObject,
    },
};

const OWNER_RECOVERY_WAIT_MS: u32 = 250;

pub enum SingleInstanceAcquire {
    Primary(SingleInstanceGuard),
    ExistingSignaled,
}

pub struct SingleInstanceGuard {
    owner_mutex: OwnedHandle,
    activation_event: Arc<OwnedHandle>,
    shutdown_event: Arc<OwnedHandle>,
    listener: Option<ActivationListener>,
}

impl SingleInstanceGuard {
    pub fn acquire(name: &str, user_sid: &str) -> Result<SingleInstanceAcquire> {
        if name.is_empty() || !name.starts_with("Local\\") {
            bail!("single-instance event name must use the Local namespace");
        }
        if !validate_allowed_user_sid_shape(user_sid) {
            bail!("single-instance user SID must use canonical numeric SID syntax");
        }

        let security = KernelObjectSecurityDescriptor::for_user(user_sid)?;
        let attributes = security.attributes();

        let mutex_name = wide_null(&format!("{name}.Owner"));
        unsafe {
            SetLastError(0);
        }
        let owner_mutex = unsafe { CreateMutexW(&attributes, 1, mutex_name.as_ptr()) };
        if owner_mutex.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create tray single-instance owner mutex");
        }
        let owner_already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let owner_mutex = OwnedHandle(owner_mutex);

        let event_name = wide_null(&format!("{name}.Activate"));
        let activation_event = unsafe { CreateEventW(&attributes, 0, 0, event_name.as_ptr()) };
        if activation_event.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create tray single-instance activation event");
        }

        let activation_event = Arc::new(OwnedHandle(activation_event));
        let shutdown_event_name = wide_null(&format!("{name}.Shutdown"));
        let shutdown_event =
            unsafe { CreateEventW(&attributes, 0, 0, shutdown_event_name.as_ptr()) };
        if shutdown_event.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create tray shutdown event");
        }
        let shutdown_event = Arc::new(OwnedHandle(shutdown_event));
        if owner_already_exists {
            if unsafe { SetEvent(activation_event.0) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to signal the existing tray instance");
            }

            match unsafe { WaitForSingleObject(owner_mutex.0, OWNER_RECOVERY_WAIT_MS) } {
                WAIT_OBJECT_0 | WAIT_ABANDONED => {}
                WAIT_TIMEOUT => return Ok(SingleInstanceAcquire::ExistingSignaled),
                _ => {
                    return Err(std::io::Error::last_os_error())
                        .context("failed while checking existing tray ownership");
                }
            }
        }

        Ok(SingleInstanceAcquire::Primary(Self {
            owner_mutex,
            activation_event,
            shutdown_event,
            listener: None,
        }))
    }

    /// Signals a currently owned tray instance to follow its normal Quit path.
    ///
    /// `Ok(false)` means there is no owner. Creating the mutex without taking
    /// ownership lets this probe stay race-safe without becoming a temporary
    /// tray owner itself.
    pub fn request_shutdown(name: &str, user_sid: &str) -> Result<bool> {
        if name.is_empty() || !name.starts_with("Local\\") {
            bail!("single-instance event name must use the Local namespace");
        }
        if !validate_allowed_user_sid_shape(user_sid) {
            bail!("single-instance user SID must use canonical numeric SID syntax");
        }

        let security = KernelObjectSecurityDescriptor::for_user(user_sid)?;
        let attributes = security.attributes();
        let mutex_name = wide_null(&format!("{name}.Owner"));
        unsafe {
            SetLastError(0);
        }
        let owner_mutex = unsafe { CreateMutexW(&attributes, 0, mutex_name.as_ptr()) };
        if owner_mutex.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to probe tray single-instance owner mutex");
        }
        let owner_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let _owner_mutex = OwnedHandle(owner_mutex);
        if !owner_exists {
            return Ok(false);
        }

        let shutdown_event_name = wide_null(&format!("{name}.Shutdown"));
        let shutdown_event =
            unsafe { CreateEventW(&attributes, 0, 0, shutdown_event_name.as_ptr()) };
        if shutdown_event.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to open tray shutdown event");
        }
        let shutdown_event = OwnedHandle(shutdown_event);
        if unsafe { SetEvent(shutdown_event.0) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to signal the existing tray to shut down");
        }

        Ok(true)
    }

    pub fn start_listener<F, G>(&mut self, on_activation: F, on_shutdown: G) -> Result<()>
    where
        F: Fn() + Send + 'static,
        G: Fn() + Send + 'static,
    {
        if self.listener.is_some() {
            bail!("single-instance activation listener is already running");
        }

        let stop_event = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if stop_event.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to create tray activation-listener stop event");
        }

        let stop_event = Arc::new(OwnedHandle(stop_event));
        let activation_event = self.activation_event.clone();
        let shutdown_event = self.shutdown_event.clone();
        let listener_stop_event = stop_event.clone();
        let join = thread::Builder::new()
            .name("boundless-tray-activation".to_string())
            .spawn(move || {
                let handles = [activation_event.0, shutdown_event.0, listener_stop_event.0];
                loop {
                    match unsafe {
                        WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE)
                    } {
                        WAIT_OBJECT_0 => on_activation(),
                        result if result == WAIT_OBJECT_0 + 1 => {
                            on_shutdown();
                            break;
                        }
                        result if result == WAIT_OBJECT_0 + 2 => break,
                        _ => break,
                    }
                }
            })
            .context("failed to start tray activation listener")?;

        self.listener = Some(ActivationListener {
            stop_event,
            join: Some(join),
        });
        Ok(())
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.listener.take();
        unsafe {
            ReleaseMutex(self.owner_mutex.0);
        }
    }
}

struct ActivationListener {
    stop_event: Arc<OwnedHandle>,
    join: Option<JoinHandle<()>>,
}

impl Drop for ActivationListener {
    fn drop(&mut self) {
        unsafe {
            SetEvent(self.stop_event.0);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct KernelObjectSecurityDescriptor {
    security_descriptor: PSECURITY_DESCRIPTOR,
}

impl KernelObjectSecurityDescriptor {
    fn for_user(user_sid: &str) -> Result<Self> {
        let sddl = wide_null(&format!(
            "D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})S:(ML;;NW;;;LW)"
        ));
        let mut security_descriptor = ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut security_descriptor,
                ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to build tray single-instance security descriptor");
        }
        Ok(Self {
            security_descriptor,
        })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.security_descriptor.cast::<c_void>(),
            bInheritHandle: 0,
        }
    }
}

impl Drop for KernelObjectSecurityDescriptor {
    fn drop(&mut self) {
        if !self.security_descriptor.is_null() {
            unsafe {
                LocalFree(self.security_descriptor);
            }
        }
    }
}

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::current_user_sid_string;
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };

    static NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn unique_name(label: &str) -> String {
        format!(
            "Local\\Boundless.Test.{label}.{}.{}",
            std::process::id(),
            NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn current_sid() -> String {
        current_user_sid_string().expect("current user SID should resolve")
    }

    #[test]
    fn second_acquire_signals_primary_listener() {
        let name = unique_name("activation");
        let SingleInstanceAcquire::Primary(mut primary) =
            SingleInstanceGuard::acquire(&name, &current_sid())
                .expect("first acquire should succeed")
        else {
            panic!("first acquire must become primary");
        };
        let (tx, rx) = mpsc::channel();
        primary
            .start_listener(
                move || {
                    tx.send(()).expect("activation receiver should stay open");
                },
                || {},
            )
            .expect("listener should start");

        let secondary_name = name.clone();
        let secondary = thread::spawn(move || {
            matches!(
                SingleInstanceGuard::acquire(&secondary_name, &current_sid())
                    .expect("second acquire should succeed"),
                SingleInstanceAcquire::ExistingSignaled
            )
        });
        assert!(secondary.join().expect("secondary thread should finish"));
        rx.recv_timeout(Duration::from_secs(2))
            .expect("primary should receive activation");
    }

    #[test]
    fn shutdown_request_signals_primary_listener() {
        let name = unique_name("shutdown");
        let sid = current_sid();
        let SingleInstanceAcquire::Primary(mut primary) =
            SingleInstanceGuard::acquire(&name, &sid).expect("first acquire should succeed")
        else {
            panic!("first acquire must become primary");
        };
        let (tx, rx) = mpsc::channel();
        primary
            .start_listener(
                || {},
                move || {
                    tx.send(()).expect("shutdown receiver should stay open");
                },
            )
            .expect("listener should start");

        assert!(
            SingleInstanceGuard::request_shutdown(&name, &sid)
                .expect("shutdown request should succeed")
        );
        rx.recv_timeout(Duration::from_secs(2))
            .expect("primary should receive shutdown");
    }

    #[test]
    fn shutdown_request_reports_missing_owner_without_claiming_it() {
        let name = unique_name("shutdown-no-owner");
        let sid = current_sid();
        assert!(
            !SingleInstanceGuard::request_shutdown(&name, &sid)
                .expect("missing-owner shutdown probe should succeed")
        );
        assert!(matches!(
            SingleInstanceGuard::acquire(&name, &sid).expect("acquire should succeed"),
            SingleInstanceAcquire::Primary(_)
        ));
    }

    #[test]
    fn dropping_primary_allows_clean_reacquire() {
        let name = unique_name("reacquire");
        let SingleInstanceAcquire::Primary(primary) =
            SingleInstanceGuard::acquire(&name, &current_sid())
                .expect("first acquire should succeed")
        else {
            panic!("first acquire must become primary");
        };
        drop(primary);

        assert!(matches!(
            SingleInstanceGuard::acquire(&name, &current_sid()).expect("reacquire should succeed"),
            SingleInstanceAcquire::Primary(_)
        ));
    }

    #[test]
    fn rejects_non_local_names() {
        let error = match SingleInstanceGuard::acquire("Global\\Boundless.Test", &current_sid()) {
            Ok(_) => panic!("global name must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("Local namespace"));
    }

    #[test]
    fn rejects_non_numeric_user_sid() {
        let error = match SingleInstanceGuard::acquire(
            &unique_name("invalid-sid"),
            "S-1-5-21);(A;;GA;;;WD",
        ) {
            Ok(_) => panic!("invalid SID must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("canonical numeric SID"));
    }

    #[test]
    fn abandoned_owner_is_promoted_to_primary() {
        let name = unique_name("abandoned");
        let owner_name = name.clone();
        thread::spawn(move || {
            let SingleInstanceAcquire::Primary(primary) =
                SingleInstanceGuard::acquire(&owner_name, &current_sid())
                    .expect("owner acquire should succeed")
            else {
                panic!("owner thread must become primary");
            };
            std::mem::forget(primary);
        })
        .join()
        .expect("owner thread should exit");

        assert!(matches!(
            SingleInstanceGuard::acquire(&name, &current_sid())
                .expect("abandoned owner recovery should succeed"),
            SingleInstanceAcquire::Primary(_)
        ));
    }

    #[test]
    fn concurrent_acquire_selects_exactly_one_primary() {
        const CONTENDERS: usize = 6;
        let name = unique_name("concurrent");
        let start = Arc::new(Barrier::new(CONTENDERS));
        let release = Arc::new(Barrier::new(CONTENDERS + 1));
        let (tx, rx) = mpsc::channel();
        let mut threads = Vec::new();

        for _ in 0..CONTENDERS {
            let name = name.clone();
            let start = start.clone();
            let release = release.clone();
            let tx = tx.clone();
            threads.push(thread::spawn(move || {
                start.wait();
                let acquisition = SingleInstanceGuard::acquire(&name, &current_sid())
                    .expect("contended acquire should succeed");
                tx.send(matches!(&acquisition, SingleInstanceAcquire::Primary(_)))
                    .expect("result receiver should stay open");
                release.wait();
                drop(acquisition);
            }));
        }
        drop(tx);

        let results = rx.iter().take(CONTENDERS).collect::<Vec<_>>();
        release.wait();
        for thread in threads {
            thread.join().expect("contender thread should finish");
        }

        assert_eq!(results.iter().filter(|is_primary| **is_primary).count(), 1);
    }
}
