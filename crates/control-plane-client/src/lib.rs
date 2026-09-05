use anyhow::{Context, Result, bail};
use ipc_api::CONTROL_PLANE_MAX_MESSAGE_BYTES;
use ipc_api::boundless::v1::control_plane_service_client::ControlPlaneServiceClient;
use tonic::transport::{Channel, Endpoint};

#[cfg(windows)]
use {
    hyper_util::rt::TokioIo,
    std::{
        future::Future,
        io,
        os::windows::{
            fs::OpenOptionsExt,
            io::{AsRawHandle, IntoRawHandle},
        },
        pin::Pin,
        task::{Context as TaskContext, Poll},
        time::Duration,
    },
    tokio::net::windows::named_pipe::NamedPipeClient,
    tonic::{codegen::Service, transport::Uri},
    windows_sys::Win32::{Foundation::HANDLE, System::Pipes::GetNamedPipeServerProcessId},
};

pub fn default_endpoint() -> String {
    if cfg!(windows) {
        "npipe://./pipe/boundlessd-api".to_string()
    } else {
        "http://127.0.0.1:50051".to_string()
    }
}

pub async fn connect_control_plane(endpoint: &str) -> Result<ControlPlaneServiceClient<Channel>> {
    Ok(ControlPlaneServiceClient::new(channel(endpoint).await?)
        .max_decoding_message_size(CONTROL_PLANE_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(CONTROL_PLANE_MAX_MESSAGE_BYTES))
}

pub async fn channel(endpoint: &str) -> Result<Channel> {
    if let Some(pipe_path) = parse_npipe_endpoint(endpoint)? {
        return connect_named_pipe(endpoint, pipe_path).await;
    }

    Endpoint::from_shared(endpoint.to_string())
        .with_context(|| format!("invalid endpoint {endpoint}"))?
        .connect()
        .await
        .with_context(|| format!("failed to connect to {endpoint}"))
}

/// Opens a local named-pipe channel only when Windows reports the expected
/// server process id on the connected pipe handle. This prevents a lower-
/// integrity pipe squatter from impersonating a just-launched injector.
#[cfg(windows)]
pub async fn channel_to_named_pipe_server(
    endpoint: &str,
    expected_server_process_id: u32,
) -> Result<Channel> {
    let pipe_path = parse_npipe_endpoint(endpoint)?
        .context("expected a named-pipe endpoint for process-bound channel")?;
    Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(NamedPipeConnector::with_expected_server(
            pipe_path,
            expected_server_process_id,
        ))
        .await
        .with_context(|| {
            format!(
                "failed to connect to named pipe endpoint {endpoint} owned by process {expected_server_process_id}"
            )
        })
}

#[cfg(not(windows))]
pub async fn channel_to_named_pipe_server(
    endpoint: &str,
    expected_server_process_id: u32,
) -> Result<Channel> {
    let _ = expected_server_process_id;
    bail!("process-bound named-pipe endpoint is only supported on Windows: {endpoint}")
}

pub fn parse_npipe_endpoint(endpoint: &str) -> Result<Option<String>> {
    let Some(rest) = endpoint.strip_prefix("npipe://") else {
        return Ok(None);
    };
    if let Some(name) = rest.strip_prefix("./pipe/") {
        return pipe_path_from_name(name).map(Some);
    }
    if let Some(name) = rest.strip_prefix(r"\\.\pipe\") {
        return pipe_path_from_name(name).map(Some);
    }

    bail!("invalid named-pipe endpoint {endpoint}; expected npipe://./pipe/<name>")
}

pub fn is_named_pipe_endpoint(endpoint: &str) -> bool {
    endpoint.trim().starts_with("npipe://")
}

pub fn has_access_denied_io_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.raw_os_error() == Some(5))
    })
}

fn pipe_path_from_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("named-pipe endpoint is missing pipe name");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        bail!("named-pipe endpoint pipe name must not contain path separators");
    }

    Ok(format!(r"\\.\pipe\{trimmed}"))
}

#[cfg(windows)]
async fn connect_named_pipe(endpoint: &str, pipe_path: String) -> Result<Channel> {
    Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(NamedPipeConnector::new(pipe_path))
        .await
        .with_context(|| format!("failed to connect to named pipe endpoint {endpoint}"))
}

#[cfg(not(windows))]
async fn connect_named_pipe(endpoint: &str, pipe_path: String) -> Result<Channel> {
    let _ = pipe_path;
    bail!("named-pipe endpoint is only supported on Windows: {endpoint}");
}

#[cfg(windows)]
#[derive(Clone)]
struct NamedPipeConnector {
    pipe_path: String,
    expected_server_process_id: Option<u32>,
}

#[cfg(windows)]
impl NamedPipeConnector {
    fn new(pipe_path: String) -> Self {
        Self {
            pipe_path,
            expected_server_process_id: None,
        }
    }

    fn with_expected_server(pipe_path: String, process_id: u32) -> Self {
        Self {
            pipe_path,
            expected_server_process_id: Some(process_id),
        }
    }
}

#[cfg(windows)]
impl Service<Uri> for NamedPipeConnector {
    type Response = TokioIo<NamedPipeClient>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        let pipe_path = self.pipe_path.clone();
        let expected_server_process_id = self.expected_server_process_id;
        Box::pin(async move {
            let client = open_named_pipe_with_retry(pipe_path).await?;
            if let Some(expected) = expected_server_process_id {
                verify_named_pipe_server_process(&client, expected)?;
            }
            Ok(TokioIo::new(client))
        })
    }
}

#[cfg(windows)]
fn verify_named_pipe_server_process(
    client: &NamedPipeClient,
    expected_server_process_id: u32,
) -> io::Result<()> {
    let mut actual = 0u32;
    let handle = client.as_raw_handle() as HANDLE;
    if unsafe { GetNamedPipeServerProcessId(handle, &mut actual) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if actual != expected_server_process_id {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "named-pipe server process mismatch: actual={actual} expected={expected_server_process_id}"
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
const ERROR_PIPE_BUSY_CODE: i32 = 231;
#[cfg(windows)]
const PIPE_BUSY_MAX_RETRIES: u32 = 20;
#[cfg(windows)]
const PIPE_BUSY_BACKOFF_MS: u64 = 25;

#[cfg(windows)]
async fn open_named_pipe_with_retry(pipe_path: String) -> io::Result<NamedPipeClient> {
    let mut attempt = 0_u32;

    loop {
        match open_client_pipe(&pipe_path) {
            Ok(client) => return Ok(client),
            Err(error) if is_pipe_busy_error(&error) && attempt < PIPE_BUSY_MAX_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(PIPE_BUSY_BACKOFF_MS)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn open_client_pipe(pipe_path: &str) -> io::Result<NamedPipeClient> {
    // Individual client rights exclude FILE_CREATE_PIPE_INSTANCE (aliased to
    // FILE_APPEND_DATA). GENERIC_WRITE would inadvertently request that right.
    const CLIENT_ACCESS: u32 = 0x0012_019b;
    const OVERLAPPED_IDENTIFICATION: u32 = 0x4000_0000 | 0x0010_0000 | 0x0001_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(CLIENT_ACCESS)
        .custom_flags(OVERLAPPED_IDENTIFICATION)
        .open(pipe_path)?;
    // The open uses FILE_FLAG_OVERLAPPED as required by Tokio. Identification
    // QoS prevents an untrusted server from impersonating the connecting user.
    unsafe { NamedPipeClient::from_raw_handle(file.into_raw_handle()) }
}

#[cfg(windows)]
fn is_pipe_busy_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PIPE_BUSY_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_npipe_endpoint_accepts_pipe_name() {
        let path = parse_npipe_endpoint("npipe://./pipe/boundlessd-api")
            .expect("parse")
            .expect("npipe");
        assert_eq!(path, r"\\.\pipe\boundlessd-api");
    }

    #[test]
    fn parse_npipe_endpoint_rejects_invalid_shape() {
        let err = parse_npipe_endpoint("npipe://boundlessd-api").expect_err("must fail");
        assert!(err.to_string().contains("expected npipe://./pipe/<name>"));
    }

    #[test]
    fn parse_npipe_endpoint_ignores_http_endpoint() {
        let parsed = parse_npipe_endpoint("http://127.0.0.1:50051").expect("parse");
        assert!(parsed.is_none());
    }

    #[test]
    fn is_named_pipe_endpoint_detects_scheme_only() {
        assert!(is_named_pipe_endpoint(" npipe://./pipe/boundlessd-api"));
        assert!(!is_named_pipe_endpoint("http://127.0.0.1:50051"));
    }

    #[test]
    fn has_access_denied_io_error_matches_raw_os_code() {
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(5));
        assert!(has_access_denied_io_error(&error));

        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(2));
        assert!(!has_access_denied_io_error(&error));
    }

    #[test]
    fn default_endpoint_uses_named_pipe_on_windows_only() {
        if cfg!(windows) {
            assert_eq!(default_endpoint(), "npipe://./pipe/boundlessd-api");
        } else {
            assert_eq!(default_endpoint(), "http://127.0.0.1:50051");
        }
    }
}
