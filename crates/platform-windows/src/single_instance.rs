use crate::runtime::validate_allowed_user_sid_shape;
use anyhow::{Context, Result, bail};
use std::{
    ffi::c_void,
    mem, ptr,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, LocalFree,
        SetLastError, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    },
    System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, MUTEX_MODIFY_STATE, OpenEventW,
        OpenMutexW, ReleaseMutex, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
    },
};

const OWNER_RECOVERY_WAIT_MS: u32 = 250;
const SHUTDOWN_OPEN_RETRY_COUNT: usize = 50;
const SHUTDOWN_OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

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
    /// Reports whether a named local mutex already exists without acquiring it.
    ///
    /// Upgrade helpers use this to publish a quiescence sentinel. A tray must
    /// fail closed on every error other than a genuinely missing sentinel so a
    /// replacement process cannot join an in-progress MSI transaction.
    pub fn local_mutex_exists(name: &str) -> Result<bool> {
        if name.is_empty() || !name.starts_with("Local\\") {
            bail!("single-instance mutex name must use the Local namespace");
        }

        let mutex_name = wide_null(name);
        let mutex = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, mutex_name.as_ptr()) };
        if mutex.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Ok(false);
            }
            return Err(error).context("failed to open local single-instance mutex");
        }
        let _mutex = OwnedHandle(mutex);
        Ok(true)
    }

    pub fn acquire(name: &str, user_sid: &str) -> Result<SingleInstanceAcquire> {
        if name.is_empty() || !name.starts_with("Local\\") {
            bail!("single-instance event name must use the Local namespace");
        }
        if !validate_allowed_user_sid_shape(user_sid) {
            bail!("single-instance user SID must use canonical numeric SID syntax");
        }

        let activation_security =
            KernelObjectSecurityDescriptor::for_user(user_sid, IntegrityLevel::Low)?;
        let attributes = activation_security.attributes();

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

        // Only the thread that owns the primary mutex may publish Shutdown.
        // A secondary/requester must never create it during the small gap
        // between owner-mutex acquisition and primary initialization.
        let shutdown_security =
            KernelObjectSecurityDescriptor::for_user(user_sid, IntegrityLevel::Medium)?;
        let shutdown_attributes = shutdown_security.attributes();
        let shutdown_event_name = wide_null(&format!("{name}.Shutdown"));
        unsafe {
            SetLastError(0);
        }
        let shutdown_event =
            unsafe { CreateEventW(&shutdown_attributes, 0, 0, shutdown_event_name.as_ptr()) };
        if shutdown_event.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe {
                ReleaseMutex(owner_mutex.0);
            }
            return Err(error).context("failed to create tray shutdown event");
        }
        let shutdown_already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let shutdown_event = Arc::new(OwnedHandle(shutdown_event));
        if shutdown_already_exists {
            unsafe {
                ReleaseMutex(owner_mutex.0);
            }
            bail!("refusing tray ownership because the shutdown event unexpectedly pre-existed");
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
    /// `Ok(false)` means there is no shutdown-capable owner. This path opens
    /// existing kernel objects only, so it cannot pre-create a lower-integrity
    /// shutdown event during primary initialization.
    pub fn request_shutdown(name: &str, user_sid: &str) -> Result<bool> {
        if name.is_empty() || !name.starts_with("Local\\") {
            bail!("single-instance event name must use the Local namespace");
        }
        if !validate_allowed_user_sid_shape(user_sid) {
            bail!("single-instance user SID must use canonical numeric SID syntax");
        }

        let mutex_name = wide_null(&format!("{name}.Owner"));
        let owner_mutex = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, mutex_name.as_ptr()) };
        if owner_mutex.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Ok(false);
            }
            return Err(error).context("failed to open tray single-instance owner mutex");
        }
        let _owner_mutex = OwnedHandle(owner_mutex);

        let shutdown_event_name = wide_null(&format!("{name}.Shutdown"));
        for attempt in 0..=SHUTDOWN_OPEN_RETRY_COUNT {
            let shutdown_event =
                unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, shutdown_event_name.as_ptr()) };
            if !shutdown_event.is_null() {
                let shutdown_event = OwnedHandle(shutdown_event);
                if unsafe { SetEvent(shutdown_event.0) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to signal the existing tray to shut down");
                }
                return Ok(true);
            }

            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32) {
                return Err(error).context("failed to open tray shutdown event");
            }
            if attempt == SHUTDOWN_OPEN_RETRY_COUNT {
                return Ok(false);
            }
            thread::sleep(SHUTDOWN_OPEN_RETRY_DELAY);
        }

        Ok(false)
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

#[derive(Clone, Copy)]
enum IntegrityLevel {
    Low,
    Medium,
}

fn kernel_object_sddl(user_sid: &str, integrity: IntegrityLevel) -> String {
    let integrity_sid = match integrity {
        IntegrityLevel::Low => "LW",
        IntegrityLevel::Medium => "ME",
    };
    format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})S:(ML;;NW;;;{integrity_sid})")
}

impl KernelObjectSecurityDescriptor {
    fn for_user(user_sid: &str, integrity: IntegrityLevel) -> Result<Self> {
        let sddl = wide_null(&kernel_object_sddl(user_sid, integrity));
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

    fn create_owned_mutex(name: &str, sid: &str) -> OwnedHandle {
        let security = KernelObjectSecurityDescriptor::for_user(sid, IntegrityLevel::Low)
            .expect("owner mutex security should build");
        let attributes = security.attributes();
        let owner_name = wide_null(&format!("{name}.Owner"));
        unsafe {
            SetLastError(0);
        }
        let owner = unsafe { CreateMutexW(&attributes, 1, owner_name.as_ptr()) };
        assert!(!owner.is_null(), "owner mutex should be created");
        assert_ne!(unsafe { GetLastError() }, ERROR_ALREADY_EXISTS);
        OwnedHandle(owner)
    }

    fn create_shutdown_event(name: &str, sid: &str, integrity: IntegrityLevel) -> OwnedHandle {
        let security = KernelObjectSecurityDescriptor::for_user(sid, integrity)
            .expect("shutdown event security should build");
        let attributes = security.attributes();
        let shutdown_name = wide_null(&format!("{name}.Shutdown"));
        unsafe {
            SetLastError(0);
        }
        let shutdown = unsafe { CreateEventW(&attributes, 0, 0, shutdown_name.as_ptr()) };
        assert!(!shutdown.is_null(), "shutdown event should be created");
        assert_ne!(unsafe { GetLastError() }, ERROR_ALREADY_EXISTS);
        OwnedHandle(shutdown)
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
    fn shutdown_request_waits_for_primary_to_publish_event_without_creating_it() {
        let name = unique_name("shutdown-open-race");
        let sid = current_sid();
        let owner_name = name.clone();
        let owner_sid = sid.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let publisher = thread::spawn(move || {
            let owner = create_owned_mutex(&owner_name, &owner_sid);
            ready_tx
                .send(())
                .expect("shutdown requester should wait for owner readiness");
            thread::sleep(Duration::from_millis(50));
            let shutdown = create_shutdown_event(&owner_name, &owner_sid, IntegrityLevel::Medium);
            assert_eq!(
                unsafe { WaitForSingleObject(shutdown.0, 2_000) },
                WAIT_OBJECT_0,
                "requester should signal the event published after the owner mutex"
            );
            unsafe {
                ReleaseMutex(owner.0);
            }
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("owner mutex should be published");
        assert!(
            SingleInstanceGuard::request_shutdown(&name, &sid)
                .expect("requester should tolerate the owner initialization gap")
        );
        publisher
            .join()
            .expect("shutdown event publisher should finish");
    }

    #[test]
    fn primary_rejects_a_precreated_low_integrity_shutdown_event() {
        let name = unique_name("shutdown-precreated");
        let sid = current_sid();
        let rogue_shutdown = create_shutdown_event(&name, &sid, IntegrityLevel::Low);

        let error = match SingleInstanceGuard::acquire(&name, &sid) {
            Ok(_) => panic!("primary must reject a pre-created shutdown event"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unexpectedly pre-existed"));

        drop(rogue_shutdown);
        assert!(matches!(
            SingleInstanceGuard::acquire(&name, &sid)
                .expect("acquire should recover after the rogue event closes"),
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
    fn local_mutex_presence_probe_opens_existing_without_acquiring_it() {
        let name = unique_name("presence");
        assert!(
            !SingleInstanceGuard::local_mutex_exists(&name)
                .expect("missing local mutex probe should succeed")
        );

        let sid = current_sid();
        let security = KernelObjectSecurityDescriptor::for_user(&sid, IntegrityLevel::Low)
            .expect("presence mutex security should build");
        let attributes = security.attributes();
        let wide_name = wide_null(&name);
        let mutex = unsafe { CreateMutexW(&attributes, 1, wide_name.as_ptr()) };
        assert!(!mutex.is_null(), "presence mutex should be created");
        let mutex = OwnedHandle(mutex);

        assert!(
            SingleInstanceGuard::local_mutex_exists(&name)
                .expect("existing local mutex probe should succeed")
        );
        drop(mutex);
        assert!(
            !SingleInstanceGuard::local_mutex_exists(&name)
                .expect("closed local mutex probe should succeed")
        );
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
    fn shutdown_signal_requires_medium_integrity_but_activation_remains_low_integrity() {
        let sid = "S-1-5-21-1-2-3-1001";
        let activation = kernel_object_sddl(sid, IntegrityLevel::Low);
        let shutdown = kernel_object_sddl(sid, IntegrityLevel::Medium);

        assert!(activation.ends_with("S:(ML;;NW;;;LW)"));
        assert!(shutdown.ends_with("S:(ML;;NW;;;ME)"));
        assert!(activation.contains(&format!("(A;;GA;;;{sid})")));
        assert!(shutdown.contains(&format!("(A;;GA;;;{sid})")));
    }

    #[test]
    fn abandoned_owner_is_promoted_to_primary() {
        let name = unique_name("abandoned");
        let sid = current_sid();
        let owner_name = name.clone();
        let owner_sid = sid.clone();
        let (owner_tx, owner_rx) = mpsc::channel();
        thread::spawn(move || {
            owner_tx
                .send(create_owned_mutex(&owner_name, &owner_sid))
                .expect("abandoned owner handle should stay open for recovery");
        })
        .join()
        .expect("owner thread should exit and abandon the mutex");
        let abandoned_owner = owner_rx
            .recv()
            .expect("abandoned owner handle should be retained");

        let recovered = SingleInstanceGuard::acquire(&name, &sid)
            .expect("abandoned owner recovery should succeed");
        assert!(matches!(&recovered, SingleInstanceAcquire::Primary(_)));
        drop(recovered);
        drop(abandoned_owner);
    }

    #[test]
    fn abandoned_owner_promotion_rejects_a_preexisting_shutdown_event() {
        let name = unique_name("abandoned-precreated-shutdown");
        let sid = current_sid();
        let owner_name = name.clone();
        let owner_sid = sid.clone();
        let (handles_tx, handles_rx) = mpsc::channel();
        thread::spawn(move || {
            let owner = create_owned_mutex(&owner_name, &owner_sid);
            let shutdown = create_shutdown_event(&owner_name, &owner_sid, IntegrityLevel::Low);
            handles_tx
                .send((owner, shutdown))
                .expect("stale handles should remain open during promotion");
        })
        .join()
        .expect("owner thread should abandon the mutex");
        let stale_handles = handles_rx
            .recv()
            .expect("stale kernel object handles should be retained");

        let error = match SingleInstanceGuard::acquire(&name, &sid) {
            Ok(_) => panic!("stale-owner promotion must reject a pre-existing shutdown event"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unexpectedly pre-existed"));
        drop(stale_handles);
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
