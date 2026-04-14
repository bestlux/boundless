#[cfg(windows)]
use std::{
    io,
    pin::Pin,
    sync::mpsc::{self as std_mpsc, SyncSender},
    task::{Context as TaskContext, Poll},
    thread::{self, JoinHandle},
};

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;

#[cfg(windows)]
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    sync::mpsc,
};
#[cfg(windows)]
use tonic::{codegen::tokio_stream::Stream, transport::server::Connected};
#[cfg(windows)]
use windows_sys::Win32::System::{
    Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, GetSystemPowerStatus,
        SYSTEM_POWER_STATUS, SetThreadExecutionState,
    },
    Shutdown::LockWorkStation,
};

#[cfg(windows)]
#[derive(Debug)]
pub struct NamedPipeIncoming {
    receiver: mpsc::Receiver<io::Result<NamedPipeIo>>,
}

#[cfg(windows)]
impl Stream for NamedPipeIncoming {
    type Item = io::Result<NamedPipeIo>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_recv(cx)
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct NamedPipeIo {
    inner: NamedPipeServer,
}

#[cfg(windows)]
impl Connected for NamedPipeIo {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

#[cfg(windows)]
impl AsyncRead for NamedPipeIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(windows)]
impl AsyncWrite for NamedPipeIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(windows)]
pub fn named_pipe_incoming(pipe_name: &str) -> io::Result<NamedPipeIncoming> {
    let pipe_path = pipe_path_for_name(pipe_name)?;
    let (sender, receiver) = mpsc::channel(32);
    let first_server = create_server(&pipe_path, true)?;

    tokio::spawn(async move {
        accept_loop(pipe_path, first_server, sender).await;
    });

    Ok(NamedPipeIncoming { receiver })
}

#[cfg(windows)]
pub fn lock_workstation() -> Result<()> {
    let ok = unsafe { LockWorkStation() };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("LockWorkStation");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn lock_workstation() -> Result<()> {
    anyhow::bail!("lock_machine is only supported on Windows");
}

#[cfg(windows)]
#[derive(Debug)]
pub struct AntiIdlePowerWorker {
    sender: SyncSender<PowerWorkerCommand>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(windows)]
#[derive(Debug)]
enum PowerWorkerCommand {
    Apply(u32),
    Shutdown,
}

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct AntiIdlePowerWorker;

#[cfg(windows)]
impl AntiIdlePowerWorker {
    pub fn apply(&mut self, flags: u32) -> Result<()> {
        self.sender
            .send(PowerWorkerCommand::Apply(flags))
            .context("send anti-idle worker command")
    }
}

#[cfg(not(windows))]
impl AntiIdlePowerWorker {
    pub fn apply(&mut self, _flags: u32) -> Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for AntiIdlePowerWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(PowerWorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
pub fn anti_idle_power_supported() -> bool {
    true
}

#[cfg(not(windows))]
pub fn anti_idle_power_supported() -> bool {
    false
}

#[cfg(windows)]
pub fn anti_idle_execution_state_flags(active: bool, display_required: bool) -> u32 {
    if !active {
        return ES_CONTINUOUS;
    }

    let mut flags = ES_CONTINUOUS | ES_SYSTEM_REQUIRED;
    if display_required {
        flags |= ES_DISPLAY_REQUIRED;
    }
    flags
}

#[cfg(not(windows))]
pub fn anti_idle_execution_state_flags(_active: bool, _display_required: bool) -> u32 {
    0
}

#[cfg(windows)]
pub fn anti_idle_system_on_ac_power() -> Result<bool> {
    let mut status = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut status as *mut SYSTEM_POWER_STATUS) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("GetSystemPowerStatus");
    }

    Ok(status.ACLineStatus == 1)
}

#[cfg(not(windows))]
pub fn anti_idle_system_on_ac_power() -> Result<bool> {
    Ok(true)
}

#[cfg(windows)]
pub fn spawn_anti_idle_power_worker() -> Result<AntiIdlePowerWorker> {
    let (sender, receiver) = std_mpsc::sync_channel::<PowerWorkerCommand>(8);
    let thread = thread::spawn(move || {
        let mut last_flags = u32::MAX;
        while let Ok(command) = receiver.recv() {
            match command {
                PowerWorkerCommand::Apply(flags) => {
                    if flags == last_flags {
                        continue;
                    }
                    let result = unsafe { SetThreadExecutionState(flags) };
                    if result == 0 {
                        tracing::warn!(
                            flags,
                            error = %std::io::Error::last_os_error(),
                            "SetThreadExecutionState failed"
                        );
                        continue;
                    }
                    last_flags = flags;
                }
                PowerWorkerCommand::Shutdown => {
                    let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                    break;
                }
            }
        }
    });

    Ok(AntiIdlePowerWorker {
        sender,
        thread: Some(thread),
    })
}

#[cfg(not(windows))]
pub fn spawn_anti_idle_power_worker() -> Result<AntiIdlePowerWorker> {
    Ok(AntiIdlePowerWorker)
}

pub fn validate_pipe_name(pipe_name: &str) -> Result<()> {
    let trimmed = pipe_name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("pipe name must not be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        anyhow::bail!("pipe name must not contain path separators");
    }
    Ok(())
}

#[cfg(windows)]
async fn accept_loop(
    pipe_path: String,
    mut server: NamedPipeServer,
    sender: mpsc::Sender<io::Result<NamedPipeIo>>,
) {
    loop {
        if let Err(error) = server.connect().await {
            let _ = sender.send(Err(error)).await;
            break;
        }

        let next_server = match create_server(&pipe_path, false) {
            Ok(next) => next,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                break;
            }
        };

        let io = NamedPipeIo { inner: server };
        if sender.send(Ok(io)).await.is_err() {
            break;
        }

        server = next_server;
    }
}

#[cfg(windows)]
fn create_server(pipe_path: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    if first_instance {
        options.first_pipe_instance(true);
    }
    options.create(pipe_path)
}

#[cfg(windows)]
fn pipe_path_for_name(pipe_name: &str) -> io::Result<String> {
    validate_pipe_name(pipe_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let trimmed = pipe_name.trim();

    Ok(format!(r"\\.\pipe\{trimmed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anti_idle_flags_clear_to_continuous_when_inactive() {
        #[cfg(windows)]
        assert_eq!(anti_idle_execution_state_flags(false, false), ES_CONTINUOUS);

        #[cfg(not(windows))]
        assert_eq!(anti_idle_execution_state_flags(false, false), 0);
    }

    #[test]
    fn anti_idle_flags_include_display_when_requested() {
        #[cfg(windows)]
        {
            let flags = anti_idle_execution_state_flags(true, true);
            assert_eq!(flags & ES_SYSTEM_REQUIRED, ES_SYSTEM_REQUIRED);
            assert_eq!(flags & ES_DISPLAY_REQUIRED, ES_DISPLAY_REQUIRED);
            assert_eq!(flags & ES_CONTINUOUS, ES_CONTINUOUS);
        }

        #[cfg(not(windows))]
        assert_eq!(anti_idle_execution_state_flags(true, true), 0);
    }
}
