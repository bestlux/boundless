use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::{
    anti_idle, clipboard, config::ApiTransport, discovery, hotkeys, input, network, pairing_wire,
    state::AppState,
};

#[derive(Debug, Clone, Default)]
pub struct HostOverrides {
    pub bind: Option<String>,
    pub api_transport: Option<ApiTransport>,
    pub api_pipe_name: Option<String>,
    pub network_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HostRuntimeOptions {
    pub input_runtime_mode: input::InputRuntimeMode,
}

#[derive(Clone)]
pub struct DaemonRuntime {
    pub state: AppState,
    pub snapshot: crate::config::RuntimeConfig,
    pub effective_api_transport: ApiTransport,
}

pub async fn run_with<F, Fut>(overrides: HostOverrides, serve: F) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    run_with_options(overrides, HostRuntimeOptions::default(), serve).await
}

pub async fn run_with_options<F, Fut>(
    overrides: HostOverrides,
    options: HostRuntimeOptions,
    serve: F,
) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let state = AppState::load_or_create().context("load app state")?;
    run_loaded_state_with_options(state, overrides, options, serve).await
}

async fn run_loaded_state_with_options<F, Fut>(
    state: AppState,
    overrides: HostOverrides,
    options: HostRuntimeOptions,
    serve: F,
) -> Result<()>
where
    F: FnOnce(DaemonRuntime) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    apply_overrides(&state, overrides).await?;
    input::apply_startup_mode(&state, options.input_runtime_mode).await;
    let _ = state.reconcile_anti_idle_runtime().await;

    let transport_listener = network::prepare_listener(&state).await;
    let snapshot = state.snapshot().await;

    clipboard::start(state.clone());
    discovery::start(state.clone());
    input::start(state.clone(), options.input_runtime_mode);
    anti_idle::start(state.clone());
    hotkeys::start(state.clone());
    pairing_wire::start(state.clone());
    network::start(state.clone(), transport_listener);

    let configured_api_transport = snapshot.api_transport;
    let effective_api_transport = configured_api_transport.effective();
    if configured_api_transport != effective_api_transport {
        warn!(
            configured = configured_api_transport.as_str(),
            effective = effective_api_transport.as_str(),
            "configured API transport is not supported on this platform; using fallback"
        );
    }

    info!(
        machine_id = %snapshot.machine_id,
        api_bind = %snapshot.api_bind,
        api_transport = effective_api_transport.as_str(),
        api_pipe_name = %snapshot.api_pipe_name,
        network_port = snapshot.network_port,
        protocol = %snapshot.protocol_version,
        "boundless daemon starting"
    );

    let runtime_state = state.clone();
    let result = serve(DaemonRuntime {
        state,
        snapshot,
        effective_api_transport,
    })
    .await;

    runtime_state.begin_transport_session_shutdown();
    runtime_state.shutdown_runtime_tasks().await;
    let aborted_transport_sessions = runtime_state
        .abort_all_transport_sessions_for_shutdown()
        .await;
    if aborted_transport_sessions > 0 {
        info!(
            aborted_transport_sessions,
            "aborted transport sessions during daemon shutdown"
        );
    }
    result
}

pub async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!(%error, "ctrl_c signal error");
    }
    info!("shutdown requested");
}

async fn apply_overrides(state: &AppState, overrides: HostOverrides) -> Result<()> {
    if let Some(bind) = overrides.bind {
        bind.parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid --bind address {bind}"))?;
        state
            .update_bind(bind)
            .await
            .context("update bind address")?;
    }
    if let Some(transport) = overrides.api_transport {
        state
            .update_api_transport(transport)
            .await
            .context("update API transport")?;
    }
    if let Some(pipe_name) = overrides.api_pipe_name {
        state
            .update_api_pipe_name(pipe_name)
            .await
            .context("update API pipe name")?;
    }
    if let Some(port) = overrides.network_port {
        state
            .update_network_port(port)
            .await
            .context("update network port")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn service_mode_named_pipe_startup_preserves_diagnostic_input_state() {
        use platform_windows::runtime::{
            current_user_sid_string, named_pipe_incoming_for_allowed_user,
        };

        let root = std::env::temp_dir().join(format!(
            "boundless-service-startup-test-{}",
            uuid::Uuid::new_v4()
        ));
        let config_path = root.join("config.json");
        let security_root = root.join("security");
        let state =
            AppState::load_or_create_with_paths(config_path, security_root).expect("load state");
        let pipe_name = format!("boundlessd-api-test-{}", uuid::Uuid::new_v4());
        let allowed_user_sid = current_user_sid_string().expect("current user sid");

        let result = run_loaded_state_with_options(
            state,
            HostOverrides {
                bind: None,
                api_transport: Some(ApiTransport::NamedPipe),
                api_pipe_name: Some(pipe_name.clone()),
                network_port: Some(0),
            },
            HostRuntimeOptions {
                input_runtime_mode: input::InputRuntimeMode::ServiceSessionUnsupported,
            },
            |runtime| async move {
                assert_eq!(runtime.effective_api_transport, ApiTransport::NamedPipe);
                assert_eq!(runtime.snapshot.api_pipe_name, pipe_name);

                let _incoming = named_pipe_incoming_for_allowed_user(
                    &runtime.snapshot.api_pipe_name,
                    &allowed_user_sid,
                )
                .expect("secure named-pipe bind should succeed");

                let bundle = runtime.state.control_plane_snapshot_bundle().await;
                assert_eq!(
                    bundle.input_capture_backend_mode,
                    "service_session_unsupported"
                );
                assert!(!bundle.input_locked);
                assert!(!bundle.input_lock_supported);
                assert!(bundle.active_input_capture_target_peer_id.is_none());

                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let _ = std::fs::remove_dir_all(root);
    }
}
