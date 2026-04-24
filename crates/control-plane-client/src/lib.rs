use anyhow::{Context, Result, bail};
use ipc_api::boundless::v1::control_plane_service_client::ControlPlaneServiceClient;
use tonic::transport::{Channel, Endpoint};

#[cfg(windows)]
use {
    hyper_util::rt::TokioIo,
    std::{
        future::Future,
        io,
        pin::Pin,
        task::{Context as TaskContext, Poll},
        time::Duration,
    },
    tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient},
    tonic::{codegen::Service, transport::Uri},
};

pub fn default_endpoint() -> String {
    if cfg!(windows) {
        "npipe://./pipe/boundlessd-api".to_string()
    } else {
        "http://127.0.0.1:50051".to_string()
    }
}

pub async fn connect_control_plane(endpoint: &str) -> Result<ControlPlaneServiceClient<Channel>> {
    Ok(ControlPlaneServiceClient::new(channel(endpoint).await?))
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
}

#[cfg(windows)]
impl NamedPipeConnector {
    fn new(pipe_path: String) -> Self {
        Self { pipe_path }
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
        Box::pin(async move {
            let client = open_named_pipe_with_retry(pipe_path).await?;
            Ok(TokioIo::new(client))
        })
    }
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
        match ClientOptions::new().open(pipe_path.as_str()) {
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
